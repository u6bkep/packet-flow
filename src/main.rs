use chrono::prelude::{DateTime, Utc};
use clap::Parser;
use influxdb::{Client, InfluxDbWriteable, WriteQuery};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    net::UdpSocket,
    signal,
    sync::mpsc::{self, Receiver, Sender, error as mpsc_error},
    time::Duration as TokioDuration,
};

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Mode to run in: "sender" or "receiver"
    #[clap(short, long)]
    mode: String,

    #[clap(short, long, default_value = "config.toml")]
    config_file: String,

    /// Local address to bind to
    #[clap(long)]
    local_addr: Option<String>,

    /// Remote address to connect to
    #[clap(long)]
    remote_addr: Option<String>,

    /// Packet sending rate in milliseconds
    #[clap(short, long)]
    rate: Option<u64>,

    /// InfluxDB URL
    #[clap(short, long)]
    influx_url: Option<String>,

    /// InfluxDB database name
    #[clap(short, long)]
    database: Option<String>,

    /// InfluxDB token
    #[clap(short, long)]
    token: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Config {
    local_addr: String,
    remote_addr: String,
    rate: u64,
    influx_url: String,
    database: String,
    token: Option<String>,
}

#[derive(InfluxDbWriteable, Clone)]
struct PacketLoss {
    time: DateTime<Utc>,
    source: String,
    destination: String,
    value: u64,
}

#[derive(InfluxDbWriteable, Clone)]
struct Latency {
    time: DateTime<Utc>,
    source: String,
    destination: String,
    value: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct Packet {
    sequence: u64,
    timestamp_us: u64,
    source: String,
}

#[derive(Clone)]
enum Measurement {
    PacketLoss(PacketLoss),
    Latency(Latency),
}

struct InfluxReporter {
    client: Client,
    rx: Receiver<Measurement>,
}

impl InfluxReporter {
    async fn run(&mut self) {
        println!(
            "Reporting measurements to InfluxDB at {}...",
            self.client.database_url()
        );
        let mut messurements: Vec<Measurement> = Vec::new();
        loop {
            self.rx.recv_many(&mut messurements, 100).await;
            self.flush_batch(&mut messurements).await;
        }
    }

