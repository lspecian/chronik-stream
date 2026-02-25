# Chronik Bench

High-performance benchmark harness for the Chronik streaming platform.

## Features

- **Multi-mode benchmarking**: Produce, consume, round-trip, and metadata operations
- **Latency tracking**: HDR histogram-based latency measurement with percentiles (p50-p99.9)
- **High concurrency**: Configurable concurrent producer/consumer tasks
- **Flexible configuration**: CLI arguments with environment variable support
- **Multiple output formats**: Console, CSV, JSON
- **Prometheus metrics** (optional): Real-time metrics export
- **Rate limiting**: Control message throughput
- **Compression support**: None, Gzip, Snappy, LZ4, Zstd
- **Message patterns**: Random, sequential, fixed keys with various payload types

## Installation

Build the benchmark tool:

```bash
# Standard build
cargo build --release --bin chronik-bench

# Build with Prometheus support
cargo build --release --bin chronik-bench --features prometheus
```

The compiled binary will be at `../../target/release/chronik-bench`.

## Quick Start

### 1. Start Chronik Server

```bash
# Start Chronik in standalone mode
cd ../..
CHRONIK_ADVERTISED_ADDR=localhost \
  ./target/release/chronik-server standalone
```

### 2. Run Basic Benchmark

```bash
# Produce benchmark (default)
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic bench-test \
  --concurrency 64 \
  --message-size 1024 \
  --duration 60s

# Consume benchmark
./target/release/chronik-bench \
  --mode consume \
  --bootstrap-servers localhost:9092 \
  --topic bench-test \
  --duration 60s
```

## Usage

```
chronik-bench [OPTIONS]

OPTIONS:
  -b, --bootstrap-servers <SERVERS>
          Kafka bootstrap servers [default: localhost:9092]

  -t, --topic <TOPIC>
          Topic name [default: chronik-bench]

  -c, --concurrency <N>
          Number of concurrent producer tasks [default: 64]

  -s, --message-size <BYTES>
          Message size in bytes [default: 1024]

  -d, --duration <DURATION>
          Benchmark duration (e.g., "60s", "5m", "1h") [default: 60s]

  -w, --warmup-duration <DURATION>
          Warmup duration [default: 5s]

  -m, --mode <MODE>
          Benchmark mode [default: produce]
          [possible values: produce, consume, round-trip, metadata]

      --compression <TYPE>
          Compression codec [default: none]
          [possible values: none, gzip, snappy, lz4, zstd]

  -p, --partitions <N>
          Number of partitions [default: 1]

  -r, --replication-factor <N>
          Replication factor [default: 1]

      --acks <ACKS>
          Producer acks (0, 1, or -1) [default: 1]

      --linger-ms <MS>
          Producer linger time [default: 0]

      --batch-size <BYTES>
          Producer batch size [default: 16384]

      --request-timeout-ms <MS>
          Request timeout [default: 30000]

      --message-timeout-ms <MS>
          Message send timeout [default: 60000]

      --report-interval-secs <SECS>
          Periodic report interval [default: 5]

      --csv-output <PATH>
          CSV output file path

      --json-output <PATH>
          JSON output file path

      --prometheus-port <PORT>
          Prometheus metrics port (requires --features prometheus)

  -v, --verbose
          Enable verbose logging

      --consumer-group <GROUP>
          Consumer group ID [default: chronik-bench-consumer]

  -n, --message-count <N>
          Number of messages to produce (0 = unlimited) [default: 0]

      --rate-limit <MSG_PER_SEC>
          Target throughput (0 = unlimited) [default: 0]

      --key-pattern <PATTERN>
          Key pattern [default: random]
          [possible values: random, sequential, fixed]

      --payload-pattern <PATTERN>
          Payload pattern [default: random]
          [possible values: random, zeros, text]

      --create-topic
          Create topic before benchmark

      --delete-topic-after
          Delete topic after benchmark

      --validate
          Validate message correctness

  -h, --help
          Print help information

  -V, --version
          Print version information
```

## Examples

### Example 1: High-Throughput Test

Test maximum throughput with large batches and compression:

```bash
chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic throughput-test \
  --concurrency 128 \
  --message-size 1024 \
  --duration 120s \
  --compression snappy \
  --batch-size 65536 \
  --linger-ms 10 \
  --csv-output throughput-results.csv
```

### Example 2: Low-Latency Test

