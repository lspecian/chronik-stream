# Baseline Performance Results

## Test Date: 2025-11-29 (v2.2.9 Searchable Feature)

### Test Configuration
- **Concurrency**: 128 producers
- **Message Size**: 256 bytes
- **Duration**: 30 seconds
- **Compression**: None
- **WAL Profile**: high (50ms batch interval)
- **Command**: `./target/release/chronik-bench -c 128 -s 256 -d 30s -m produce`

---

## v2.2.9 Searchable vs Non-Searchable Benchmarks (2025-11-29)

### Summary Table

| Configuration | Throughput | p50 | p99 | Total Messages |
|--------------|------------|-----|-----|----------------|
| **Standalone Non-Searchable** | 197,793 msg/s | 0.58 ms | 0.59 ms | 6,922,860 |
| **Standalone Searchable** | 192,064 msg/s | 0.59 ms | 2.15 ms | 5,762,217 |
| **Cluster Non-Searchable** | 183,133 msg/s | 0.61 ms | 2.85 ms | 5,494,200 |
| **Cluster Searchable** | 123,406 msg/s | 0.59 ms | 14.43 ms | 3,702,347 |

### Performance Analysis

#### Standalone vs Cluster

| Metric | Standalone | Cluster | Ratio |
|--------|----------:|--------:|------:|
| Non-Searchable Throughput | 197,793 msg/s | 183,133 msg/s | 92.6% |
| Searchable Throughput | 192,064 msg/s | 123,406 msg/s | 64.3% |

**Key Finding**: The 3-node cluster achieves **92.6%** of standalone throughput for non-searchable topics.

#### Searchable Indexing Impact

| Configuration | Non-Searchable | Searchable | Overhead |
|--------------|---------------:|-----------:|---------:|
| Standalone | 197,793 msg/s | 192,064 msg/s | 2.9% |
| Cluster | 183,133 msg/s | 123,406 msg/s | 32.6% |

**Key Finding**:
- Standalone searchable topics have minimal overhead (only 3%)
- Cluster searchable topics have higher overhead (33%) due to combined indexing + replication costs

#### Latency Impact

| Configuration | p99 Non-Searchable | p99 Searchable | Increase |
|--------------|-------------------:|---------------:|---------:|
| Standalone | 0.59ms | 2.15ms | 3.6x |
| Cluster | 2.85ms | 14.43ms | 5.1x |

### Detailed Latency Breakdown

#### Standalone Non-Searchable
```
p50:    0.58ms
p90:    0.58ms
p95:    0.59ms
p99:    0.59ms
p99.9:  0.64ms
max:    5.40ms
```

#### Standalone Searchable
```
p50:    0.59ms
p90:    1.03ms
p95:    1.27ms
p99:    2.15ms
p99.9:  2.98ms
max:    9.70ms
```

#### Cluster Non-Searchable (3 nodes, acks=1)
```
p50:    0.61ms
p90:    1.01ms
p95:    1.27ms
p99:    2.85ms
p99.9:  3.60ms
max:   12.22ms
```

#### Cluster Searchable (3 nodes, acks=1)
```
p50:    0.59ms
p90:    1.09ms
p95:    1.64ms
p99:   14.43ms
p99.9: 17.38ms
max:   23.21ms
```

### Recommendations

1. **For maximum throughput**: Use standalone mode with non-searchable topics (~200K msg/s)

2. **For searchable requirements**:
   - Standalone mode maintains 97% throughput with searchable enabled
   - Accept the 33% throughput reduction in cluster mode if search is required

3. **For low-latency requirements**:
   - Non-searchable topics maintain sub-1ms p99 in standalone
   - Cluster non-searchable maintains sub-3ms p99
   - Avoid searchable topics if p99 < 5ms is required

4. **For high availability**:
   - Cluster mode with non-searchable topics provides best balance
   - 183K msg/s with 3-way replication and 2.85ms p99

---

## v2.2.10 Benchmark Results (2025-11-28)

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

**Build Version**: v2.2.9
**Test System**: Ubuntu Linux 6.11.0-28-generic
