# Chronik Benchmarking Tools Comparison

## Overview

Chronik has **TWO separate benchmark crates** with different purposes:

1. **`chronik-benchmarks`** - Internal micro-benchmarks (existing)
2. **`chronik-bench`** - External load testing tool (NEW - just created)

---

## 1. chronik-benchmarks (Existing)

**Location**: `crates/chronik-benchmarks/`
**Type**: Internal micro-benchmarking framework
**Purpose**: Unit-level performance testing of Chronik components

### What It Does

- **Internal component benchmarks** using Criterion.rs
- **Micro-benchmarks** for specific functions/modules
- **Development-time performance testing**
- **Regression detection** for code changes

### Key Features

```rust
// Example: Benchmarks individual Chronik components
[[bench]]
name = "produce_throughput"    // Benchmark ProduceHandler directly
harness = false

[[bench]]
name = "search_latency"        // Benchmark Search API directly
harness = false

[[bench]]
name = "metadata_performance"  // Benchmark metadata store
harness = false
```

### Architecture

```
chronik-benchmarks (Internal)
├── Uses Chronik crates directly (not Kafka protocol)
├── Tests individual components in isolation
├── Runs as Rust benchmark suite (cargo bench)
└── Output: Criterion HTML reports
```

### Usage

```bash
# Run internal benchmarks
cargo bench --package chronik-benchmarks

# Run specific benchmark
cargo bench --package chronik-benchmarks --bench produce_throughput

# Output: target/criterion/
```

### Scenarios

- Light workload (small messages, low concurrency)
- Medium workload (typical production)
- Heavy workload (stress testing)
- Kafka compatibility tests
- Elasticsearch compatibility tests
- Mixed workload tests
- Scalability tests (resource intensive)
- Reliability tests (special setup required)

### Dependencies

```toml
chronik-common = { path = "../chronik-common" }      # Internal
chronik-protocol = { path = "../chronik-protocol" }  # Internal
chronik-storage = { path = "../chronik-storage" }    # Internal
chronik-wal = { path = "../chronik-wal" }            # Internal
criterion = "0.5"                                     # Benchmarking framework
statistical = "1.0"                                   # Statistics
```

### Target Users

- **Chronik developers** optimizing specific components
- **CI/CD pipelines** catching performance regressions
- **Performance engineers** profiling hotspots

---

## 2. chronik-bench (NEW)

**Location**: `crates/chronik-bench/`
**Type**: External Kafka-compatible load testing tool
**Purpose**: End-to-end performance testing via Kafka protocol

### What It Does

- **Black-box testing** through Kafka wire protocol
- **Load testing** with configurable concurrency
- **Latency measurement** with HDR histograms
- **Throughput benchmarking** for capacity planning

### Key Features

```bash
# External Kafka client benchmark
chronik-bench \
  --bootstrap-servers localhost:9092 \     # Kafka protocol
  --concurrency 64 \                       # Concurrent producers
  --message-size 1024 \                    # Message payload size
  --duration 60s \                         # Test duration
  --compression snappy \                   # Compression codec
  --csv-output results.csv                 # Export results
```

### Architecture

```
chronik-bench (External)
├── Uses rdkafka (standard Kafka client)
├── Tests entire system end-to-end
├── Runs as standalone CLI tool
└── Output: CSV, JSON, Prometheus metrics
```

### Usage

```bash
# Build
cargo build --release --package chronik-bench

# Run benchmark
./target/release/chronik-bench \
  --bootstrap-servers localhost:9092 \
  --concurrency 32 \
  --duration 60s

# Output: Console, CSV files, JSON files
```

### Test Modes

- **Produce**: Producer-only benchmark (✅ Implemented)
- **Consume**: Consumer-only benchmark (⏸️ Planned)
- **Round-trip**: End-to-end latency (⏸️ Planned)
- **Metadata**: Admin operations (⏸️ Planned)

### Dependencies

```toml
rdkafka = "0.36"           # Standard Kafka client (external)
hdrhistogram = "7.5"       # Latency tracking
clap = "4.5"               # CLI parsing
csv = "1.3"                # CSV export
serde_json = "1.0"         # JSON export
prometheus = "0.13"        # Metrics (optional)
```

### Target Users

- **Operators** measuring production capacity
- **QA engineers** comparing Chronik vs Kafka
- **DevOps** doing capacity planning
- **Customers** validating Chronik performance