Test latency with small messages and immediate flush:

```bash
chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic latency-test \
  --concurrency 1 \
  --message-size 128 \
  --duration 60s \
  --compression none \
  --batch-size 1 \
  --linger-ms 0 \
  --acks 1 \
  --csv-output latency-results.csv
```

### Example 3: Stress Test with Prometheus Metrics

Run stress test with real-time Prometheus monitoring:

```bash
# Build with Prometheus support first
cargo build --release --bin chronik-bench --features prometheus

# Run benchmark
chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic stress-test \
  --concurrency 256 \
  --message-size 4096 \
  --duration 300s \
  --compression zstd \
  --prometheus-port 9091 \
  --csv-output stress-results.csv

# In another terminal, check metrics:
curl http://localhost:9091/metrics
```

### Example 4: Multi-Partition Load Test

Create topic with multiple partitions and distribute load:

```bash
chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic multi-partition-test \
  --create-topic \
  --partitions 16 \
  --replication-factor 1 \
  --concurrency 64 \
  --message-size 2048 \
  --duration 180s \
  --key-pattern sequential \
  --csv-output multi-partition-results.csv
```

### Example 5: Rate-Limited Test

Test specific throughput target:

```bash
chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic rate-limited-test \
  --concurrency 10 \
  --message-size 512 \
  --duration 60s \
  --rate-limit 10000 \
  --csv-output rate-limited-results.csv
```

### Example 6: Fixed Message Count

Produce exactly N messages (ignoring duration):

```bash
chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic fixed-count-test \
  --concurrency 32 \
  --message-size 1024 \
  --message-count 1000000 \
  --csv-output fixed-count-results.csv
```

## Output Formats

### Console Output

```
╔══════════════════════════════════════════════════════════════╗
║            Chronik Benchmark Results                        ║
╠══════════════════════════════════════════════════════════════╣
║ Mode:             Produce
║ Duration:         60.00s
║ Concurrency:      64
║ Compression:      Snappy
╠══════════════════════════════════════════════════════════════╣
║ THROUGHPUT                                                   ║
╠══════════════════════════════════════════════════════════════╣
║ Messages:         1,234,567 total
║ Failed:                   0 (0.00%)
║ Data transferred: 1.18 GB
║ Message rate:        20,576 msg/s
║ Bandwidth:           20.11 MB/s
╠══════════════════════════════════════════════════════════════╣
║ LATENCY (microseconds → milliseconds)                       ║
╠══════════════════════════════════════════════════════════════╣
║ p50:                  1,234 μs  (    1.23 ms)
║ p90:                  2,456 μs  (    2.46 ms)
║ p95:                  3,567 μs  (    3.57 ms)
║ p99:                  5,678 μs  (    5.68 ms)
║ p99.9:                8,901 μs  (    8.90 ms)
║ max:                 12,345 μs  (   12.35 ms)
╚══════════════════════════════════════════════════════════════╝
```

### CSV Output

Results are written to a CSV file with the following columns:

- `timestamp`: ISO 8601 timestamp
- `mode`: Benchmark mode (produce, consume, etc.)
- `duration_secs`: Test duration in seconds
- `total_messages`: Total messages processed
- `failed_messages`: Failed message count
- `total_bytes`: Total bytes transferred
- `throughput_msg_per_sec`: Messages per second
- `throughput_mb_per_sec`: Megabytes per second
- `success_rate_pct`: Success rate percentage
- `latency_p50_us`: p50 latency in microseconds
- `latency_p90_us`: p90 latency in microseconds
- `latency_p95_us`: p95 latency in microseconds
- `latency_p99_us`: p99 latency in microseconds
- `latency_p999_us`: p99.9 latency in microseconds
- `latency_max_us`: Maximum latency in microseconds
- `latency_p50_ms`: p50 latency in milliseconds
- `latency_p90_ms`: p90 latency in milliseconds
- `latency_p95_ms`: p95 latency in milliseconds
- `latency_p99_ms`: p99 latency in milliseconds
- `latency_p999_ms`: p99.9 latency in milliseconds
- `latency_max_ms`: Maximum latency in milliseconds
- `message_size`: Message size in bytes
- `concurrency`: Concurrency level
- `compression`: Compression codec

### JSON Output

Results can also be exported to JSON for programmatic processing:

