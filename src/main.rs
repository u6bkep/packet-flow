use chrono::prelude::{DateTime, Utc};
use clap::Parser;
use influxdb::{Client, InfluxDbWriteable, WriteQuery};
use log::{debug, error, info, trace, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    net::UdpSocket,
    signal,
    sync::{broadcast, mpsc::{self, Receiver, Sender}},
    time::Duration as TokioDuration,
};
use thiserror::Error;

// Environment variable for config file path
const CONFIG_FILE_ENV_VAR: &str = "PACKET_FLOW_CONFIG";

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
    
    #[error("No InfluxDB authentication provided")]
    MissingAuthentication,
    
    #[error("Invalid mode: {0}")]
    InvalidMode(String),
}

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Mode to run in: "sender", "receiver", or "both"
    #[clap(short, long)]
    mode: Option<String>,

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
    
    /// InfluxDB username
    #[clap(short = 'u', long)]
    username: Option<String>,
    
    /// InfluxDB password
    #[clap(short = 'p', long)]
    password: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct Config {
    mode: Option<String>, // Add mode to the config
    local_addr: String,
    remote_addr: String,
    rate: u64,
    influx_url: String,
    database: String,
    token: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

impl Config {
    // Create a default configuration
    fn default() -> Self {
        Self {
            mode: None, // Default mode is None
            local_addr: "0.0.0.0:8000".to_string(),
            remote_addr: "127.0.0.1:8000".to_string(),
            rate: 1000,
            influx_url: "http://localhost:8086".to_string(),
            database: "packet_flow".to_string(),
            token: None,
            username: None,
            password: None,
        }
    }

    // Apply environment variables to override config values
    fn apply_env_vars(&mut self) {
        if let Ok(val) = env::var("PACKET_FLOW_MODE") {
            self.mode = Some(val);
        }
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
                warn!("Invalid PACKET_FLOW_RATE value '{}', ignoring", val);
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
        
        if let Ok(val) = env::var("PACKET_FLOW_USERNAME") {
            self.username = Some(val);
        }
        
        if let Ok(val) = env::var("PACKET_FLOW_PASSWORD") {
            self.password = Some(val);
        }
    }

    // Apply command-line arguments to override config values
    fn apply_args(&mut self, args: &Args) {
        if let Some(mode) = &args.mode {
            self.mode = Some(mode.clone());
        }
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
        if let Some(username) = &args.username {
            self.username = Some(username.clone());
        }
        if let Some(password) = &args.password {
            self.password = Some(password.clone());
        }
    }

    // Validate configuration values
    fn validate(&self) -> Result<(), AppError>{
        // Check that mode is valid
        if let Some(ref mode) = self.mode {
            if !["sender", "receiver", "both"].contains(&mode.as_str()) {
                return Err(AppError::InvalidMode(format!("{}, choose from 'sender', 'receiver', or 'both'.",mode.clone())));
            }
        }
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

    // Check if authentication credentials are provided
    fn has_authentication(&self) -> bool {
        self.token.is_some() || (self.username.is_some() && self.password.is_some())
    }

    // Load configuration from file and apply command-line overrides
    fn load(args: &Args) -> Result<Self, AppError> {
        // Config loading priority (highest to lowest):
        // 1. Command-line arguments
        // 2. Environment variables
        // 3. Config file
        // 4. Default values
        
        // Check for config file path in environment variable
        let config_path = if let Ok(env_path) = env::var(CONFIG_FILE_ENV_VAR) {
            info!("Using config file from environment variable: {}", env_path);
            PathBuf::from(env_path)
        } else {
            args.config_file.clone()
        };
        
        let mut config = if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            info!("Reading configuration from file: {}", config_path.display());
            match toml::from_str::<Config>(&contents) {
                Ok(config) => config,
                Err(e) => {
                    error!("Error parsing config file: {}", e);
                    info!("Falling back to default configuration");
                    Config::default()
                }
            }
        } else {
            info!("Config file not found. Using default configuration.");
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
        info!("Default configuration saved to {}", path.display());
        Ok(())
    }
}

impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Config {{\n")?;
        write!(f, "  mode: {}\n", self.mode.as_deref().unwrap_or("Not set"))?;
        write!(f, "  local bind address: {}\n", self.local_addr)?;
        write!(f, "  remote address: {}\n", self.remote_addr)?;
        write!(f, "  rate: {}ms\n", self.rate)?;
        write!(f, "  influx url: {}\n", self.influx_url)?;
        write!(f, "  database (bucket): {}\n", self.database)?;
        write!(f, "  token: {}\n", self.token.as_ref().map(|_| "****").unwrap_or("Not set"))?;
        write!(f, "  username: {}\n", self.username.as_deref().unwrap_or("Not set"))?;
        write!(f, "  password: {}\n", self.password.as_ref().map(|_| "****").unwrap_or("Not set"))?;
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
    shutdown_rx: broadcast::Receiver<()>,
}

impl InfluxReporter {
    async fn run(&mut self) {
        info!(
            "Reporting measurements to InfluxDB at {}...",
            self.client.database_url()
        );
        let mut messurements: Vec<Measurement> = Vec::new();
        let mut shutdown_rx = self.shutdown_rx.resubscribe();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received, flushing remaining measurements");
                    // Try to receive any remaining measurements
                    let _ = self.rx.recv_many(&mut messurements, 1000).await;
                    if !messurements.is_empty() {
                        self.flush_batch(&mut messurements).await;
                    }
                    info!("Reporter shutdown complete");
                    break;
                }
                num = self.rx.recv_many(&mut messurements, 100) => {
                    if num > 0 {
                        trace!("Flushing batch of {} measurements", messurements.len());
                        self.flush_batch(&mut messurements).await;
                    } else {
                        tokio::time::sleep(TokioDuration::from_millis(100)).await;
                    }
                }
            }
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

        match self.client.query(&queries).await {
            Ok(x) => trace!("Batch of {} measurements written to InfluxDB, got back: {}", queries.len(), x),
            Err(e) => error!("Error writing measurements to InfluxDB: {}", e),
        }
    }
}

struct UdpSender {
    socket: UdpSocket,
    rate: TokioDuration,
    shutdown_rx: broadcast::Receiver<()>,
}

impl UdpSender {
    async fn run(&self) {
        info!(
            "Sending packets from {} to {}...",
            self.socket.local_addr().unwrap(),
            self.socket.peer_addr().unwrap()
        );
        let mut sequence = 0u64;
        let mut interval = tokio::time::interval(self.rate);
        let mut shutdown_rx = self.shutdown_rx.resubscribe();

        // Set up signal handler for SIGUSR1
        let mut sigusr1 =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                .expect("Failed to install SIGUSR1 handler");

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let packet = Packet {
                        sequence,
                        timestamp_us: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_micros() as u64,
                        is_final: false,
                    };

                    if let Ok(data) = bincode::serialize(&packet) {
                        match self.socket.send(&data).await {
                            Ok(_) => {
                                trace!("Sent packet: sequence={}", sequence);
                            },
                            Err(e) => {
                                match e.kind() {
                                    std::io::ErrorKind::ConnectionRefused => {
                                        warn!("Connection refused, ignoring...");
                                        continue;
                                    }
                                    _ => {
                                        error!("Socket error: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    sequence += 1;
                }

                _ = sigusr1.recv() => {
                    info!("SIGUSR1 received, skipping a sequence number");
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
                        is_final: true,
                    };

                    if let Ok(data) = bincode::serialize(&final_packet) {
                        info!("Shutdown signal received, sending final packet");
                        let _ = self.socket.send(&data).await;
                    }
                    info!("Sent final packet, shutting down sender");
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
    shutdown_rx: broadcast::Receiver<()>,
}

impl UdpReceiver {
    async fn run(&self) {
        let mut buffer = [0u8; 1024];
        // Track sequence numbers and last seen time for each sender
        let mut last_sequences: HashMap<String, u64> = HashMap::new();
        let mut last_seen: HashMap<String, SystemTime> = HashMap::new();
        let mut shutdown_rx = self.shutdown_rx.resubscribe();
        
        // Define the sender deadness threshold (1 minute)
        let sender_deadness_threshold = TokioDuration::from_secs(60);

        info!("Listening on {}...", self.local_addr);

        loop {
            // Check for shutdown first
            if let Ok(_) = shutdown_rx.try_recv() {
                info!("Shutdown signal received, stopping receiver");
                break;
            }

            // Check for timeouts before receiving next packet
            let now = SystemTime::now();
            let mut dead_senders = Vec::new();
            let timeout_senders: Vec<String> = last_seen
                .iter()
                .filter_map(|(sender, last_time)| {
                    if let Ok(duration_since) = now.duration_since(*last_time) {
                        // Check if sender should be considered dead (> 1 minute inactive)
                        if duration_since > sender_deadness_threshold {
                            dead_senders.push(sender.clone());
                            None // Don't report timeout for dead senders, we'll remove them
                        } else if duration_since > self.timeout_duration {
                            Some(sender.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect();

            // Remove dead senders from tracking
            for sender in &dead_senders {
                info!("Sender {} has been inactive for over 1 minute, removing from tracking", sender);
                last_sequences.remove(sender);
                last_seen.remove(sender);
            }

            // Report timeouts for each timed-out sender
            for sender in &timeout_senders {
                warn!("Timeout for sender: {}", sender);
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

            match tokio::time::timeout(TokioDuration::from_secs(1), self.socket.recv_from(&mut buffer))
                .await
            {
                Ok(Ok((size, addr))) => {
                    let source = addr.to_string();
                    
                    if let Ok(packet) = bincode::deserialize::<Packet>(&buffer[..size]) {
                        // Process the packet inline here instead of in a separate function
                        let now_micro = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_micros() as u64;
                        let latency = now_micro - packet.timestamp_us;

                        // Track last seen time for this sender
                        last_seen.insert(source.clone(), SystemTime::now());

                        // Handle "is_final" packet by removing the sender from tracking
                        if packet.is_final {
                            info!(
                                "Received final packet from {}, removing from tracking",
                                source
                            );
                            last_sequences.remove(&source);
                            last_seen.remove(&source);
                            continue;
                        }

                        // Get or insert the last sequence number for this sender
                        let last_sequence = last_sequences
                            .entry(source.clone())
                            .or_insert(packet.sequence);

                        // Check for packet loss if we've seen this sender before
                        if *last_sequence != packet.sequence
                            && packet.sequence > last_sequence.wrapping_add(1)
                        {
                            warn!(
                                "Packet loss detected from {}: expected {}, got {}",
                                source,
                                last_sequence.wrapping_add(1),
                                packet.sequence
                            );
                            let lost_packets =
                                packet.sequence.wrapping_sub(last_sequence.wrapping_add(1));
                            let loss = PacketLoss {
                                time: Utc::now(),
                                source: source.clone(),
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
                            source: source.clone(),
                            destination: self.local_addr.clone(),
                            value: latency,
                        };

                        debug!(
                            "Received packet from {}: sequence={}, latency={}us",
                            source, packet.sequence, latency
                        );

                        self.measurement_tx
                            .send(Measurement::Latency(latency_measurement))
                            .await
                            .unwrap();

                        // Update the last sequence number for this sender
                        *last_sequence = packet.sequence;
                    }
                }
                Ok(Err(e)) => {
                    error!("Socket error: {}", e);
                    break;
                }
                Err(_) => {
                    // No packet received in this iteration, continue to check timeouts
                    continue;
                }
            }
        }
        
        info!("Receiver shutdown complete");
    }
}

// Helper function to create the InfluxDB client with appropriate authentication
fn create_influx_client(config: &Config) -> Result<Client, AppError> {
    let client = Client::new(&config.influx_url, &config.database);
    
    // Prefer token auth if available
    if let Some(token) = &config.token {
        return Ok(client.with_token(token));
    }
    
    log::debug!("No token provided, falling back to username/password auth");

    // Fall back to username/password auth
    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        return Ok(client.with_auth(username, password));
    }
    
    // No auth provided
    Err(AppError::MissingAuthentication)
}

// Setup signal handlers for graceful shutdown
async fn setup_shutdown_signal() -> broadcast::Sender<()> {
    let (shutdown_tx, _) = broadcast::channel(1);
    let shutdown_tx_clone = shutdown_tx.clone();
    
    tokio::spawn(async move {
        let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler");
        let mut sigquit = signal::unix::signal(signal::unix::SignalKind::quit())
            .expect("Failed to install SIGQUIT handler");
            
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("SIGINT/CTRL+C received, initiating graceful shutdown");
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received, initiating graceful shutdown");
            }
            _ = sigquit.recv() => {
                info!("SIGQUIT received, initiating graceful shutdown");
            }
        }
        
        // Notify all components to shut down
        let _ = shutdown_tx_clone.send(());
        
        // Allow some time for cleanup before the process exits
        tokio::time::sleep(TokioDuration::from_secs(2)).await;
    });
    
    shutdown_tx
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    // Initialize the logger
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    
    let args = Args::parse();
    
    // Handle config generation request
    if args.generate_config {
        // Check for config file path in environment variable
        let config_path = if let Ok(env_path) = env::var(CONFIG_FILE_ENV_VAR) {
            PathBuf::from(env_path)
        } else {
            args.config_file.clone()
        };
        
        return Config::default().save(&config_path);
    }
    
    // Load and validate configuration
    let config = Config::load(&args)?;
    
    // Show current configuration
    info!("Configuration:\n{}", config);
    
    // Validate mode
    let mode = config.mode.as_deref().unwrap_or("");
    if !["sender", "receiver", "both"].contains(&mode) {
        return Err(AppError::InvalidMode(mode.to_string()));
    }
    
    // Setup shutdown signal handling
    let shutdown_tx = setup_shutdown_signal().await;
    
    // For receiver and both modes, verify authentication upfront
    if (mode == "receiver" || mode == "both") && !config.has_authentication() {
        return Err(AppError::MissingAuthentication);
    }
    
    match mode {
        "sender" => {
            let socket = UdpSocket::bind("0.0.0.0:0").await?;
            socket.connect(&config.remote_addr).await?;
            
            info!("Running in sender mode");
            
            let sender = UdpSender {
                socket,
                rate: TokioDuration::from_millis(config.rate),
                shutdown_rx: shutdown_tx.subscribe(),
            };
            
            sender.run().await;
        }
        "receiver" => {
            let socket = UdpSocket::bind(&config.local_addr).await?;

            info!("Running in receiver mode");

            if !config.has_authentication() {
                error!("InfluxDB authentication not provided. Exiting...");
                return Ok(());
            }
            
            let (tx, rx) = mpsc::channel(100);
            let influx_client = create_influx_client(&config)?;

            let mut reporter = InfluxReporter {
                client: influx_client,
                rx,
                shutdown_rx: shutdown_tx.subscribe(),
            };

            let receiver = UdpReceiver {
                socket,
                measurement_tx: tx.clone(),
                local_addr: config.local_addr.clone(),
                timeout_duration: TokioDuration::from_secs(5),
                shutdown_rx: shutdown_tx.subscribe(),
            };

            // Run both components concurrently
            let (receiver_result, reporter_result) = tokio::join!(
                receiver.run(),
                reporter.run()
            );
            
            info!("Receiver mode shutdown complete");
        }
        "both" => {
            let sender_socket = UdpSocket::bind("0.0.0.0:0").await?;
            sender_socket.connect(&config.remote_addr).await?;

            let receiver_socket = UdpSocket::bind(&config.local_addr).await?;

            info!("Running in both mode");

            if !config.has_authentication() {
                error!("InfluxDB authentication not provided. Exiting...");
                return Ok(());
            }
            
            let (tx, rx) = mpsc::channel(100);
            let influx_client = create_influx_client(&config)?;

            let mut reporter = InfluxReporter {
                client: influx_client,
                rx,
                shutdown_rx: shutdown_tx.subscribe(),
            };

            let sender = UdpSender {
                socket: sender_socket,
                rate: TokioDuration::from_millis(config.rate),
                shutdown_rx: shutdown_tx.subscribe(),
            };

            let receiver = UdpReceiver {
                socket: receiver_socket,
                measurement_tx: tx,
                local_addr: config.local_addr.clone(),
                timeout_duration: TokioDuration::from_secs(5),
                shutdown_rx: shutdown_tx.subscribe(),
            };

            // Run all components concurrently
            let (sender_result, receiver_result, reporter_result) = tokio::join!(
                sender.run(),
                receiver.run(),
                reporter.run()
            );
            
            info!("Both mode shutdown complete");
        }
        _ => {
            error!("Invalid mode specified. Use 'sender', 'receiver', or 'both'.");
            return Ok(());
        }
    }
    
    info!("Application shutdown complete");
    Ok(())
}
