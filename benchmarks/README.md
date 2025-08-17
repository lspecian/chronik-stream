# Chronik Stream Performance Benchmarks

This directory contains comprehensive performance benchmarks for Chronik Stream, including comparisons with Apache Kafka.

## Structure

```
benchmarks/
├── python/          # Python-based end-to-end benchmarks
├── rust/           # Rust criterion benchmarks for components
├── scripts/        # Benchmark automation scripts
└── results/        # Benchmark results and reports
```

## Quick Start

### Prerequisites

1. Install Python dependencies:
```bash
pip install -r python/requirements.txt
```

2. Ensure Chronik Stream is running:
```bash
docker-compose up -d
```

### Running Benchmarks

#### Basic Benchmark Suite

Run the full benchmark suite against Chronik Stream:

```bash
python python/benchmark_suite.py
```

#### Comparison with Apache Kafka

To compare Chronik Stream with Apache Kafka:

1. Start Apache Kafka on port 9095:
```bash
docker-compose -f tests/kafka-test-compose.yml up -d
```

2. Run the comparison script:
```bash
./scripts/compare_kafka.sh
```

#### Custom Benchmarks

Run specific benchmarks with custom parameters:

```bash
# High-throughput test
python python/benchmark_suite.py \
    --num-messages 10000000 \
    --message-size 100 \
    --compression snappy

# Latency-focused test
python python/benchmark_suite.py \
    --num-messages 100000 \
    --message-size 1024 \
    --num-partitions 1
```

## Benchmark Tests

### 1. Producer Throughput
- Measures maximum messages/second and MB/second
- Tests different message sizes and compression types
- Single producer performance

### 2. Consumer Throughput
- Measures consumption rate
- Tests different fetch sizes and configurations
- Single consumer performance

### 3. End-to-End Latency
- Measures time from produce to consume
- Reports min, avg, p50, p95, p99, and max latencies
- Uses synchronized producer/consumer

### 4. Concurrent Producers
- Tests scalability with multiple producers
- Measures aggregate and per-producer throughput
- Identifies bottlenecks

### 5. Consumer Groups
- Tests consumer group performance
- Measures partition assignment and rebalancing
- Multi-consumer scalability

## Rust Benchmarks

Build and run Rust benchmarks:

```bash
cd rust
cargo bench
```

This runs micro-benchmarks for:
- Protocol encoding/decoding
- Storage operations
- Compression algorithms
- Index operations

## Results

Results are saved in the `results/` directory with:
- JSON files with raw data
- Performance charts (throughput, latency distribution)
- Comparison summaries
- Timestamped for tracking over time

### Example Results

```
=== Performance Comparison ===

┌─────────────────┬──────────────────────┬─────────────────────┬────────────────────┬──────────────────┬──────────────────┐
│ System          │ Test                 │ Throughput (msg/s)  │ Throughput (MB/s)  │ Avg Latency (ms) │ P99 Latency (ms) │
├─────────────────┼──────────────────────┼─────────────────────┼────────────────────┼──────────────────┼──────────────────┤
│ chronik-stream  │ Producer Throughput  │ 1,234,567          │ 1,205.63          │ N/A              │ N/A              │
│ apache-kafka    │ Producer Throughput  │ 987,654            │ 964.31            │ N/A              │ N/A              │
│ chronik-stream  │ E2E Latency         │ N/A                │ N/A               │ 1.23             │ 2.45             │
│ apache-kafka    │ E2E Latency         │ N/A                │ N/A               │ 2.34             │ 4.56             │
└─────────────────┴──────────────────────┴─────────────────────┴────────────────────┴──────────────────┴──────────────────┘
```

## Performance Tuning

### Chronik Stream Configuration

Optimize performance by adjusting:

```yaml
# config/chronik.yaml
ingest:
  batch_size: 16384
  linger_ms: 10
  compression: snappy
  buffer_memory: 67108864  # 64MB

storage:
  segment_size: 268435456  # 256MB
  cache_size: 1073741824   # 1GB
```

### Benchmark Configuration

Adjust benchmark parameters in `BenchmarkConfig`:
- `num_messages`: Total messages to send
- `message_size`: Size of each message
- `batch_size`: Producer batch size
- `num_partitions`: Number of topic partitions
- `compression_type`: none, gzip, snappy, lz4, zstd

## Continuous Performance Testing

Add to CI/CD pipeline:

```yaml
# .github/workflows/performance.yml
- name: Run Performance Benchmarks
  run: |
    python benchmarks/python/benchmark_suite.py \
      --num-messages 100000 \
      --results-dir benchmarks/results/ci
    
- name: Check Performance Regression
  run: |
    python benchmarks/scripts/check_regression.py \
      --baseline benchmarks/results/baseline.json \
      --current benchmarks/results/ci/latest.json \
      --threshold 10  # Allow 10% degradation
```

## Troubleshooting

### Common Issues

1. **Connection Refused**
   - Ensure Chronik Stream is running
   - Check port availability (9092)

2. **Slow Performance**
   - Check system resources (CPU, memory, disk I/O)
   - Verify network connectivity
   - Review configuration settings

3. **Inconsistent Results**
   - Run multiple iterations
   - Ensure exclusive system access
   - Disable power management features

### Debug Mode

Enable detailed logging:

```bash
RUST_LOG=debug python python/benchmark_suite.py
```

## Contributing

When adding new benchmarks:

1. Follow existing patterns for consistency
2. Include both throughput and latency measurements
3. Add comparison with Apache Kafka where applicable
4. Document configuration parameters
5. Generate visual reports (charts)

## References

- [Apache Kafka Performance Tuning](https://kafka.apache.org/documentation/#performance)
- [Criterion.rs Documentation](https://bheisler.github.io/criterion.rs/book/)
- [kafka-python Performance Tips](https://kafka-python.readthedocs.io/en/master/usage.html#performance-tips)