    async fn flush_batch(&self, batch: &mut Vec<Measurement>) {
        let mut queries = Vec::new();

        for measurement in batch.drain(..) {
            match measurement {
                Measurement::PacketLoss(pl) => {
                    queries.push(WriteQuery::from(pl.into_query("packet_loss")))
                }
                Measurement::Latency(l) => queries.push(WriteQuery::from(l.into_query("latency"))),
            }
        }

        self.client.query(&queries).await.unwrap();
    }
}

struct UdpSender {
    socket: UdpSocket,
    rate: TokioDuration,
    local_addr: String,
}

impl UdpSender {
    async fn run(&self) {
        println!(
            "Sending packets from {} to {}...",
            self.socket.local_addr().unwrap(),
            self.socket.peer_addr().unwrap()
        );
        let mut sequence = 0u64;
        let mut interval = tokio::time::interval(self.rate);

        //check if program is running interactively
        let is_tty = atty::is(atty::Stream::Stdin);

        // spawn a thread to listen for keyboard input.
        let (keyboard_notify_channel_tx, mut keyboard_notify_channel_rx) = mpsc::channel(1);
        if is_tty {
            tokio::spawn(async move {
                loop {
                    // use system stio to blocking read from stdin
                    let mut read_buf = String::new();
                    std::io::stdin().read_line(&mut read_buf).unwrap();
                    keyboard_notify_channel_tx
                        .send(read_buf.chars().nth(0))
                        .await
                        .unwrap();
                    read_buf.clear();
                }
            });
        } else {
            drop(keyboard_notify_channel_tx);
        }

        loop {
            interval.tick().await;
            let packet = Packet {
                sequence,
                timestamp_us: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as u64,
                source: self.local_addr.clone(),
            };

            if let Ok(data) = bincode::serialize(&packet) {
                self.socket.send(&data).await.unwrap();
            }

            sequence += 1;

            if is_tty {
                // check if space bar has been pressed, then skip a sequence number
                match keyboard_notify_channel_rx.try_recv() {
                    Ok(x) => {
                        match x {
                            Some(' ') => {
                                println!("space bar pressed, skipping a sequence number");
                                sequence += 1;
                            }
                            Some('1') => {
                                println!("1 pressed, skipping 1 sequence number");
                                sequence += 1;
                            }
                            Some('2') => {
                                println!("2 pressed, skipping 2 sequence number");
                                sequence += 2;
                            }
                            Some('3') => {
                                println!("3 pressed, skipping 3 sequence number");
                                sequence += 3;
                            }
                            _ => {}
                        }
                        sequence += 1;
                    }
                    Err(mpsc_error::TryRecvError::Empty) => {}
                    Err(mpsc_error::TryRecvError::Disconnected) => {
                        println!("keyboard_notify_channel_rx disconnected");
                        break;
                    }
                }
            }
        }
    }
}

struct UdpReceiver {
    socket: UdpSocket,
    measurement_tx: Sender<Measurement>,
    local_addr: String,
    remote_addr: String,
}

impl UdpReceiver {
    async fn run(&self) {
        let mut buffer = [0u8; 1024];
        let mut last_sequence = 0;

        println!("Listening on {}...", self.local_addr);

        loop {
            match tokio::time::timeout(TokioDuration::from_secs(10), self.socket.recv(&mut buffer))
                .await
            {
                Ok(Ok(size)) => {
                    if let Ok(packet) = bincode::deserialize::<Packet>(&buffer[..size]) {
                        self.process_packet(packet, &mut last_sequence).await;
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("Socket error: {}", e);
                    break;
                }
                Err(_) => {
                    //timeout, report packet loss
                    println!("no packet received. Reporting packet loss");
                    let loss = PacketLoss {
                        time: Utc::now(),
                        source: self.local_addr.clone(),
                        destination: self.remote_addr.clone(),
                        value: 1,
                    };
                    self.measurement_tx
                        .send(Measurement::PacketLoss(loss))
                        .await
                        .unwrap();
                }
            }
        }
    }

    async fn process_packet(&self, packet: Packet, last_sequence: &mut u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        let latency = now - packet.timestamp_us;

        if packet.sequence > last_sequence.wrapping_add(1) {
            println!(
                "Packet loss detected from {}: expected {}, got {}",
                packet.source,
                last_sequence.wrapping_add(1),
                packet.sequence
            );
            let lost_packets = packet.sequence.wrapping_sub(last_sequence.wrapping_add(1));
            let loss = PacketLoss {
                time: Utc::now(),
                source: packet.source.clone(),
                destination: self.local_addr.clone(),
                value: lost_packets,
            };
            self.measurement_tx
                .send(Measurement::PacketLoss(loss))
                .await
                .unwrap();
        }

        let latency = Latency {
            time: Utc::now(),
            source: packet.source.clone(),
            destination: self.local_addr.clone(),
            value: latency,
        };
        self.measurement_tx
            .send(Measurement::Latency(latency))
            .await
            .unwrap();

        *last_sequence = packet.sequence;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let default_config: Config = Config {
        local_addr: "0.0.0.0:8000".to_string(),
        remote_addr: "127.0.0.1:8000".to_string(),
        rate: 1000,
        influx_url: "http://localhost:8086".to_string(),
        database: "network_monitor".to_string(),
        token: None,
    };
    let args = Args::parse();

    // handle configuration. options not supplied in the command line will be read from the config file. options not supplied in the config file will be set to the default value.
    let file = std::fs::read_to_string(&args.config_file);
    let config: Config = match file {
        Ok(contents) => {
            println!("Reading configuration from file: {}", args.config_file);
            toml::from_str(&contents).unwrap()
        }
        Err(e) => {
            println!(
                "Error reading config file: {}. Using default configuration.",
                e
            );
            let mut config = default_config;
            if args.local_addr.is_some() {
                config.local_addr = args.local_addr.clone().unwrap();
            }
            if args.remote_addr.is_some() {
                config.remote_addr = args.remote_addr.clone().unwrap();
            }
            if args.rate.is_some() {
                config.rate = args.rate.unwrap();
            }
            if args.influx_url.is_some() {
                config.influx_url = args.influx_url.clone().unwrap();
            }
            if args.database.is_some() {
                config.database = args.database.clone().unwrap();
            }
            if args.token.is_some() {
                config.token = args.token.clone();
            }
            config
        }
    };

    match args.mode.as_str() {
        "sender" => {
            let socket = UdpSocket::bind("0.0.0.0:0").await?;
            socket.connect(&config.remote_addr).await?;

            println!("in sender mode");

            let sender = UdpSender {
                socket,
                rate: TokioDuration::from_millis(config.rate),
                local_addr: config.local_addr.clone(),
            };

            let sender_handle = tokio::spawn(async move { sender.run().await });

            println!("spawning sender");

            tokio::select! {
                _ = signal::ctrl_c() => {println!("Shutting Down...")}
                _ = sender_handle => {}
            }
        }
        "receiver" => {
            let socket = UdpSocket::bind(&config.local_addr).await?;
            // socket.connect(&config.remote_addr).await?;

            println!("in receiver mode");

            if config.token.is_none() {
                eprintln!("InfluxDB token not provided. Exiting...");
                return Ok(());
            }
            let (tx, rx) = mpsc::channel(100);
            let influx_client = Client::new(config.influx_url.clone(), config.database.clone())
                .with_token(config.token.expect("token not found").clone());

            let mut reporter = InfluxReporter {
                client: influx_client,
                rx,
            };

            let receiver = UdpReceiver {
                socket,
                measurement_tx: tx,
                local_addr: config.local_addr.clone(),
                remote_addr: config.remote_addr.clone(),
            };

            let receiver_handle = tokio::spawn(async move { receiver.run().await });

            let reporter_handle = tokio::spawn(async move { reporter.run().await });

            tokio::select! {
                _ = signal::ctrl_c() => {println!("Shutting Down...")}
                _ = receiver_handle => {}
                _ = reporter_handle => {}
            }
        }
        "both" => {
            let sender_socket = UdpSocket::bind("0.0.0.0:0").await?;
            sender_socket.connect(&config.remote_addr).await?;

            let receiver_socket = UdpSocket::bind(&config.local_addr).await?;

            println!("in both mode");

            if config.token.is_none() {
                eprintln!("InfluxDB token not provided. Exiting...");
                return Ok(());
            }
            let (tx, rx) = mpsc::channel(100);
            let influx_client = Client::new(config.influx_url.clone(), config.database.clone())
                .with_token(config.token.expect("token not found").clone());

            let mut reporter = InfluxReporter {
                client: influx_client,
                rx,
            };

            let sender = UdpSender {
                socket: sender_socket,
                rate: TokioDuration::from_millis(config.rate),
                local_addr: config.local_addr.clone(),
            };

            let receiver = UdpReceiver {
                socket: receiver_socket,
                measurement_tx: tx,
                local_addr: config.local_addr.clone(),
                remote_addr: config.remote_addr.clone(),
            };

            let sender_handle = tokio::spawn(async move { sender.run().await });

            let receiver_handle = tokio::spawn(async move { receiver.run().await });

            let reporter_handle = tokio::spawn(async move { reporter.run().await });

            tokio::select! {
                _ = signal::ctrl_c() => {println!("Shutting Down...")}
                _ = sender_handle => {}
                _ = receiver_handle => {}
                _ = reporter_handle => {}
            }
        }
        _ => {
            eprintln!("Invalid mode specified. Use 'sender', 'receiver', or 'both'.");
            return Ok(());
        }
    }
    Ok(())
}
