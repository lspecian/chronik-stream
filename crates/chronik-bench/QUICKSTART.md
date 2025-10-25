# Chronik Bench - Quick Start Guide

## Installation

```bash
# Build the benchmark tool
cd /Users/lspecian/Development/chronik-stream
cargo build --release --package chronik-bench

# Binary location
./target/release/chronik-bench
```

## 5-Minute Quick Start

### 1. Start Chronik Server

```bash
# Terminal 1: Start Chronik in standalone mode
cd /Users/lspecian/Development/chronik-stream
CHRONIK_ADVERTISED_ADDR=localhost ./target/release/chronik-server standalone
```

### 2. Run Benchmark

```bash
# Terminal 2: Run benchmark
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic quickstart-test \
  --concurrency 32 \
  --message-size 1024 \
  --duration 60s \
  --csv-output results.csv
```

### 3. View Results

Results are displayed in the console and saved to `results.csv`.

## Common Use Cases

### Case 1: Maximum Throughput Test

**Goal**: Find maximum messages/sec Chronik can handle

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
  --csv-output max-throughput.csv
```

**What to look for**:
- Messages per second (msg/s)
- Bandwidth (MB/s)
- p99 latency (should stay < 100ms even at max throughput)

### Case 2: Minimum Latency Test

**Goal**: Find minimum p99 latency

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
  --csv-output min-latency.csv
```

**What to look for**:
- p50 latency (should be < 5ms)
- p99 latency (should be < 20ms for low-latency profile)
- p99.9 latency (tail behavior)

### Case 3: Stress Test

**Goal**: Sustained high load for stability testing

```bash
chronik-bench \
  --bootstrap-servers localhost:9092 \
  --topic stress-test \
  --concurrency 256 \
  --message-size 4096 \
  --duration 300s \
  --compression zstd \
  --csv-output stress-test.csv
```

**What to look for**:
- Consistent throughput over time
- No increase in latency over time
- Zero failed messages

### Case 4: Profile Comparison

**Goal**: Compare Chronik produce profiles

```bash
# Test low-latency profile
CHRONIK_PRODUCE_PROFILE=low-latency \
  ./target/release/chronik-server standalone &
sleep 5

chronik-bench \
  --topic profile-low-latency \
  --concurrency 32 \
  --duration 60s \
  --csv-output profile-low-latency.csv

kill %1
sleep 2

# Test high-throughput profile
CHRONIK_PRODUCE_PROFILE=high-throughput \
  ./target/release/chronik-server standalone &
sleep 5

chronik-bench \
  --topic profile-high-throughput \
  --concurrency 32 \
  --duration 60s \
  --csv-output profile-high-throughput.csv

kill %1

# Compare results
diff profile-low-latency.csv profile-high-throughput.csv
```

## Quick Commands Reference

```bash
# View help
chronik-bench --help

# Test for 30 seconds
chronik-bench --duration 30s

# Use 64 concurrent producers
chronik-bench --concurrency 64

# 2KB messages
chronik-bench --message-size 2048

# With Snappy compression
chronik-bench --compression snappy

# Save CSV output
chronik-bench --csv-output results.csv

# Save JSON output
chronik-bench --json-output results.json

# Rate limit to 10K msg/s
chronik-bench --rate-limit 10000

# Exactly 1 million messages
chronik-bench --message-count 1000000 --duration 0

# With Prometheus metrics on port 9091
chronik-bench --prometheus-port 9091
```

## Interpreting Results

### Console Output Explained

```
╔══════════════════════════════════════════════════════════════╗
║            Chronik Benchmark Results                        ║
╠══════════════════════════════════════════════════════════════╣
║ Mode:             Produce                  ← Test type
║ Duration:         60.00s                   ← Actual duration
║ Concurrency:      64                       ← Parallel tasks
║ Compression:      Snappy                   ← Compression used
╠══════════════════════════════════════════════════════════════╣
║ THROUGHPUT                                                   ║
╠══════════════════════════════════════════════════════════════╣
║ Messages:         1,234,567 total          ← Total messages sent
║ Failed:                   0 (0.00%)        ← Errors (should be 0%)
║ Data transferred: 1.18 GB                  ← Total bytes
║ Message rate:        20,576 msg/s          ← Messages per second
║ Bandwidth:           20.11 MB/s            ← Megabytes per second
╠══════════════════════════════════════════════════════════════╣
║ LATENCY (microseconds → milliseconds)                       ║
╠══════════════════════════════════════════════════════════════╣
║ p50:                  1,234 μs  (    1.23 ms) ← Median
║ p90:                  2,456 μs  (    2.46 ms) ← 90th percentile
║ p95:                  3,567 μs  (    3.57 ms) ← 95th percentile
║ p99:                  5,678 μs  (    5.68 ms) ← 99th percentile (key metric!)
║ p99.9:                8,901 μs  (    8.90 ms) ← Tail latency
║ max:                 12,345 μs  (   12.35 ms) ← Worst case
╚══════════════════════════════════════════════════════════════╝
```

### What's Good Performance?

**Throughput**:
- ✅ **Good**: > 50,000 msg/s (1KB messages, 64 concurrency)
- ⚠️ **OK**: 20,000 - 50,000 msg/s
- ❌ **Poor**: < 20,000 msg/s

