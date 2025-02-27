# Packet Flow
[![Docker Image Build](https://github.com/u6bkep/packet-flow/actions/workflows/build.yml/badge.svg)](https://github.com/u6bkep/packet-flow/actions/workflows/build.yml)

A network monitoring tool that measures UDP packet loss and latency between endpoints and stores metrics in InfluxDB.

## Overview

Packet Flow sends UDP packets between endpoints and monitors network performance metrics like packet loss and latency. All metrics are sent to InfluxDB for storage and visualization.

## Features

- UDP packet sending with configurable rate
- Packet loss detection and measurement
- Network latency measurement
- Multiple operating modes: sender, receiver, or both
- Flexible configuration via command-line, environment variables, or config file
- InfluxDB integration for metrics storage
- Docker support

## Installation

### Building from source

```sh
# Clone the repository
git clone https://github.com/yourusername/packet-flow.git
cd packet-flow

# Build the project
cargo build --release

# The binary will be available at target/release/packet-flow
```

### Using Docker

```sh
# Build the Docker image
docker build -t packet-flow .

# Run the container
docker run packet-flow --help
```

## Configuration

Configuration can be provided in three ways (in order of precedence):

1. Command line arguments
2. Environment variables
3. Configuration file (TOML)
4. Default values

### Generate a default config file

```sh
./target/release/packet-flow --generate-config --config-file config.toml
```

### Configuration options

| Option | CLI Flag | Environment Variable | Description |
|--------|----------|----------------------|-------------|
| Mode | `-m, --mode` | `PACKET_FLOW_MODE` | Operating mode: "sender", "receiver", or "both" |
| Local Address | `--local-addr` | `PACKET_FLOW_LOCAL_ADDR` | Local address to bind to (e.g., "0.0.0.0:8000") |
| Remote Address | `--remote-addr` | `PACKET_FLOW_REMOTE_ADDR` | Remote address to connect to (e.g., "127.0.0.1:8000") |
| Rate | `-r, --rate` | `PACKET_FLOW_RATE` | Packet sending rate in milliseconds |
| InfluxDB URL | `-i, --influx-url` | `PACKET_FLOW_INFLUX_URL` | InfluxDB server URL |
| Database | `-d, --database` | `PACKET_FLOW_DATABASE` | InfluxDB database/bucket name |
| Token | `-t, --token` | `PACKET_FLOW_TOKEN` | InfluxDB authentication token |
| username | `-u, --username` | `PACKET_FLOW_USERNAME` | InfluxDB username |
| password | `-p, --password` | `PACKET_FLOW_PASSWORD` | InfluxDB password |
| Config File | `--config_file` | `PACKET_FLOW_CONFIG_FILE` | Path to a TOML config file. this is the only environment variable that overrides a commandline option |
| Generate Config | `--generate-config` | N/A | Generate a default config file |

## Usage

### Sender Mode

Sends UDP packets to a specified address:

```sh
./target/release/packet-flow --mode sender --remote-addr 192.168.1.100:8000 --rate 100
```

### Receiver Mode

Listens for UDP packets and reports metrics to InfluxDB:

```sh
./target/release/packet-flow --mode receiver --local-addr 0.0.0.0:8000 --token your-influxdb-token
```

### Both Mode

Sends and receives packets simultaneously:

```sh
./target/release/packet-flow --mode both --local-addr 0.0.0.0:8000 --remote-addr 192.168.1.100:8000 --token your-influxdb-token
```

## InfluxDB Setup

1. Make sure InfluxDB is running
2. Create a bucket named "network_monitor" (or specify a different name with `--database`)
3. Generate an API token with write access to the bucket
4. Provide the token to packet-flow with `--token` or the `PACKET_FLOW_TOKEN` environment variable

The included start_influx.sh script can be used to start a local InfluxDB instance for testing.

