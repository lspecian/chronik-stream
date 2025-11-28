# Baseline Performance Results

## Test Date: 2025-11-28 (v2.2.10)

### Test Configuration
- **Concurrency**: 128 producers
- **Message Size**: 256 bytes
- **Duration**: 30 seconds
- **Compression**: None
- **Command**: `./target/release/chronik-bench -b localhost:9092 -c 128 -s 256 -d 30s --acks <1|all>`

---

## v2.2.10 Benchmark Results

### Summary Table

| Mode | acks | Throughput | Bandwidth | p50 | p99 | p99.9 |
|------|------|------------|-----------|-----|-----|-------|
| **Standalone** | 1 | 309,590 msg/s | 75.58 MB/s | 0.33 ms | 0.59 ms | 4.37 ms |
| **Standalone** | all | 347,585 msg/s | 84.86 MB/s | 0.34 ms | 0.56 ms | 0.88 ms |
| **Cluster (3-node)** | 1 | 188,165 msg/s | 45.94 MB/s | 0.59 ms | 2.81 ms | 3.65 ms |
| **Cluster (3-node)** | all | 165,808 msg/s | 40.48 MB/s | 0.71 ms | 1.80 ms | 2.64 ms |

### Detailed Results

#### Standalone Mode - acks=1

| Metric | Value |
|--------|-------|
| **Message Rate** | **309,590 msg/s** |
| **Total Messages** | 10,835,762 |
| **Bandwidth** | 75.58 MB/s |
| **Data Transferred** | 2.58 GB |
| **Success Rate** | 100% (0 failures) |

**Latency Distribution:**

| Percentile | Latency (μs) | Latency (ms) |
|------------|--------------|--------------|
| p50 | 327 | 0.33 |
| p90 | 417 | 0.42 |
| p95 | 451 | 0.45 |
| p99 | 590 | 0.59 |
| p99.9 | 4,371 | 4.37 |
| max | 15,895 | 15.89 |

#### Standalone Mode - acks=all

| Metric | Value |
|--------|-------|
| **Message Rate** | **347,585 msg/s** |
| **Total Messages** | 10,428,006 |
| **Bandwidth** | 84.86 MB/s |
| **Data Transferred** | 2.49 GB |
| **Success Rate** | 100% (0 failures) |

**Latency Distribution:**

| Percentile | Latency (μs) | Latency (ms) |
|------------|--------------|--------------|
| p50 | 341 | 0.34 |
| p90 | 435 | 0.43 |
| p95 | 468 | 0.47 |
| p99 | 556 | 0.56 |
| p99.9 | 882 | 0.88 |
| max | 431,615 | 431.62 |

#### Cluster Mode (3-node) - acks=1

| Metric | Value |
|--------|-------|
| **Message Rate** | **188,165 msg/s** |
| **Total Messages** | 5,645,145 |
| **Bandwidth** | 45.94 MB/s |
| **Data Transferred** | 1.35 GB |
| **Success Rate** | 100% (0 failures) |

**Latency Distribution:**

| Percentile | Latency (μs) | Latency (ms) |
|------------|--------------|--------------|
| p50 | 588 | 0.59 |
| p90 | 981 | 0.98 |
| p95 | 1,227 | 1.23 |
| p99 | 2,807 | 2.81 |
| p99.9 | 3,645 | 3.65 |
| max | 17,055 | 17.05 |

#### Cluster Mode (3-node) - acks=all

| Metric | Value |
|--------|-------|
| **Message Rate** | **165,808 msg/s** |
| **Total Messages** | 4,974,495 |
| **Bandwidth** | 40.48 MB/s |
| **Data Transferred** | 1.19 GB |
| **Success Rate** | 100% (0 failures) |

**Latency Distribution:**

| Percentile | Latency (μs) | Latency (ms) |
|------------|--------------|--------------|
| p50 | 710 | 0.71 |
| p90 | 1,162 | 1.16 |
| p95 | 1,347 | 1.35 |
| p99 | 1,801 | 1.80 |
| p99.9 | 2,635 | 2.64 |
| max | 6,195 | 6.20 |

### Key Observations

1. **Standalone acks=all faster than acks=1** (347K vs 310K msg/s)
   - Expected: single-node has no replication overhead
   - acks=all just waits for leader (which is the only node)

2. **Cluster acks=1 vs acks=all**:
   - acks=1: 188K msg/s (leader-only acknowledgment)
   - acks=all: 166K msg/s (waits for all replicas)
   - ~12% overhead for full replication guarantees

3. **Cluster vs Standalone ratio**:
   - acks=1: 188K/310K = **61%** of standalone performance
   - acks=all: 166K/348K = **48%** of standalone performance
   - Expected due to Raft consensus + WAL replication overhead

4. **Latency is excellent across all modes**:
   - Standalone p99 < 1ms
   - Cluster p99 < 3ms even with full replication

---

## Historical Results

### v2.2.9 (2025-11-25)

#### Standalone - acks=1

| Metric | Value |
|--------|-------|
| **Average Message Rate** | **333,321 msg/s** |
| **Peak Message Rate** | **348,258 msg/s** |
| **Total Messages** | 10,000,168 |
| **Bandwidth** | 81.38 MB/s |
| **p99 Latency** | 0.81 ms |

---

**Build Version**: v2.2.10
**Test System**: Ubuntu Linux 6.11.0-28-generic