**Latency (produce)**:
- ✅ **Good**: p99 < 20ms (low-latency profile)
- ✅ **Good**: p99 < 150ms (balanced profile)
- ⚠️ **OK**: p99 < 500ms (high-throughput profile)
- ❌ **Poor**: p99 > 1000ms (investigate!)

**Success Rate**:
- ✅ **Good**: 100% (0% failures)
- ❌ **Poor**: < 100% (investigate errors)

## Troubleshooting

### "Connection refused" error

**Problem**: Can't connect to Chronik server

**Solution**:
```bash
# 1. Check if Chronik is running
lsof -i :9092

# 2. If not, start it
CHRONIK_ADVERTISED_ADDR=localhost \
  ./target/release/chronik-server standalone

# 3. Wait 5 seconds, then retry benchmark
```

### "All brokers down" error

**Problem**: Chronik advertised address misconfigured

**Solution**:
```bash
# Always set CHRONIK_ADVERTISED_ADDR
CHRONIK_ADVERTISED_ADDR=localhost \
  ./target/release/chronik-server standalone
```

### Very high latency (p99 > 1 second)

**Problem**: Wrong produce profile for workload

**Solution**:
```bash
# For latency-sensitive apps, use low-latency profile
CHRONIK_PRODUCE_PROFILE=low-latency \
  ./target/release/chronik-server standalone

# Then re-run benchmark
```

### Low throughput (< 10K msg/s)

**Possible causes**:
1. **Low concurrency**: Increase `--concurrency` to 64+
2. **Small batches**: Increase `--batch-size` to 65536
3. **No compression**: Add `--compression snappy`
4. **Immediate flush**: Increase `--linger-ms` to 10

**Example fix**:
```bash
chronik-bench \
  --concurrency 128 \
  --batch-size 65536 \
  --linger-ms 10 \
  --compression snappy
```

## Advanced Usage

### Prometheus Monitoring

**Terminal 1**: Start Chronik
```bash
CHRONIK_ADVERTISED_ADDR=localhost \
  ./target/release/chronik-server standalone
```

**Terminal 2**: Start benchmark with Prometheus
```bash
# Requires --features prometheus build
cargo build --release --package chronik-bench --features prometheus

./target/release/chronik-bench \
  --prometheus-port 9091 \
  --duration 300s
```

**Terminal 3**: Query metrics
```bash
# View all metrics
curl http://localhost:9091/metrics

# Just message count
curl -s http://localhost:9091/metrics | grep chronik_bench_messages_sent_total

# Just p99 latency
curl -s http://localhost:9091/metrics | grep chronik_bench_latency_p99
```

### CSV Analysis with Python

```python
import pandas as pd
import matplotlib.pyplot as plt

# Load results
df = pd.read_csv('results.csv')

# Plot latency percentiles
df[['latency_p50_ms', 'latency_p90_ms', 'latency_p99_ms']].plot(kind='bar')
plt.ylabel('Latency (ms)')
plt.title('Chronik Latency Distribution')
plt.show()

# Print summary
print(f"Throughput: {df['throughput_msg_per_sec'].iloc[0]:,.0f} msg/s")
print(f"p99 Latency: {df['latency_p99_ms'].iloc[0]:.2f} ms")
```

### Grafana Integration

1. **Import CSV to Grafana**:
   - Install CSV plugin
   - Create dashboard
   - Add CSV data source pointing to `results.csv`

2. **Create panels**:
   - Time series: Throughput over time
   - Gauge: Current p99 latency
   - Stat: Total messages processed

## Tips & Best Practices

### 1. Always Warmup

**Bad**:
```bash
chronik-bench --warmup-duration 0s  # ❌ Cold start affects results
```

**Good**:
```bash
chronik-bench --warmup-duration 5s  # ✅ JIT warmup, connections established
```

### 2. Run Long Enough

**Bad**:
```bash
chronik-bench --duration 10s  # ❌ Too short, not statistically significant
```

**Good**:
```bash
chronik-bench --duration 60s  # ✅ Enough samples for accurate percentiles
```

### 3. Isolate Tests

**Bad**:
```bash
# Multiple benchmarks on same topic
chronik-bench --topic shared &
chronik-bench --topic shared &  # ❌ Interfere with each other
```

**Good**:
```bash
# Separate topics
chronik-bench --topic test1
chronik-bench --topic test2  # ✅ Isolated
```

### 4. Save Results

**Bad**:
```bash
chronik-bench  # ❌ Results lost after test
```

**Good**:
```bash
chronik-bench \
  --csv-output results-$(date +%Y%m%d-%H%M%S).csv \
  --json-output results-$(date +%Y%m%d-%H%M%S).json
# ✅ Timestamped results for comparison
```

## Next Steps

1. **Read full docs**: `crates/chronik-bench/README.md`
2. **Run test suite**: `cd crates/chronik-bench && ./test_benchmark.sh`
3. **Compare with Kafka**: Benchmark Kafka on same hardware
4. **Tune Chronik**: Adjust profiles based on benchmark results
5. **Report issues**: File bugs if benchmarks reveal problems

## Quick Links

- **Full README**: [crates/chronik-bench/README.md](README.md)
- **Implementation details**: [crates/chronik-bench/SUMMARY.md](SUMMARY.md)
- **Chronik docs**: [CLAUDE.md](../../CLAUDE.md)
- **Chronik server**: `./target/release/chronik-server`

---

**Questions?** Check the README or run `chronik-bench --help`
