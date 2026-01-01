# Chronik Stream Performance Benchmarks

**Version**: v2.2.22+
**Last Updated**: 2025-12-31

This document summarizes the performance characteristics of Chronik Stream's advanced storage features: columnar storage (Parquet), SQL queries (DataFusion), and vector search (HNSW).

---

## Running Benchmarks

```bash
# All columnar benchmarks
cargo bench -p chronik-columnar

# Specific benchmark groups
cargo bench -p chronik-columnar -- parquet_conversion
cargo bench -p chronik-columnar -- sql_query
cargo bench -p chronik-columnar -- vector_search

# With HTML reports (output to target/criterion/)
cargo bench -p chronik-columnar -- --save-baseline main
```

---

## Parquet Conversion Benchmarks

**Location**: `crates/chronik-columnar/benches/parquet_conversion.rs`

### Record Conversion (KafkaRecord → Arrow RecordBatch)

| Records | Throughput | Latency |
|---------|------------|---------|
| 1,000 | ~500K records/sec | ~2ms |
| 10,000 | ~600K records/sec | ~16ms |
| 50,000 | ~650K records/sec | ~77ms |

**Key Findings**:
- Conversion is CPU-bound and scales linearly with record count
- Arrow columnar format enables efficient memory layout
- Headers and embeddings add minimal overhead

### Parquet Write Throughput

| Compression | Records | Throughput | File Size Ratio |
|-------------|---------|------------|-----------------|
| None | 10,000 | ~400K records/sec | 100% |
| Snappy | 10,000 | ~380K records/sec | ~40% |
| Gzip | 10,000 | ~150K records/sec | ~25% |
| Zstd | 10,000 | ~300K records/sec | ~22% |
| Lz4 | 10,000 | ~350K records/sec | ~35% |

**Recommendations**:
- **Default**: Zstd - Best compression ratio with good speed
- **Low-latency**: Snappy or Lz4 - Faster writes, less compression
- **Archival**: Gzip - Maximum compression for cold storage

### End-to-End Pipeline

| Records | Latency (Zstd) | Memory |
|---------|----------------|--------|
| 1,000 | ~5ms | ~4MB |
| 10,000 | ~40ms | ~35MB |
| 50,000 | ~180ms | ~170MB |

**With Embeddings (1536-dim OpenAI)**:

| Records | Latency | Memory Overhead |
|---------|---------|-----------------|
| 10,000 | ~150ms | +90MB (vectors) |

---

## SQL Query Benchmarks

**Location**: `crates/chronik-columnar/benches/sql_query.rs`

### SELECT * Queries

| Data Size | Latency (LIMIT 1000) |
|-----------|---------------------|
| 1K rows | ~5ms |
| 10K rows | ~8ms |
| 50K rows | ~12ms |

### COUNT(*) Queries

| Data Size | Latency |
|-----------|---------|
| 1K rows | ~2ms |
| 10K rows | ~3ms |
| 100K rows | ~8ms |

**Key Finding**: COUNT uses Parquet metadata when possible, avoiding full scan.

### WHERE Filter Queries

| Data Size | Filter Type | Latency |
|-----------|-------------|---------|
| 10K rows | Partition = 0 | ~3ms |
| 10K rows | Offset range | ~4ms |
| 100K rows | Partition = 0 | ~5ms |
| 100K rows | Offset range | ~8ms |

**Key Finding**: Predicate pushdown to Parquet reader significantly reduces I/O.

### Aggregation Queries

| Data Size | Query Type | Latency |
|-----------|------------|---------|
| 10K rows | GROUP BY partition | ~6ms |
| 10K rows | MIN/MAX | ~4ms |
| 100K rows | GROUP BY partition | ~15ms |
| 100K rows | MIN/MAX | ~10ms |

### Query Plan Generation (EXPLAIN)

| Query Complexity | Latency |
|------------------|---------|
| Simple SELECT | ~0.5ms |
| Complex (GROUP BY + ORDER BY) | ~1ms |

---

## Vector Search Benchmarks

**Location**: `crates/chronik-columnar/benches/vector_search.rs`

### Index Population (HNSW Build)

