# Chronik Stream Deployment Guide

## Table of Contents
1. [Prerequisites](#prerequisites)
2. [Installation](#installation)
3. [Configuration](#configuration)
4. [Running the Server](#running-the-server)
5. [Docker Deployment](#docker-deployment)
6. [Kubernetes Deployment](#kubernetes-deployment)
7. [Production Considerations](#production-considerations)
8. [Monitoring](#monitoring)
9. [Troubleshooting](#troubleshooting)

## Prerequisites

### System Requirements
- **OS**: Linux, macOS, or Windows (with WSL2)
- **CPU**: 2+ cores recommended
- **Memory**: 512MB minimum, 2GB+ recommended
- **Storage**: 10GB+ for data storage
- **Network**: Open port 9092 for Kafka protocol

### Software Requirements
- Rust 1.70+ (for building from source)
- Docker (optional, for containerized deployment)
- Kubernetes (optional, for orchestrated deployment)

## Installation

### From Source

```bash
# Clone the repository
git clone https://github.com/lspecian/chronik-stream.git
cd chronik-stream

# Build the project
cargo build --release --bin chronik-server

# Binary will be at ./target/release/chronik-server
```

### Using Pre-built Binaries

```bash
# Download the latest release
wget https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-server-linux-amd64
chmod +x chronik-server-linux-amd64
sudo mv chronik-server-linux-amd64 /usr/local/bin/chronik-server
```

### Using Docker

```bash
# Pull the official image
docker pull chronik/chronik-stream:latest

# Or build locally
docker build -t chronik-stream .
```

## Configuration

### Environment Variables

```bash
# Core settings
export CHRONIK_KAFKA_PORT=9092
export CHRONIK_ADMIN_PORT=3000
export CHRONIK_DATA_DIR=/var/lib/chronik
export CHRONIK_BIND_ADDR=0.0.0.0

# Logging
export RUST_LOG=info,chronik=debug

# Performance tuning
export CHRONIK_BUFFER_MEMORY=67108864  # 64MB
export CHRONIK_SEGMENT_SIZE=268435456  # 256MB
```

### Configuration File

Create `chronik.toml`:

```toml
[server]
node_id = 0
advertised_host = "your-hostname.example.com"
advertised_port = 9092
bind_address = "0.0.0.0"
data_dir = "/var/lib/chronik/data"

[kafka]
auto_create_topics_enable = true
num_partitions = 3
default_replication_factor = 1
min_insync_replicas = 1

[storage]
backend = "local"  # Options: local, s3, gcs, azure
compression_type = "snappy"  # Options: none, gzip, snappy, lz4, zstd
segment_size_bytes = 268435456  # 256MB
segment_ms = 30000  # 30 seconds
retention_ms = 604800000  # 7 days

[producer]
acks = 1  # Options: 0, 1, -1 (all)
compression_type = "snappy"
batch_size = 16384
linger_ms = 10
buffer_memory = 33554432  # 32MB

[consumer]
fetch_min_bytes = 1
fetch_max_wait_ms = 500
max_partition_fetch_bytes = 1048576  # 1MB

[admin]
enabled = true
port = 3000
bind_address = "0.0.0.0"

[metrics]
enabled = true
port = 9090
path = "/metrics"
```

### Storage Backend Configuration

#### Local Filesystem
```toml
[storage]
backend = "local"
path = "/var/lib/chronik/segments"
```

#### AWS S3
```toml
[storage]
backend = "s3"
bucket = "my-chronik-bucket"
region = "us-west-2"
prefix = "chronik/segments/"

[storage.s3]
access_key_id = "AKIAIOSFODNN7EXAMPLE"
secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
```

#### Google Cloud Storage
```toml
[storage]
backend = "gcs"
bucket = "my-chronik-bucket"
prefix = "chronik/segments/"

[storage.gcs]
credentials_path = "/path/to/service-account.json"
```

#### Azure Blob Storage
```toml
[storage]
backend = "azure"
container = "chronik"
prefix = "segments/"

[storage.azure]
account = "myaccount"
access_key = "myaccesskey"
```

## Running the Server

### Basic Usage

```bash
# Start single-node (default)
chronik-server start

# Start with configuration file (enables cluster mode if configured)
chronik-server start --config /etc/chronik/chronik.toml

# Specify data directory and advertised address
chronik-server start --data-dir /var/lib/chronik --advertise hostname:9092
```

### Systemd Service

Create `/etc/systemd/system/chronik.service`:

```ini
[Unit]
Description=Chronik Stream Server
After=network.target
Documentation=https://github.com/yourusername/chronik-stream

[Service]
Type=simple
User=chronik
Group=chronik
ExecStart=/usr/local/bin/chronik-server start --config /etc/chronik/chronik.toml
Restart=on-failure
RestartSec=10
StandardOutput=journal
StandardError=journal
SyslogIdentifier=chronik

# Security
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/chronik

# Resource limits
LimitNOFILE=65536
LimitNPROC=32768

[Install]
WantedBy=multi-user.target
```

Enable and start:
```bash
sudo systemctl daemon-reload
sudo systemctl enable chronik
sudo systemctl start chronik
sudo systemctl status chronik
```

## Docker Deployment

### Dockerfile

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release --bin chronik-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/chronik-server /usr/local/bin/chronik-server
RUN useradd -m -u 1000 chronik

USER chronik
EXPOSE 9092 9090

VOLUME ["/data"]
ENV CHRONIK_DATA_DIR=/data

ENTRYPOINT ["chronik-server"]
CMD ["start"]
```

### Docker Compose

```yaml
version: '3.8'

services:
  chronik:
    image: chronik/chronik-stream:latest
    container_name: chronik
    ports:
      - "9092:9092"  # Kafka protocol
      - "3000:3000"  # Admin API
      - "9090:9090"  # Metrics
    volumes:
      - chronik-data:/data
      - ./chronik.toml:/etc/chronik/chronik.toml:ro
    environment:
      - RUST_LOG=info
      - CHRONIK_CONFIG=/etc/chronik/chronik.toml
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:3000/health"]
      interval: 30s
      timeout: 10s
      retries: 3

volumes:
  chronik-data:
    driver: local
```

Run with:
```bash
docker-compose up -d
docker-compose logs -f chronik
```

## Kubernetes Deployment

### StatefulSet

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: chronik
  namespace: streaming
spec:
  serviceName: chronik
  replicas: 1
  selector:
    matchLabels:
      app: chronik
  template:
    metadata:
      labels:
        app: chronik
    spec:
      containers:
      - name: chronik
        image: chronik/chronik-stream:latest
        ports:
        - containerPort: 9092
          name: kafka
        - containerPort: 3000
          name: admin
        - containerPort: 9090
          name: metrics
        env:
        - name: CHRONIK_ADVERTISED_HOST
          valueFrom:
            fieldRef:
              fieldPath: status.podIP
        - name: RUST_LOG
          value: "info"
        volumeMounts:
        - name: data
          mountPath: /data
        - name: config
          mountPath: /etc/chronik
        livenessProbe:
          httpGet:
            path: /health
            port: admin
          initialDelaySeconds: 30
          periodSeconds: 10
        readinessProbe:
          httpGet:
            path: /ready
            port: admin
          initialDelaySeconds: 10
          periodSeconds: 5
        resources:
          requests:
            memory: "512Mi"
            cpu: "500m"
          limits:
            memory: "2Gi"
            cpu: "2000m"
      volumes:
      - name: config
        configMap:
          name: chronik-config
  volumeClaimTemplates:
  - metadata:
      name: data
    spec:
      accessModes: ["ReadWriteOnce"]
      resources:
        requests:
          storage: 10Gi
```

### Service

```yaml
apiVersion: v1
kind: Service
metadata:
  name: chronik
  namespace: streaming
spec:
  type: ClusterIP
  ports:
  - port: 9092
    targetPort: kafka
    name: kafka
  - port: 3000
    targetPort: admin
    name: admin
  - port: 9090
    targetPort: metrics
    name: metrics
  selector:
    app: chronik
```

### ConfigMap

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: chronik-config
  namespace: streaming
data:
  chronik.toml: |
    [server]
    node_id = 0
    advertised_port = 9092
    data_dir = "/data"
    
    [kafka]
    auto_create_topics_enable = true
    num_partitions = 3
    
    [storage]
    backend = "local"
    compression_type = "snappy"
```

Deploy:
```bash
kubectl create namespace streaming
kubectl apply -f chronik-statefulset.yaml
kubectl apply -f chronik-service.yaml
kubectl apply -f chronik-configmap.yaml
```

## Production Considerations

### Performance Tuning

```bash
# System settings (add to /etc/sysctl.conf)
vm.swappiness=1
vm.max_map_count=262144
net.core.rmem_default=134217728
net.core.wmem_default=134217728
net.core.rmem_max=134217728
net.core.wmem_max=134217728
net.ipv4.tcp_rmem=4096 87380 134217728
net.ipv4.tcp_wmem=4096 65536 134217728

# Apply settings
sudo sysctl -p
```

### File Descriptor Limits

```bash
# Add to /etc/security/limits.conf
chronik soft nofile 100000
chronik hard nofile 100000
```

### Storage Considerations

- Use SSD for data directory
- Separate disk for data and logs
- Configure appropriate retention policies
- Monitor disk usage and growth

### Network Security

```bash
# Firewall rules (iptables)
sudo iptables -A INPUT -p tcp --dport 9092 -j ACCEPT
sudo iptables -A INPUT -p tcp --dport 3000 -s 10.0.0.0/8 -j ACCEPT
sudo iptables -A INPUT -p tcp --dport 9090 -s 10.0.0.0/8 -j ACCEPT
```

### Backup Strategy

```bash
#!/bin/bash
# Backup script
BACKUP_DIR="/backup/chronik"
DATA_DIR="/var/lib/chronik/data"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

# Stop writes (optional)
curl -X POST http://localhost:3000/admin/pause

# Create backup
tar -czf "$BACKUP_DIR/chronik_$TIMESTAMP.tar.gz" "$DATA_DIR"

# Resume writes
curl -X POST http://localhost:3000/admin/resume

# Upload to S3 (optional)
aws s3 cp "$BACKUP_DIR/chronik_$TIMESTAMP.tar.gz" s3://my-backup-bucket/chronik/
```

## Monitoring

### Prometheus Configuration

```yaml
global:
  scrape_interval: 15s

scrape_configs:
  - job_name: 'chronik'
    static_configs:
      - targets: ['chronik:9090']
```

### Grafana Dashboard

Import the provided dashboard from `monitoring/grafana-dashboard.json`.

Key metrics to monitor:
- Message throughput (messages/sec)
- Bytes in/out per second
- Consumer lag
- Disk usage
- Memory usage
- CPU usage
- Connection count
- Error rate

### Alerting Rules

```yaml
groups:
  - name: chronik
    rules:
    - alert: ChronikDown
      expr: up{job="chronik"} == 0
      for: 5m
      annotations:
        summary: "Chronik is down"
        
    - alert: HighConsumerLag
      expr: kafka_consumer_lag > 10000
      for: 10m
      annotations:
        summary: "High consumer lag detected"
        
    - alert: DiskSpaceLow
      expr: disk_free_percent < 10
      for: 5m
      annotations:
        summary: "Low disk space on Chronik server"
```

## Troubleshooting

### Common Issues

#### Server Won't Start
```bash
# Check logs
journalctl -u chronik -f

# Check port availability
sudo lsof -i:9092

# Check permissions
ls -la /var/lib/chronik
```

#### Client Connection Issues
```bash
# Test connectivity
telnet localhost 9092

# Check with kafkactl
kafkactl --brokers localhost:9092 get brokers

# Check firewall
sudo iptables -L -n
```

#### Performance Issues
```bash
# Check system resources
top -p $(pgrep chronik)
iostat -x 1
netstat -ant | grep 9092 | wc -l

# Check Chronik metrics
curl http://localhost:9090/metrics | grep chronik_
```

### Debug Mode

```bash
# Enable debug logging
RUST_LOG=debug,chronik=trace chronik-server start

# Enable protocol debugging
RUST_LOG=chronik_protocol=trace chronik-server start
```

### Recovery Procedures

#### Corrupted Segment Recovery
```bash
# Stop server
sudo systemctl stop chronik

# WAL recovery is automatic on startup
chronik-server start --data-dir /var/lib/chronik/data

# Restart server
sudo systemctl start chronik
```

#### Reset Consumer Group
```bash
# Reset to earliest
kafkactl --brokers localhost:9092 reset offset my-group --topic my-topic --to-earliest

# Reset to specific offset
kafkactl --brokers localhost:9092 reset offset my-group --topic my-topic --partition 0 --to-offset 1000
```

## Support

- GitHub Issues: https://github.com/lspecian/chronik-stream/issues

## License

Chronik Stream is licensed under the Apache License 2.0.