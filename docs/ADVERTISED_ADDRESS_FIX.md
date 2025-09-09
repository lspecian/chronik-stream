# Advertised Address Configuration Fix

## Summary

This document describes the fix implemented to resolve Kafka client connectivity issues caused by incorrect advertised address configuration in Chronik Stream v0.6.1.

## Issue Description

Clients were unable to connect to Chronik Stream when running in Docker or when the server was bound to `0.0.0.0`. The server would advertise `0.0.0.0:9092` as the broker address, which clients cannot connect to.

## Root Cause

The server was using the bind address (`--bind-addr` or `CHRONIK_BIND_ADDR`) for both:
1. Binding the server socket (correct)
2. Advertising to clients in metadata responses (incorrect)

When binding to `0.0.0.0` (all interfaces), the server would tell clients to connect to `0.0.0.0`, which is invalid.

## Solution Implemented

### 1. Added Advertised Address Configuration

Added new configuration options:
- CLI: `--advertised-addr` and `--advertised-port`
- Environment: `CHRONIK_ADVERTISED_ADDR` and `CHRONIK_ADVERTISED_PORT`

These options allow specifying the hostname/IP that clients should use to connect, separate from the bind address.

### 2. Updated Server Configuration

Modified `crates/chronik-server/src/main.rs` to:
- Parse advertised address configuration
- Default to bind address if not specified
- Warn if advertised address is `0.0.0.0`

### 3. Metadata Response Uses Advertised Address

The server now:
- Registers brokers with the advertised address in metadata store
- Returns the advertised address in Metadata API responses
- Ensures clients receive a resolvable address

## Configuration Examples

### Docker Deployment

```yaml
version: '3.8'
services:
  chronik-stream:
    image: ghcr.io/lspecian/chronik-stream:latest
    hostname: chronik-stream
    container_name: chronik-stream
    ports:
      - "9092:9092"
    environment:
      CHRONIK_BIND_ADDR: "0.0.0.0"            # Bind to all interfaces
      CHRONIK_ADVERTISED_ADDR: "chronik-stream" # Advertise container hostname
      CHRONIK_ADVERTISED_PORT: "9092"
      RUST_LOG: "info"
    networks:
      - kafka-network
```

### Docker with Host Network

```yaml
services:
  chronik-stream:
    image: ghcr.io/lspecian/chronik-stream:latest
    network_mode: host
    environment:
      CHRONIK_ADVERTISED_ADDR: "localhost"  # For local testing
      # Or use actual hostname/IP for production
      # CHRONIK_ADVERTISED_ADDR: "kafka.example.com"
```

### Standalone Server

```bash
# Local development - bind to all interfaces, advertise localhost
chronik-server --bind-addr 0.0.0.0 --advertised-addr localhost

# Production - advertise public hostname
chronik-server \
  --bind-addr 0.0.0.0 \
  --advertised-addr kafka.example.com \
  --advertised-port 9092

# Using environment variables
export CHRONIK_BIND_ADDR=0.0.0.0
export CHRONIK_ADVERTISED_ADDR=kafka.example.com
export CHRONIK_ADVERTISED_PORT=9092
chronik-server
```

### Kubernetes Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: chronik-stream
spec:
  template:
    spec:
      containers:
      - name: chronik-stream
        image: ghcr.io/lspecian/chronik-stream:latest
        env:
        - name: CHRONIK_BIND_ADDR
          value: "0.0.0.0"
        - name: CHRONIK_ADVERTISED_ADDR
          value: "chronik-stream-service.default.svc.cluster.local"
        - name: CHRONIK_ADVERTISED_PORT
          value: "9092"
```

## Testing the Fix

A test script is provided to verify the fix:

```bash
# Install dependencies
pip install kafka-python

# Run the test
python test_advertised_address.py
```

The test script will:
1. Connect to Chronik Stream
2. Send a test message
3. Consume the message
4. Report success or detailed error information

## Compatibility

This fix ensures compatibility with:
- kafka-python
- confluent-kafka-go (librdkafka)
- Java Kafka clients
- KSQLDB
- Kafka UI
- All standard Kafka ecosystem tools

## Migration from v0.6.1

No migration required. Simply update your configuration to specify the advertised address:

1. If using Docker, add `CHRONIK_ADVERTISED_ADDR` environment variable
2. If using CLI, add `--advertised-addr` argument
3. The server will default to the bind address if not specified (backward compatible)

## Best Practices

1. **Always set advertised address in Docker**: When running in Docker, always explicitly set the advertised address to the container name or service name.

2. **Use resolvable hostnames**: Ensure the advertised address is resolvable by clients. Use:
   - `localhost` for local development
   - Container/service names for Docker networks
   - Public DNS names for internet-facing deployments

3. **Avoid 0.0.0.0**: Never use `0.0.0.0` as the advertised address. The server will warn if this is detected.

4. **Match port mappings**: Ensure the advertised port matches the external port mapping in Docker/Kubernetes.

## Verification

After starting the server with proper advertised address configuration, verify:

1. Check server logs for the advertised address:
   ```
   INFO Integrated Kafka server initialized successfully
   INFO   Node ID: 1
   INFO   Advertised: kafka.example.com:9092
   ```

2. Test with a Kafka client:
   ```python
   from kafka import KafkaProducer
   producer = KafkaProducer(
       bootstrap_servers=['kafka.example.com:9092'],
       api_version=(0, 10, 0)
   )
   # Should connect without errors
   ```

## Related Issues

- Original bug report: Customer integration issues with v0.6.1
- Affects: All deployments where bind address differs from accessible address
- Fixed in: v0.7.0 (pending release)