| Vectors | Dimensions | Time | Memory |
|---------|------------|------|--------|
| 1,000 | 384 | ~50ms | ~2MB |
| 5,000 | 384 | ~300ms | ~10MB |
| 10,000 | 384 | ~700ms | ~20MB |

**HNSW Parameters**: M=16, ef_construction=100

### Search Latency by Index Size

| Index Size | k=10 Latency | k=50 Latency |
|------------|--------------|--------------|
| 1K vectors | ~0.5ms | ~0.8ms |
| 10K vectors | ~1.2ms | ~2.0ms |
| 50K vectors | ~2.5ms | ~4.5ms |

**Dimensions**: 384 (all-MiniLM-L6-v2)

### Search Latency by k Value

| k | Latency (10K vectors) |
|---|----------------------|
| 10 | ~1.2ms |
| 50 | ~2.0ms |
| 100 | ~3.0ms |
| 200 | ~5.5ms |

### Multi-Partition Search

| Scenario | Latency (30K vectors total) |
|----------|----------------------------|
| Single partition (5K) | ~0.8ms |
| All 6 partitions | ~4.5ms |

**Key Finding**: Multi-partition search parallelizes across partitions.

### Distance Metrics Comparison

| Metric | Latency (10K vectors) |
|--------|----------------------|
| Cosine | ~1.2ms |
| Euclidean | ~1.0ms |
| Dot Product | ~0.9ms |

**Recommendation**: Use Dot Product for normalized vectors (fastest).

### Embedding Dimensions Impact

| Dimensions | Model Example | Latency (10K) | Memory |
|------------|---------------|---------------|--------|
| 128 | Custom | ~0.8ms | ~5MB |
| 384 | all-MiniLM-L6-v2 | ~1.2ms | ~15MB |
| 768 | all-mpnet-base-v2 | ~2.0ms | ~30MB |
| 1536 | text-embedding-3-small | ~4.0ms | ~60MB |

### HNSW M Parameter Tuning

| M | Build Time | Search Latency | Memory |
|---|------------|----------------|--------|
| 8 | ~400ms | ~1.5ms | ~12MB |
| 16 | ~700ms | ~1.2ms | ~20MB |
| 32 | ~1.2s | ~1.0ms | ~35MB |
| 64 | ~2.2s | ~0.9ms | ~65MB |

**Recommendations**:
- **M=16**: Default, good balance of speed/memory/accuracy
- **M=8**: Memory-constrained environments
- **M=32+**: Maximum recall for critical applications

---

## Performance Optimization Tips

### Parquet

1. **Row Group Size**: Use 10K-100K for best query performance
2. **Bloom Filters**: Enable for frequently filtered columns
3. **Compression**: Zstd for general use, Snappy for low-latency

### SQL Queries

1. **Predicate Pushdown**: Filter on _partition, _offset, _timestamp for best performance
2. **Projection**: SELECT only needed columns
3. **Partitioning**: Use daily partitioning for time-range queries

### Vector Search

1. **ef_search**: Increase for better recall (default 50)
2. **Batch Operations**: Add vectors in batches of 1000+
3. **Dimension Reduction**: Consider smaller models for speed
4. **Partition Strategy**: Match Kafka partitions for filtered queries

---

## Hardware Considerations

**Test Environment**:
- CPU: AMD EPYC 7B12 (8 cores)
- Memory: 32GB DDR4
- Storage: NVMe SSD

**Scaling Factors**:
- SQL queries scale with CPU cores (DataFusion parallelism)
- Vector search is single-threaded per partition
- Compression is CPU-bound

---

## Benchmark Methodology

All benchmarks use [Criterion.rs](https://github.com/bheisler/criterion.rs) with:
- Warm-up: 3 seconds
- Measurement: 5 seconds
- Sample size: 100 (reduced for slow operations)

Results are median values to exclude outliers.

---

## Related Documentation

- [Columnar Storage Guide](COLUMNAR_STORAGE_GUIDE.md) - Configuration and usage
- [Vector Search Guide](VECTOR_SEARCH_GUIDE.md) - HNSW tuning
- [API Reference](API_REFERENCE.md) - All endpoints
