# Getting Started with Chronik Stream

This guide will help you get Chronik Stream up and running quickly.

## Prerequisites

- Rust 1.70 or later
- PostgreSQL 14 or later
- Docker and Docker Compose (optional, for containerized deployment)
- Kubernetes cluster (optional, for Kubernetes deployment)

## Installation

### From Source

1. Clone the repository:
```bash
git clone https://github.com/chronik-stream/chronik-stream.git
cd chronik-stream
```

2. Build the project:
```bash
cargo build --release
```

3. Install the CLI tool:
```bash
cargo install --path crates/chronik-cli
```

### Using Docker

Pull the pre-built images:
```bash
docker pull chronik/controller:latest
docker pull chronik/ingest:latest
docker pull chronik/admin:latest
```

## Quick Start

### 1. Start PostgreSQL

Using Docker:
```bash
docker run -d \
  --name chronik-postgres \
  -e POSTGRES_PASSWORD=chronik \
  -e POSTGRES_DB=chronik \
  -p 5432:5432 \
  postgres:14
```

### 2. Run Database Migrations

```bash
export DATABASE_URL=postgres://postgres:chronik@localhost/chronik
sqlx migrate run
```

### 3. Start Controller Node

```bash
./target/release/chronik-controller \
  --node-id 1 \
  --listen-addr 0.0.0.0:9090 \
  --db-url postgres://postgres:chronik@localhost/chronik
```

### 4. Start Ingest Node

```bash
./target/release/chronik-ingest \
  --listen-addr 0.0.0.0:9092 \
  --controller-addr localhost:9090 \
  --storage-path /tmp/chronik-data
```

### 5. Start Admin API

```bash
./target/release/chronik-admin \
  --port 8080 \
  --db-url postgres://postgres:chronik@localhost/chronik \
  --controller-addr localhost:9090
```

## Using the CLI

### Authentication

First, login to get an access token:
```bash
chronik-ctl auth login -u admin -p admin
export CHRONIK_API_TOKEN="<token>"
```

### Topic Management

Create a topic:
```bash
chronik-ctl topic create my-events -p 6 -r 3
```

List topics:
```bash
chronik-ctl topic list
```

Get topic details:
```bash
chronik-ctl topic get my-events
```

### Producing Messages

Using Kafka command-line tools:
```bash
kafka-console-producer \
  --broker-list localhost:9092 \
  --topic my-events
```

Using kafkacat:
```bash
echo "Hello, Chronik!" | kafkacat -P -b localhost:9092 -t my-events
```

### Consuming Messages

Using Kafka command-line tools:
```bash
kafka-console-consumer \
  --bootstrap-server localhost:9092 \
  --topic my-events \
  --from-beginning
```

Using kafkacat:
```bash
kafkacat -C -b localhost:9092 -t my-events -o beginning
```

### Searching Messages

Search for messages containing specific text:
```bash
curl -X GET "http://localhost:8080/api/v1/search?q=hello&index=my-events"
```

## Docker Compose Setup

Create a `docker-compose.yml` file:

```yaml
version: '3.8'

services:
  postgres:
    image: postgres:14
    environment:
      POSTGRES_PASSWORD: chronik
      POSTGRES_DB: chronik
    volumes:
      - postgres_data:/var/lib/postgresql/data
    networks:
      - chronik

  controller:
    image: chronik/controller:latest
    environment:
      DATABASE_URL: postgres://postgres:chronik@postgres/chronik
      NODE_ID: 1
    depends_on:
      - postgres
    networks:
      - chronik
    ports:
      - "9090:9090"

  ingest:
    image: chronik/ingest:latest
    environment:
      CONTROLLER_ADDR: controller:9090
      STORAGE_PATH: /data
    volumes:
      - ingest_data:/data
    depends_on:
      - controller
    networks:
      - chronik
    ports:
      - "9092:9092"

  admin:
    image: chronik/admin:latest
    environment:
      DATABASE_URL: postgres://postgres:chronik@postgres/chronik
      CONTROLLER_ENDPOINTS: controller:9090
    depends_on:
      - controller
    networks:
      - chronik
    ports:
      - "8080:8080"

volumes:
  postgres_data:
  ingest_data:

networks:
  chronik:
```

Start the cluster:
```bash
docker-compose up -d
```

## Kubernetes Deployment

### Install the Operator

```bash
kubectl apply -f deploy/k8s/operator.yaml
```

### Create a Namespace

```bash
kubectl create namespace chronik-system
```

### Deploy PostgreSQL

```bash
kubectl apply -f deploy/k8s/postgres.yaml -n chronik-system
```

### Create a Chronik Cluster

Create a file `chronik-cluster.yaml`:

```yaml
apiVersion: chronik.stream/v1alpha1
kind: ChronikCluster
metadata:
  name: my-cluster
  namespace: chronik-system
spec:
  controllers: 3
  ingestNodes: 5
  storage:
    backend:
      local: {}
    size: 100Gi
  metastore:
    database: Postgres
    connection:
      host: postgres.chronik-system.svc.cluster.local
      port: 5432
      database: chronik
      credentialsSecret: postgres-credentials
```

Apply it:
```bash
kubectl apply -f chronik-cluster.yaml
```

### Access the Cluster

Port-forward to access the services:
```bash
# Admin API
kubectl port-forward -n chronik-system svc/my-cluster-admin 8080:8080

# Kafka endpoint
kubectl port-forward -n chronik-system svc/my-cluster-ingest 9092:9092
```

## Configuration Options

### Storage Backends

#### S3
```yaml
storage:
  backend: s3
  bucket: my-chronik-bucket
  region: us-east-1
  access_key: <access-key>
  secret_key: <secret-key>
```

#### Google Cloud Storage
```yaml
storage:
  backend: gcs
  bucket: my-chronik-bucket
  credentials_path: /path/to/credentials.json
```

#### Azure Blob Storage
```yaml
storage:
  backend: azure
  container: my-chronik-container
  account: myaccount
  access_key: <access-key>
```

### Security Configuration

Enable TLS:
```yaml
server:
  tls:
    cert_file: /path/to/cert.pem
    key_file: /path/to/key.pem
```

Enable authentication:
```yaml
auth:
  enabled: true
  mechanisms:
    - PLAIN
    - SCRAM-SHA-256
```

## Next Steps

- Read the [Architecture Guide](architecture.md) to understand how Chronik Stream works
- Check out the [API Reference](api-reference.md) for detailed API documentation
- See [Production Deployment](production.md) for production best practices
- Join our [Community](https://github.com/chronik-stream/chronik-stream/discussions) for help and discussions