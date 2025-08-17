# Installation Guide

This guide covers various installation methods for Chronik Stream.

## Table of Contents

- [Docker Installation](#docker-installation)
- [Building from Source](#building-from-source)
- [Kubernetes Deployment](#kubernetes-deployment)
- [Package Managers](#package-managers)
- [System Requirements](#system-requirements)

## System Requirements

### Minimum Requirements

- **CPU**: 2 cores
- **RAM**: 4GB
- **Storage**: 10GB available space
- **OS**: Linux, macOS, or Windows (with WSL2)

### Recommended for Production

- **CPU**: 8+ cores
- **RAM**: 16GB+
- **Storage**: SSD with 100GB+ available
- **Network**: 1Gbps+ network interface

## Docker Installation

### Using Docker Compose (Recommended)

1. **Download docker-compose.yml**:

```bash
curl -O https://raw.githubusercontent.com/chronik-stream/chronik-stream/main/docker-compose.yml
```

2. **Start services**:

```bash
docker-compose up -d
```

3. **Verify installation**:

```bash
docker-compose ps
curl http://localhost:8080/health
```

### Using Docker Run

For a single-node setup:

```bash
# Create network
docker network create chronik

# Start TiKV (metadata store)
docker run -d \
  --name tikv \
  --network chronik \
  -p 2379:2379 \
  pingcap/tikv:latest

# Start Chronik Stream
docker run -d \
  --name chronik-stream \
  --network chronik \
  -p 9092:9092 \
  -p 8080:8080 \
  -e TIKV_PD_ENDPOINTS=tikv:2379 \
  chronik/chronik-stream:latest
```

## Building from Source

### Prerequisites

1. **Install Rust** (1.70 or later):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

2. **Install build dependencies**:

**Ubuntu/Debian**:
```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev cmake
```

**macOS**:
```bash
brew install cmake openssl
```

**Fedora/RHEL**:
```bash
sudo dnf install -y gcc gcc-c++ make cmake openssl-devel
```

### Build Steps

1. **Clone the repository**:

```bash
git clone https://github.com/chronik-stream/chronik-stream.git
cd chronik-stream
```

2. **Build the project**:

```bash
cargo build --release
```

3. **Run tests** (optional):

```bash
cargo test
```

4. **Install binaries**:

```bash
# Install to ~/.cargo/bin
cargo install --path crates/chronik-ingest
cargo install --path crates/chronik-query

# Or copy manually
sudo cp target/release/chronik-ingest /usr/local/bin/
sudo cp target/release/chronik-query /usr/local/bin/
```

### Configuration

Create a configuration file at `/etc/chronik/config.yaml`:

```yaml
# Chronik Stream Configuration
server:
  kafka_port: 9092
  http_port: 8080
  
storage:
  data_dir: /var/lib/chronik/data
  cache_size: 1073741824  # 1GB
  
metadata:
  tikv_endpoints:
    - localhost:2379
  
search:
  index_dir: /var/lib/chronik/index
  memory_budget: 2147483648  # 2GB
  
logging:
  level: info
  format: json
```

### Running Chronik Stream

1. **Start TiKV** (for metadata):

```bash
# Using Docker
docker run -d -p 2379:2379 pingcap/tikv:latest

# Or install TiKV directly
# See: https://tikv.org/docs/deploy/install/
```

2. **Start Chronik Stream**:

```bash
# With default config
chronik-ingest

# With custom config
chronik-ingest --config /etc/chronik/config.yaml
```

## Kubernetes Deployment

See the [Kubernetes Deployment Guide](../deployment/kubernetes.md) for detailed instructions.

Quick start with Helm:

```bash
# Add Chronik Stream Helm repository
helm repo add chronik https://chronik-stream.github.io/helm-charts
helm repo update

# Install with default values
helm install my-chronik chronik/chronik-stream

# Install with custom values
helm install my-chronik chronik/chronik-stream \
  --set replicaCount=3 \
  --set persistence.size=100Gi
```

## Package Managers

### Homebrew (macOS)

```bash
brew tap chronik-stream/tap
brew install chronik-stream
```

### APT (Ubuntu/Debian)

```bash
# Add repository
curl -fsSL https://packages.chronik-stream.io/gpg | sudo apt-key add -
echo "deb https://packages.chronik-stream.io/apt stable main" | sudo tee /etc/apt/sources.list.d/chronik.list

# Install
sudo apt update
sudo apt install chronik-stream
```

### YUM/DNF (RHEL/Fedora)

```bash
# Add repository
sudo dnf config-manager --add-repo https://packages.chronik-stream.io/rpm/chronik.repo

# Install
sudo dnf install chronik-stream
```

## Post-Installation

### Verify Installation

1. **Check service status**:

```bash
# Systemd
sudo systemctl status chronik-stream

# Docker
docker ps | grep chronik

# Process
ps aux | grep chronik-ingest
```

2. **Test connectivity**:

```bash
# Health check
curl http://localhost:8080/health

# Kafka protocol
kafkacat -L -b localhost:9092
```

3. **View logs**:

```bash
# Systemd
sudo journalctl -u chronik-stream -f

# Docker
docker logs chronik-stream -f

# File logs
tail -f /var/log/chronik/chronik-stream.log
```

### Security Configuration

1. **Enable authentication** (see [Security Guide](../deployment/security.md))
2. **Configure TLS/SSL** for encrypted connections
3. **Set up firewall rules** for production deployments

### Performance Tuning

For production deployments, see:
- [Performance Tuning Guide](../deployment/performance-tuning.md)
- [Hardware Recommendations](../deployment/hardware-recommendations.md)

## Troubleshooting

### Common Issues

1. **Port already in use**:
   ```bash
   # Find process using port
   sudo lsof -i :9092
   # Change port in configuration
   ```

2. **TiKV connection failed**:
   ```bash
   # Check TiKV is running
   curl http://localhost:2379/pd/api/v1/members
   ```

3. **Out of memory**:
   - Increase system memory
   - Adjust `memory_budget` in configuration
   - Enable swap (not recommended for production)

### Getting Help

- Check [FAQ](faq.md)
- Join [Discord Community](https://discord.gg/chronik-stream)
- Report issues on [GitHub](https://github.com/chronik-stream/chronik-stream/issues)

## Next Steps

- Follow the [First Application Tutorial](first-application.md)
- Learn about [Configuration Options](configuration.md)
- Explore [Client Libraries](client-libraries.md)