---

## Side-by-Side Comparison

| Aspect | chronik-benchmarks | chronik-bench |
|--------|-------------------|---------------|
| **Type** | Internal micro-benchmarks | External load testing |
| **Interface** | Rust API (direct crate access) | Kafka wire protocol (rdkafka) |
| **Scope** | Individual components | Entire system end-to-end |
| **Execution** | `cargo bench` | Standalone binary |
| **Framework** | Criterion.rs | Custom Tokio async |
| **Output** | HTML reports (Criterion) | CSV, JSON, Prometheus |
| **Latency** | Nano/microsecond (function calls) | Millisecond (network roundtrip) |
| **Workload** | Synthetic (in-process) | Realistic (client-server) |
| **Purpose** | Regression detection | Capacity planning |
| **Users** | Developers | Operators/Customers |
| **Setup** | No server needed | Requires running Chronik server |
| **Complexity** | Low (single process) | Medium (client + server) |
| **Realism** | Low (no network) | High (real Kafka clients) |

---

## When to Use Each

### Use `chronik-benchmarks` When:

✅ Optimizing a specific function or module
✅ Comparing alternative implementations
✅ Detecting performance regressions in CI
✅ Profiling CPU hotspots
✅ Testing component in isolation

**Example**:
```bash
# Benchmark WAL write performance
cargo bench --package chronik-benchmarks --bench wal_throughput
```

### Use `chronik-bench` When:

✅ Measuring overall system throughput
✅ Comparing Chronik vs Kafka performance
✅ Planning production capacity
✅ Validating SLA requirements
✅ Testing realistic client workloads

**Example**:
```bash
# Benchmark end-to-end producer performance
chronik-bench --concurrency 64 --duration 300s --csv-output capacity.csv
```

---

## Complementary Usage

**Best practice**: Use BOTH tools together

### Development Workflow

```
1. Code change to ProduceHandler
   ↓
2. Run chronik-benchmarks (micro-benchmark)
   - Detect regression: ProduceHandler is 10% slower
   ↓
3. Fix performance issue
   ↓
4. Run chronik-benchmarks again
   - Confirm fix: ProduceHandler back to baseline
   ↓
5. Run chronik-bench (end-to-end)
   - Validate: Overall throughput unchanged
   ↓
6. Commit with confidence
```

### Production Validation

```
1. Deploy new Chronik version
   ↓
2. Run chronik-bench (load test)
   - Measure: 15,000 msg/s @ p99 30ms
   ↓
3. Compare with Kafka baseline
   - Chronik: 15,000 msg/s
   - Kafka: 20,000 msg/s
   ↓
4. Identify gap
   ↓
5. Run chronik-benchmarks (component profiling)
   - Find: WAL commit is bottleneck
   ↓
6. Optimize WAL
   ↓
7. Re-test with chronik-bench
   - New: 18,000 msg/s (closer to Kafka)
```

---

## Migration Path

### Current State

- ✅ `chronik-benchmarks` exists (light usage)
- ✅ `chronik-bench` created (fully functional)

### Recommended Actions

1. **Keep both crates** - They serve different purposes
2. **Integrate into CI**:
   ```yaml
   # .github/workflows/benchmarks.yml
   - name: Run micro-benchmarks
     run: cargo bench --package chronik-benchmarks

   - name: Run load tests
     run: |
       ./target/release/chronik-server &
       ./target/release/chronik-bench --duration 60s
   ```
3. **Document differences** - Add to main README
4. **Cross-reference** - Link from each crate to the other

---

## Naming Clarity

### Rename Suggestion (Optional)

To avoid confusion, consider renaming:

| Current | Suggested | Rationale |
|---------|-----------|-----------|
| `chronik-benchmarks` | `chronik-microbench` | Emphasizes micro-benchmarking |
| `chronik-bench` | `chronik-loadtest` or `chronik-perf` | Emphasizes load testing |

**OR** keep current names and add clear documentation (easier).

---

## Conclusion

**Both crates are valuable and serve different needs**:

- **`chronik-benchmarks`** = Developer tool for optimizing code
- **`chronik-bench`** = Operator tool for capacity planning

**Recommendation**: Keep both, document clearly, use together for comprehensive performance testing.

---

**Created**: October 25, 2024
**Status**: Both crates functional and complementary
**Action**: Document relationship in main README
