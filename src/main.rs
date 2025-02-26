use chrono::prelude::{DateTime, Utc};
use clap::Parser;
use influxdb::{Client, InfluxDbWriteable, WriteQuery};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    net::UdpSocket,
    signal,
    sync::mpsc::{self, Receiver, Sender},
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
    is_final: bool, // New field to indicate this is the final packet from this sender
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

        // Set up signal handler for SIGUSR1
        let mut sigusr1 =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                .expect("Failed to install SIGUSR1 handler");

        // Set up a channel to receive shutdown notification
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        // Handle CTRL+C signal to ensure we send final packet
        tokio::spawn(async move {
            if let Ok(_) = signal::ctrl_c().await {
                let _ = shutdown_tx.send(()).await;
                println!("Received CTRL+C, shutting down sender");
            }
        });

        loop {
            tokio::select! {
            _ = interval.tick() => {
                let packet = Packet {
                    sequence,
                    timestamp_us: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_micros() as u64,
                    source: self.local_addr.clone(),
                    is_final: false,
                };

                if let Ok(data) = bincode::serialize(&packet) {
                    self.socket.send(&data).await.unwrap();
                }

                sequence += 1;
                }

                _ = sigusr1.recv() => {
                    println!("SIGUSR1 received, skipping a sequence number");
                    sequence += 1;
                    }

                _ = shutdown_rx.recv() => {
                    // Send final packet with is_final=true
                    let final_packet = Packet {
                        sequence,
                        timestamp_us: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_micros() as u64,
                        source: self.local_addr.clone(),
                        is_final: true,
                    };

                    if let Ok(data) = bincode::serialize(&final_packet) {
                        let _ = self.socket.send(&data).await;
                    }
                    println!("Sent final packet, shutting down sender");
                    break;
                }
            }
        }
    }
}

struct UdpReceiver {
    socket: UdpSocket,
    measurement_tx: Sender<Measurement>,
    local_addr: String,
    timeout_duration: TokioDuration,
}

impl UdpReceiver {
    async fn run(&self) {
        let mut buffer = [0u8; 1024];
        // Track sequence numbers and last seen time for each sender
        let mut last_sequences: HashMap<String, u64> = HashMap::new();
        let mut last_seen: HashMap<String, SystemTime> = HashMap::new();

        println!("Listening on {}...", self.local_addr);

        loop {
            // Check for timeouts before receiving next packet
            let now = SystemTime::now();
            let timeout_senders: Vec<String> = last_seen
                .iter()
                .filter_map(|(sender, last_time)| {
                    if now.duration_since(*last_time).ok()? > self.timeout_duration {
                        Some(sender.clone())
                    } else {
                        None
                    }
                })
                .collect();

            // Report timeouts for each timed-out sender
            for sender in &timeout_senders {
                println!("Timeout for sender: {}", sender);
                let loss = PacketLoss {
                    time: Utc::now(),
                    source: sender.clone(),
                    destination: self.local_addr.clone(),
                    value: 1,
                };
                self.measurement_tx
                    .send(Measurement::PacketLoss(loss))
                    .await
                    .unwrap();
            }

            match tokio::time::timeout(TokioDuration::from_secs(1), self.socket.recv(&mut buffer))
                .await
            {
                Ok(Ok(size)) => {
                    if let Ok(packet) = bincode::deserialize::<Packet>(&buffer[..size]) {
                        // Process the packet inline here instead of in a separate function
                        let now_micro = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_micros() as u64;
                        let latency = now_micro - packet.timestamp_us;

                        // Track last seen time for this sender
                        last_seen.insert(packet.source.clone(), SystemTime::now());

                        // Handle "is_final" packet by removing the sender from tracking
                        if packet.is_final {
                            println!(
                                "Received final packet from {}, removing from tracking",
                                packet.source
                            );
                            last_sequences.remove(&packet.source);
                            last_seen.remove(&packet.source);
                            continue;
                        }

                        // Get or insert the last sequence number for this sender
                        let last_sequence = last_sequences
                            .entry(packet.source.clone())
                            .or_insert(packet.sequence);

                        // Check for packet loss if we've seen this sender before
                        if *last_sequence != packet.sequence
                            && packet.sequence > last_sequence.wrapping_add(1)
                        {
                            println!(
                                "Packet loss detected from {}: expected {}, got {}",
                                packet.source,
                                last_sequence.wrapping_add(1),
                                packet.sequence
                            );
                            let lost_packets =
                                packet.sequence.wrapping_sub(last_sequence.wrapping_add(1));
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

                        // Report latency
                        let latency_measurement = Latency {
                            time: Utc::now(),
                            source: packet.source.clone(),
                            destination: self.local_addr.clone(),
                            value: latency,
                        };
                        self.measurement_tx
                            .send(Measurement::Latency(latency_measurement))
                            .await
                            .unwrap();

                        // Update the last sequence number for this sender
                        *last_sequence = packet.sequence;
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("Socket error: {}", e);
                    break;
                }
                Err(_) => {
                    // No packet received in this iteration, continue to check timeouts
                    continue;
                }
            }
        }
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

            sender_handle.await?;
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
                timeout_duration: TokioDuration::from_secs(5),
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
                timeout_duration: TokioDuration::from_secs(5),
            };

            let sender_handle = tokio::spawn(async move { sender.run().await });

            let receiver_handle = tokio::spawn(async move { receiver.run().await });

            let reporter_handle = tokio::spawn(async move { reporter.run().await });

            tokio::select! {
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
