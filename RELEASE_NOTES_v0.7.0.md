# Release Notes - v0.7.0

## Release Date: September 9, 2025

## Overview

This release addresses **critical Kafka client connectivity issues** that prevented standard Kafka clients from connecting to Chronik Stream when running in Docker or when bound to `0.0.0.0`. This fix restores full drop-in replacement capability for Apache Kafka.

## Critical Fix: Advertised Address Configuration

### The Problem
In v0.6.1, Chronik Stream would advertise `0.0.0.0:9092` as the broker address in metadata responses, which clients cannot connect to. This affected:
- All Docker deployments
- Any deployment binding to `0.0.0.0`
- Kubernetes deployments
- Multi-network environments

### The Solution
Added proper advertised address configuration, allowing the server to:
- Bind to one address (e.g., `0.0.0.0` for all interfaces)
- Advertise a different address to clients (e.g., `localhost`, hostname, or public IP)

### Impact
✅ **Full compatibility restored** with:
- kafka-python
- confluent-kafka-go (librdkafka v2.11.1+)
- Java Kafka clients
- KSQLDB
- Kafka UI
- All Kafka ecosystem tools

## New Features

### Configuration Options

#### CLI Arguments
- `--advertised-addr <ADDR>` - Address advertised to clients (defaults to bind address)
- `--advertised-port <PORT>` - Port advertised to clients (defaults to Kafka port)

#### Environment Variables
- `CHRONIK_ADVERTISED_ADDR` - Address advertised to clients
- `CHRONIK_ADVERTISED_PORT` - Port advertised to clients

## Usage Examples

### Docker
```bash
docker run -d -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  ghcr.io/lspecian/chronik-stream:v0.7.0
```

### Docker Compose
```yaml
services:
  chronik-stream:
    image: ghcr.io/lspecian/chronik-stream:v0.7.0
    ports:
      - "9092:9092"
    environment:
      CHRONIK_BIND_ADDR: "0.0.0.0"
      CHRONIK_ADVERTISED_ADDR: "kafka.example.com"
      CHRONIK_ADVERTISED_PORT: "9092"
```

### Kubernetes
```yaml
env:
- name: CHRONIK_ADVERTISED_ADDR
  value: "chronik-service.default.svc.cluster.local"
```

### Standalone
```bash
# Bind to all interfaces, advertise localhost
chronik-server --bind-addr 0.0.0.0 --advertised-addr localhost

# Use environment variables
export CHRONIK_ADVERTISED_ADDR=kafka.example.com
chronik-server
```

## Migration Guide

**No breaking changes!** This release is fully backward compatible.

### Upgrading from v0.6.1

1. **If using Docker**: Add `CHRONIK_ADVERTISED_ADDR` environment variable
2. **If binding to 0.0.0.0**: Use `--advertised-addr` with your hostname/IP
3. **If not specified**: Defaults to bind address (existing behavior)

## Testing

A test script is provided to verify connectivity:

```bash
pip install kafka-python
python test_advertised_address.py
```

## Files Changed

### Core Implementation
- `crates/chronik-server/src/main.rs` - Added advertised address configuration
- `crates/chronik-server/src/integrated_server.rs` - Use advertised address in broker registration

### Documentation
- `README.md` - Updated with configuration examples
- `docker-compose.yml` - Added advertised address configuration
- `docs/ADVERTISED_ADDRESS_FIX.md` - Comprehensive fix documentation
- `test_advertised_address.py` - Test script for verification

## Docker Images

```bash
# Pull the latest image
docker pull ghcr.io/lspecian/chronik-stream:v0.7.0
docker pull ghcr.io/lspecian/chronik-stream:latest

# Multi-architecture support
# Supports: linux/amd64, linux/arm64, darwin/amd64, darwin/arm64
```

## Verification

After starting with proper configuration, verify:

1. **Check server logs**:
   ```
   INFO   Advertised: kafka.example.com:9092
   ```

2. **Test client connection**:
   ```python
   from kafka import KafkaProducer
   producer = KafkaProducer(
       bootstrap_servers=['kafka.example.com:9092'],
       api_version=(0, 10, 0)
   )
   # Should connect without errors
   ```

## Known Issues

None at this time. All reported client compatibility issues have been resolved.

## What's Next

- v0.8.0: Multiple advertised listeners support (similar to Kafka's `advertised.listeners`)
- Enhanced monitoring and metrics
- Performance optimizations

## Contributors

Thanks to the customer who provided detailed bug reports and testing feedback that led to this critical fix.

## Upgrade Recommendation

**CRITICAL**: All users running Chronik Stream in Docker or binding to `0.0.0.0` should upgrade to v0.7.0 immediately and configure the advertised address appropriately.

---

For support or questions, please open an issue on GitHub.