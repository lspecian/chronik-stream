# Chronik Ingest Node

Production-ready TCP server that accepts Kafka client connections and handles the Kafka wire protocol.

## Features

### Core Server Features
- **High-performance async I/O** using Tokio
- **Connection lifecycle management** with proper cleanup
- **Frame-based protocol handling** using the Kafka wire protocol codec
- **Request routing** to appropriate handlers (produce, fetch, metadata, etc.)
- **Graceful shutdown** with configurable timeout
- **Comprehensive error handling** and recovery

### Connection Management
- **Connection pooling** with configurable limits
- **Per-IP connection limits** to prevent resource exhaustion
- **Rate limiting** with automatic IP banning for violations
- **Idle connection cleanup** with configurable timeout
- **Backpressure handling** to prevent server overload

### Security
- **TLS/SSL support** for encrypted connections
- **Client certificate authentication** (optional)
- **Configurable cipher suites** and TLS versions

### Monitoring & Observability
- **Prometheus metrics** for all key operations
- **Connection metrics**: total, active, rejected
- **Request metrics**: count, errors, timeouts, duration
- **Data transfer metrics**: bytes sent/received
- **Backpressure events** and frame errors
- **Structured logging** with tracing

### Performance
- **Zero-copy frame parsing** where possible
- **Efficient buffer management** with configurable sizes
- **Concurrent request handling** per connection
- **TCP_NODELAY** for low latency
- **Handles 10,000+ concurrent connections**

## Configuration

The server is configured through environment variables:

```bash
# Server binding
BIND_ADDRESS=0.0.0.0:9092

# Connection limits
MAX_CONNECTIONS=10000
MAX_CONNECTIONS_PER_IP=100
CONNECTION_RATE_LIMIT=10
RATE_LIMIT_BAN_SECS=300

# Timeouts
REQUEST_TIMEOUT_SECS=30
IDLE_TIMEOUT_SECS=600

# Backpressure
BACKPRESSURE_THRESHOLD=1000

# TLS Configuration (optional)
TLS_CERT_PATH=/path/to/cert.pem
TLS_KEY_PATH=/path/to/key.pem
TLS_CA_PATH=/path/to/ca.pem
TLS_REQUIRE_CLIENT_CERT=false

# Storage
STORAGE_PATH=/var/lib/chronik-data

# Logging
RUST_LOG=chronik_ingest=info,chronik_protocol=debug
```

## Usage

### Basic Server

```rust
use chronik_ingest::{IngestServer, ServerConfig};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig::default();
    let data_dir = PathBuf::from("/var/lib/chronik-data");
    
    let server = IngestServer::new(config, data_dir).await?;
    let handle = server.start().await?;
    
    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    
    // Graceful shutdown
    server.stop().await?;
    handle.await??;
    
    Ok(())
}
```

### TLS Server

```rust
use chronik_ingest::{IngestServer, ServerConfig, TlsConfig};

let tls_config = TlsConfig {
    cert_path: "/path/to/cert.pem".into(),
    key_path: "/path/to/key.pem".into(),
    ca_path: Some("/path/to/ca.pem".into()),
    require_client_cert: true,
};

let mut config = ServerConfig::default();
config.tls_config = Some(tls_config);

let server = IngestServer::new(config, data_dir).await?;
```

### Custom Configuration

```rust
use chronik_ingest::{ServerConfig, ConnectionPoolConfig};
use std::time::Duration;

let config = ServerConfig {
    listen_addr: "0.0.0.0:9092".parse()?,
    max_connections: 5000,
    request_timeout: Duration::from_secs(60),
    buffer_size: 50 * 1024 * 1024, // 50MB
    idle_timeout: Duration::from_secs(300),
    tls_config: None,
    pool_config: ConnectionPoolConfig {
        max_per_ip: 50,
        rate_limit: 20,
        ban_duration: Duration::from_secs(600),
    },
    backpressure_threshold: 500,
    shutdown_timeout: Duration::from_secs(30),
    metrics_interval: Duration::from_secs(60),
};
```

## Metrics

The server exposes Prometheus metrics on the monitoring port:

### Connection Metrics
- `chronik_connections_total`: Total connections accepted
- `chronik_connections_active`: Currently active connections
- `chronik_connections_rejected`: Connections rejected due to limits

### Request Metrics
- `chronik_requests_total`: Total requests processed
- `chronik_request_errors`: Request processing errors
- `chronik_request_timeouts`: Request timeouts
- `chronik_request_duration`: Request processing duration histogram

### Data Transfer Metrics
- `chronik_bytes_received`: Total bytes received
- `chronik_bytes_sent`: Total bytes sent

### System Metrics
- `chronik_pending_requests`: Pending requests across all connections
- `chronik_backpressure_events`: Backpressure events triggered
- `chronik_frame_errors`: Protocol frame parsing errors
- `chronik_tls_handshake_failures`: TLS handshake failures

## Testing

### Unit Tests
```bash
cargo test -p chronik-ingest
```

### Integration Tests
```bash
cargo test -p chronik-ingest --test server_integration_test
```

### Performance Benchmarks
```bash
cargo bench -p chronik-ingest
```

### Load Testing with Kafka Tools
```bash
# Test with kafka-console-producer
kafka-console-producer.sh \
  --broker-list localhost:9092 \
  --topic test-topic

# Test with kafka-producer-perf-test
kafka-producer-perf-test.sh \
  --topic test-topic \
  --num-records 1000000 \
  --record-size 1024 \
  --throughput 10000 \
  --producer-props bootstrap.servers=localhost:9092
```

## Architecture

### Connection Flow
1. TCP connection accepted by main server loop
2. Rate limiting and connection limit checks
3. TLS handshake (if configured)
4. Connection tracked in server state
5. Framed codec created for Kafka protocol
6. Request loop handles incoming frames
7. Requests routed to appropriate handlers
8. Responses sent back through framed codec
9. Connection cleanup on disconnect

### Request Processing
1. Frame received and decoded
2. Backpressure check
3. Request parsed and routed by API key
4. Handler processes request asynchronously
5. Response encoded and sent
6. Metrics updated

### Graceful Shutdown
1. Stop accepting new connections
2. Send shutdown signal to all connections
3. Wait for pending requests to complete
4. Force close after timeout
5. Cleanup resources

## Performance Considerations

- **Buffer Sizes**: Adjust `buffer_size` based on your typical message sizes
- **Connection Limits**: Set `max_connections` based on available memory
- **Timeouts**: Balance between client patience and resource usage
- **Backpressure**: Tune `backpressure_threshold` to prevent overload
- **TLS**: Expect 10-20% overhead with TLS enabled

## Troubleshooting

### High Memory Usage
- Reduce `max_connections` or `buffer_size`
- Enable more aggressive `idle_timeout`
- Check for memory leaks with profiling tools

### Connection Rejections
- Increase `max_connections` or `max_per_ip`
- Check rate limiting settings
- Monitor with metrics

### Performance Issues
- Enable `TCP_NODELAY` (already set)
- Increase `backpressure_threshold`
- Profile with flamegraph
- Check network latency

### TLS Issues
- Verify certificate paths and permissions
- Check certificate validity and chain
- Enable debug logging for TLS handshakes
- Test with openssl s_client