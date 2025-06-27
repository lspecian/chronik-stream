# Getting Started with Chronik Stream

## Prerequisites

- Rust 1.75+ (for building from source)
- Docker (for running pre-built images)
- 8GB RAM minimum
- 20GB disk space

## Quick Start with Docker Compose

The easiest way to get started is using Docker Compose:

```bash
# Clone the repository
git clone https://github.com/your-org/chronik-stream.git
cd chronik-stream

# Start all services
docker-compose up -d

# Check service health
docker-compose ps
```

This starts:
- 1 Controller node (port 9090)
- 1 Ingest node (port 9092)
- 1 Search node (port 9200)
- PostgreSQL (port 5432)
- Prometheus (port 9091)
- Grafana (port 3001)

## Building from Source

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/your-org/chronik-stream.git
cd chronik-stream
cargo build --release

# Run tests
cargo test
```

## Basic Usage

### 1. Creating a Topic

Using the Kafka protocol:

```python
from kafka import KafkaAdminClient
from kafka.admin import NewTopic

admin = KafkaAdminClient(
    bootstrap_servers=['localhost:9092'],
    client_id='admin'
)

topic = NewTopic(
    name='events',
    num_partitions=3,
    replication_factor=1
)

admin.create_topics([topic])
```

### 2. Producing Messages

```python
from kafka import KafkaProducer
import json

producer = KafkaProducer(
    bootstrap_servers=['localhost:9092'],
    value_serializer=lambda v: json.dumps(v).encode('utf-8')
)

# Send a message
producer.send('events', {
    'user_id': '123',
    'action': 'page_view',
    'page': '/products',
    'timestamp': '2024-01-15T10:30:00Z'
})

producer.flush()
```

### 3. Consuming Messages

```python
from kafka import KafkaConsumer
import json

consumer = KafkaConsumer(
    'events',
    bootstrap_servers=['localhost:9092'],
    auto_offset_reset='earliest',
    value_deserializer=lambda m: json.loads(m.decode('utf-8'))
)

for message in consumer:
    print(f"Received: {message.value}")
```

### 4. Searching Messages

The search API is Elasticsearch-compatible:

```bash
# Search for all messages containing "page_view"
curl -X POST "localhost:9200/events/_search" \
  -H 'Content-Type: application/json' \
  -d '{
    "query": {
      "match": {
        "action": "page_view"
      }
    }
  }'

# Get aggregations
curl -X POST "localhost:9200/events/_search" \
  -H 'Content-Type: application/json' \
  -d '{
    "size": 0,
    "aggs": {
      "actions": {
        "terms": {
          "field": "action.keyword",
          "size": 10
        }
      }
    }
  }'
```

## Configuration

### Environment Variables

#### Controller Node
- `NODE_ID`: Unique node identifier (default: 1)
- `RAFT_ADDR`: Raft listen address (default: 0.0.0.0:9090)
- `ADMIN_ADDR`: Admin API address (default: 0.0.0.0:8090)
- `DATA_DIR`: Data directory (default: /var/chronik/controller)

#### Ingest Node
- `BIND_ADDRESS`: Kafka protocol address (default: 0.0.0.0:9092)
- `STORAGE_PATH`: Local storage path (default: /var/chronik/ingest)
- `CONTROLLER_ADDRS`: Controller addresses (default: localhost:9090)
- `MAX_CONNECTIONS`: Max client connections (default: 10000)

#### Search Node
- `BIND_ADDRESS`: HTTP API address (default: 0.0.0.0:9200)
- `INDEX_DIR`: Index directory (default: /var/chronik/search)
- `CONTROLLER_ADDRS`: Controller addresses (default: localhost:9090)

### Storage Configuration

Configure object storage backend:

```bash
# S3 Storage
export OBJECT_STORE_TYPE=s3
export S3_BUCKET=my-chronik-bucket
export AWS_ACCESS_KEY_ID=your-key
export AWS_SECRET_ACCESS_KEY=your-secret

# GCS Storage
export OBJECT_STORE_TYPE=gcs
export GCS_BUCKET=my-chronik-bucket
export GOOGLE_APPLICATION_CREDENTIALS=/path/to/credentials.json

# Azure Storage
export OBJECT_STORE_TYPE=azure
export AZURE_CONTAINER=my-chronik-container
export AZURE_STORAGE_ACCOUNT=myaccount
export AZURE_STORAGE_ACCESS_KEY=your-key

# Local Storage (development)
export OBJECT_STORE_TYPE=local
export LOCAL_STORAGE_PATH=/var/chronik/segments
```

## Monitoring

### Prometheus Metrics

All components expose metrics at `/metrics`:

```bash
# Controller metrics
curl http://localhost:8090/metrics

# Ingest metrics
curl http://localhost:9093/metrics

# Search metrics
curl http://localhost:9201/metrics
```

### Grafana Dashboards

Pre-built dashboards are available at http://localhost:3001:
- Cluster Overview
- Ingest Performance
- Search Performance
- Storage Usage

### Health Checks

```bash
# Controller health
curl http://localhost:8090/health

# Search health
curl http://localhost:9200/health
```

## Client Libraries

### Python
```bash
pip install kafka-python elasticsearch
```

### Java
```xml
<dependency>
    <groupId>org.apache.kafka</groupId>
    <artifactId>kafka-clients</artifactId>
    <version>3.6.0</version>
</dependency>
```

### Node.js
```bash
npm install kafkajs @elastic/elasticsearch
```

## Common Operations

### Topic Management

```bash
# List topics
curl http://localhost:8090/api/topics

# Get topic details
curl http://localhost:8090/api/topics/events

# Update topic config
curl -X PUT http://localhost:8090/api/topics/events/config \
  -H 'Content-Type: application/json' \
  -d '{
    "retention.ms": "604800000",
    "compression.type": "snappy"
  }'
```

### Consumer Group Management

```bash
# List consumer groups
curl http://localhost:8090/api/groups

# Get group details
curl http://localhost:8090/api/groups/my-group

# Reset offsets
curl -X POST http://localhost:8090/api/groups/my-group/reset-offsets \
  -H 'Content-Type: application/json' \
  -d '{
    "topic": "events",
    "partition": 0,
    "offset": 0
  }'
```

## Troubleshooting

### Connection Issues

1. Check service is running:
   ```bash
   docker-compose ps
   # or
   systemctl status chronik-*
   ```

2. Check logs:
   ```bash
   docker-compose logs ingest
   # or
   journalctl -u chronik-ingest -f
   ```

3. Test connectivity:
   ```bash
   telnet localhost 9092
   ```

### Performance Issues

1. Check metrics:
   - Message lag
   - Storage usage
   - Memory usage

2. Common tuning:
   - Increase batch size
   - Adjust flush intervals
   - Scale horizontally

### Data Issues

1. Verify data integrity:
   ```bash
   curl http://localhost:8090/api/topics/events/verify
   ```

2. Check consumer offsets:
   ```bash
   curl http://localhost:8090/api/groups/my-group/offsets
   ```

## Next Steps

- Read the [Architecture Guide](architecture.md)
- Explore [Advanced Configuration](configuration.md)
- Check out [Example Applications](examples/)