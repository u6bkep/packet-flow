use chrono::prelude::{DateTime, Utc};
use clap::Parser;
use influxdb::{Client, InfluxDbWriteable, WriteQuery};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    net::UdpSocket,
    signal,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration as TokioDuration,
};
use thiserror::Error;

// comprehensive error type
#[derive(Error, Debug)]
enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("TOML deserialization error: {0}")]
    TomlDeserialize(#[from] toml::de::Error),
    
    #[error("TOML serialization error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
    
    #[error("InfluxDB token required but not provided")]
    MissingToken,
    
    #[error("Invalid mode: {0}")]
    InvalidMode(String),
}

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Mode to run in: "sender", "receiver", or "both"
    #[clap(short, long)]
    mode: String,

    #[clap(short, long, default_value = "config.toml")]
    config_file: PathBuf,

    /// Generate default config file and exit
    #[clap(long)]
    generate_config: bool,

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

#[derive(Deserialize, Serialize, Debug, Clone)]
struct Config {
    local_addr: String,
    remote_addr: String,
    rate: u64,
    influx_url: String,
    database: String,
    token: Option<String>,
}

impl Config {
    // Create a default configuration
    fn default() -> Self {
        Self {
            local_addr: "0.0.0.0:8000".to_string(),
            remote_addr: "127.0.0.1:8000".to_string(),
            rate: 1000,
            influx_url: "http://localhost:8086".to_string(),
            database: "network_monitor".to_string(),
            token: None,
        }
    }

    // Apply environment variables to override config values
    fn apply_env_vars(&mut self) {
        if let Ok(val) = env::var("PACKET_FLOW_LOCAL_ADDR") {
            self.local_addr = val;
        }
        
        if let Ok(val) = env::var("PACKET_FLOW_REMOTE_ADDR") {
            self.remote_addr = val;
        }
        
        if let Ok(val) = env::var("PACKET_FLOW_RATE") {
            if let Ok(rate) = val.parse::<u64>() {
                self.rate = rate;
            } else {
                eprintln!("Warning: Invalid PACKET_FLOW_RATE value '{}', ignoring", val);
            }
        }
        
        if let Ok(val) = env::var("PACKET_FLOW_INFLUX_URL") {
            self.influx_url = val;
        }
        
        if let Ok(val) = env::var("PACKET_FLOW_DATABASE") {
            self.database = val;
        }
        
        if let Ok(val) = env::var("PACKET_FLOW_TOKEN") {
            self.token = Some(val);
        }
    }

    // Apply command-line arguments to override config values
    fn apply_args(&mut self, args: &Args) {
        if let Some(addr) = &args.local_addr {
            self.local_addr = addr.clone();
        }
        if let Some(addr) = &args.remote_addr {
            self.remote_addr = addr.clone();
        }
        if let Some(rate) = args.rate {
            self.rate = rate;
        }
        if let Some(url) = &args.influx_url {
            self.influx_url = url.clone();
        }
        if let Some(db) = &args.database {
            self.database = db.clone();
        }
        if let Some(token) = &args.token {
            self.token = Some(token.clone());
        }
    }

    // Validate configuration values
    fn validate(&self) -> Result<(), AppError>{
        // Check that addresses are valid
        if self.local_addr.split(':').count() != 2 {
            return Err(AppError::InvalidConfig(format!("Invalid local address: {}", self.local_addr)));
        }
        
        if self.remote_addr.split(':').count() != 2 {
            return Err(AppError::InvalidConfig(format!("Invalid remote address: {}", self.remote_addr)));
        }
        
        // Check that rate is reasonable
        if self.rate < 1 || self.rate > 10000 {
            return Err(AppError::InvalidConfig(format!("Rate must be between 1 and 10000, got {}", self.rate)));
        }
        
        // Minimal URL validation
        if !self.influx_url.starts_with("http://") && !self.influx_url.starts_with("https://") {
            return Err(AppError::InvalidConfig(format!("InfluxDB URL must start with http:// or https://")));
        }
        
        Ok(())
    }

    // Load configuration from file and apply command-line overrides
    fn load(args: &Args) -> Result<Self, AppError> {
        // Config loading priority (highest to lowest):
        // 1. Command-line arguments
        // 2. Environment variables
        // 3. Config file
        // 4. Default values
        
        let mut config = if args.config_file.exists() {
            let contents = std::fs::read_to_string(&args.config_file)?;
            println!("Reading configuration from file: {}", args.config_file.display());
            match toml::from_str::<Config>(&contents) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("Error parsing config file: {}", e);
                    println!("Falling back to default configuration");
                    Config::default()
                }
            }
        } else {
            println!("Config file not found. Using default configuration.");
            Config::default()
        };
        
        // Apply environment variables
        config.apply_env_vars();
        
        // Apply command-line arguments (highest priority)
        config.apply_args(args);
        
        // Validate the final configuration
        config.validate()?;
        
        Ok(config)
    }

    // Save configuration to file
    fn save(&self, path: &PathBuf) -> Result<(), AppError> {
        let contents = toml::to_string_pretty(self).map_err(AppError::TomlSerialize)?;
        std::fs::write(path, contents)?;
        println!("Default configuration saved to {}", path.display());
        Ok(())
    }
}

impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Config {{\n")?;
        write!(f, "  local bind address: {}\n", self.local_addr)?;
        write!(f, "  remote address: {}\n", self.remote_addr)?;
        write!(f, "  rate: {}ms\n", self.rate)?;
        write!(f, "  influx url: {}\n", self.influx_url)?;
        write!(f, "  database (bucket): {}\n", self.database)?;
        write!(f, "  token: {}\n", self.token.as_ref().map(|_| "****").unwrap_or("Not set"))?;
        write!(f, "}}")
    }
    
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
async fn main() -> Result<(), AppError> {
    let args = Args::parse();
    
    // Handle config generation request
    if args.generate_config {
        return Config::default().save(&args.config_file);
    }
    
    // Load and validate configuration
    let config = Config::load(&args)?;
    
    // Show current configuration
    println!("Configuration:\n{}", config);
    
    // Validate mode
    let mode = args.mode.as_str();
    if !["sender", "receiver", "both"].contains(&mode) {
        return Err(AppError::InvalidMode(mode.to_string()));
    }
    
    // For receiver and both modes, verify token upfront
    if (mode == "receiver" || mode == "both") && config.token.is_none() {
        return Err(AppError::MissingToken);
    }
    
    match mode {
        "sender" => {
            let socket = UdpSocket::bind("0.0.0.0:0").await?;
            socket.connect(&config.remote_addr).await?;
            
            println!("Running in sender mode");
            
            let sender = UdpSender {
                socket,
                rate: TokioDuration::from_millis(config.rate),
                local_addr: config.local_addr.clone(),
            };
            
            let sender_handle = tokio::spawn(async move { sender.run().await });
            sender_handle.await.map_err(|_| AppError::InvalidConfig("Sender task failed".to_string()))?;
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