```json
{
  "mode": "Produce",
  "duration": {
    "secs": 60,
    "nanos": 123456789
  },
  "total_messages": 1234567,
  "failed_messages": 0,
  "total_bytes": 1268677632,
  "latency_p50_us": 1234,
  "latency_p90_us": 2456,
  "latency_p95_us": 3567,
  "latency_p99_us": 5678,
  "latency_p999_us": 8901,
  "latency_max_us": 12345,
  "message_size": 1024,
  "concurrency": 64,
  "compression": "Snappy"
}
```

## Prometheus Metrics

When built with `--features prometheus`, the benchmark exposes the following metrics on the specified port:

- `chronik_bench_messages_sent_total`: Total messages sent (counter)
- `chronik_bench_messages_failed_total`: Total messages failed (counter)
- `chronik_bench_bytes_sent_total`: Total bytes sent (counter)
- `chronik_bench_latency_p50_microseconds`: p50 latency (gauge)
- `chronik_bench_latency_p90_microseconds`: p90 latency (gauge)
- `chronik_bench_latency_p99_microseconds`: p99 latency (gauge)
- `chronik_bench_latency_p999_microseconds`: p99.9 latency (gauge)
- `chronik_bench_throughput_msg_per_sec`: Messages per second (gauge)
- `chronik_bench_throughput_mb_per_sec`: Megabytes per second (gauge)

Access metrics at `http://localhost:<prometheus-port>/metrics`

## Environment Variables

All CLI options can be set via environment variables:

- `KAFKA_BOOTSTRAP_SERVERS`: Bootstrap servers
- `KAFKA_TOPIC`: Topic name
- `RUST_LOG`: Logging level (e.g., `debug`, `info`, `warn`, `error`)

Example:

```bash
export KAFKA_BOOTSTRAP_SERVERS=localhost:9092
export KAFKA_TOPIC=my-test-topic
export RUST_LOG=info

chronik-bench --concurrency 64 --duration 60s
```

## Performance Tips

### Maximize Throughput

1. **Increase concurrency**: More parallel producers
   ```bash
   --concurrency 128
   ```

2. **Enable compression**: Reduce network bandwidth
   ```bash
   --compression snappy
   ```

3. **Increase batch size**: Amortize overhead
   ```bash
   --batch-size 65536 --linger-ms 10
   ```

4. **Disable acks for testing**: (not recommended for production)
   ```bash
   --acks 0
   ```

### Minimize Latency

1. **Reduce concurrency**: Less contention
   ```bash
   --concurrency 1
   ```

2. **Disable compression**: Avoid CPU overhead
   ```bash
   --compression none
   ```

3. **Small batches**: Immediate sends
   ```bash
   --batch-size 1 --linger-ms 0
   ```

4. **Synchronous acks**: Ensure durability
   ```bash
   --acks 1
   ```

## Comparing with Kafka/Redpanda

To benchmark against standard Kafka or Redpanda:

```bash
# Benchmark Chronik
chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic chronik-test \
  --concurrency 64 \
  --duration 60s \
  --csv-output chronik-results.csv

# Benchmark Kafka (on port 9093)
chronik-bench \
  --bootstrap-servers localhost:9093 \
  --topic kafka-test \
  --concurrency 64 \
  --duration 60s \
  --csv-output kafka-results.csv

# Compare results
diff chronik-results.csv kafka-results.csv
```

## Troubleshooting

### Connection Refused

Ensure Chronik server is running and `CHRONIK_ADVERTISED_ADDR` is set correctly:

```bash
CHRONIK_ADVERTISED_ADDR=localhost ./target/release/chronik-server standalone
```

### Timeouts

Increase timeout values if network is slow:

```bash
chronik-bench \
  --request-timeout-ms 60000 \
  --message-timeout-ms 120000
```

### High Latency

Check Chronik produce profile:

```bash
# Low-latency profile (< 20ms p99)
CHRONIK_PRODUCE_PROFILE=low-latency ./target/release/chronik-server standalone

# Then benchmark
chronik-bench --concurrency 1 --message-size 128
```

### Out of Memory

Reduce concurrency or message size:

```bash
chronik-bench --concurrency 16 --message-size 512
```

## Development

### Run Tests

```bash
cargo test --package chronik-bench
```

### Run with Debug Logging

```bash
RUST_LOG=debug chronik-bench --concurrency 1 --duration 10s
```

### Build with Optimizations

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release --bin chronik-bench
```

## License

Apache-2.0
