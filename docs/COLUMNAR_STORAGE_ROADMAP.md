# Columnar Storage Implementation Roadmap

> **SUPERSEDED**: This roadmap has been merged into the unified Advanced Storage Roadmap.
> Please use [ADVANCED_STORAGE_ROADMAP.md](ADVANCED_STORAGE_ROADMAP.md) instead.
>
> The unified roadmap combines **Columnar Storage** + **Vector Search** as both are
> per-topic options sharing the same infrastructure (topic config, unified API, WalIndexer).

---

*Original content preserved below for reference:*

---

**Project**: Per-Topic Arrow/Parquet Storage with Unified API
**Target Version**: v2.3.0
**Start Date**: ___________
**Estimated Duration**: 5 weeks

---

## Progress Tracker

### Overall Status

| Phase | Status | Progress | Start Date | Completion Date |
|-------|--------|----------|------------|-----------------|
| Phase 1: Foundation | Not Started | 0/12 | - | - |
| Phase 2: Write Path | Not Started | 0/14 | - | - |
| Phase 3: Query Engine | Not Started | 0/16 | - | - |
| Phase 4: Unified API | Not Started | 0/18 | - | - |
| Phase 5: Arrow Flight | Not Started | 0/10 | - | - |
| Phase 6: Testing & Polish | Not Started | 0/12 | - | - |

**Total Progress**: 0/82 tasks

---

## Phase 1: Foundation (Week 1)

**Goal**: Create the `chronik-columnar` crate with configuration and schema support.

### 1.1 Crate Setup
- [ ] Create `crates/chronik-columnar/` directory structure
- [ ] Create `Cargo.toml` with Arrow/Parquet dependencies
- [ ] Add to workspace in root `Cargo.toml`
- [ ] Create `src/lib.rs` with module exports
- [ ] Verify crate compiles with `cargo check -p chronik-columnar`

### 1.2 Configuration
- [ ] Create `src/config.rs` with `ColumnarConfig` struct
- [ ] Implement `ColumnarConfig::from_topic_config()` parser
- [ ] Add validation for all columnar config keys
- [ ] Add unit tests for config parsing
- [ ] Document all config keys in code comments

### 1.3 Topic Config Integration
- [ ] Update `TopicValidator` to validate columnar config keys
- [ ] Add integration test: create topic with columnar config via Kafka API

**Phase 1 Checklist**: 0/12 complete

**Notes**:
```
Session notes go here...
```

---

## Phase 2: Write Path (Week 2)

**Goal**: Generate Parquet files from WAL segments for columnar-enabled topics.

### 2.1 Arrow Schema
- [ ] Create `src/schema.rs` with `kafka_message_schema()`
- [ ] Implement standard schema (topic, partition, offset, timestamp, key, value, headers)
- [ ] Add `extended_schema_with_json_fields()` for decoded JSON columns
- [ ] Add unit tests for schema creation

### 2.2 Record Conversion
- [ ] Create `src/converter.rs`
- [ ] Implement `CanonicalRecord` → Arrow `RecordBatch` conversion
- [ ] Handle nullable key field
- [ ] Handle headers as Arrow Map type
- [ ] Add benchmarks for conversion performance

### 2.3 Parquet Writer
- [ ] Create `src/writer.rs` with `ParquetSegmentWriter`
- [ ] Implement compression support (zstd, snappy, lz4, gzip, none)
- [ ] Implement bloom filter generation for specified columns
- [ ] Implement row group management
- [ ] Implement statistics collection (min/max/null_count)
- [ ] Add unit tests for Parquet writing

### 2.4 WalIndexer Integration
- [ ] Add `is_columnar_enabled()` check in WalIndexer
- [ ] Implement `write_parquet_segment()` method
- [ ] Add Parquet path generation (with time partitioning)
- [ ] Integration test: produce messages → verify Parquet file created

**Phase 2 Checklist**: 0/14 complete

**Notes**:
```
Session notes go here...
```

---

## Phase 3: Query Engine (Week 3)

**Goal**: SQL query support via DataFusion with predicate pushdown.

### 3.1 DataFusion Setup
- [ ] Create `src/query_engine.rs`
- [ ] Initialize `SessionContext` with custom configuration
- [ ] Set up memory limits and query timeout
- [ ] Add unit test for basic query execution

### 3.2 Topic Registration
- [ ] Implement `register_topic()` for Parquet-backed tables
- [ ] Create `ListingTable` with Parquet files
- [ ] Handle schema inference from Parquet metadata
- [ ] Support topics with multiple partition directories
- [ ] Add test: register topic and run simple SELECT

### 3.3 Query Execution
- [ ] Implement `execute_sql()` returning `Vec<RecordBatch>`
- [ ] Implement `execute_sql_stream()` for streaming results
- [ ] Implement `explain()` for query plan visualization
- [ ] Add predicate pushdown for timestamp/offset filters
- [ ] Add partition pruning based on topic/partition columns

### 3.4 Custom Functions (UDFs)
- [ ] Create `src/udfs.rs`
- [ ] Implement `json_extract(value, path)` UDF
- [ ] Implement `json_extract_string(value, path)` UDF
- [ ] Implement `decode_utf8(binary)` UDF
- [ ] Add tests for all UDFs

### 3.5 Segment Index Integration
- [ ] Extend `SegmentIndex` to track Parquet files
- [ ] Implement `get_parquet_paths(topic)` method
- [ ] Add Parquet metadata caching
- [ ] Persist Parquet index to metadata WAL

**Phase 3 Checklist**: 0/16 complete

**Notes**:
```
Session notes go here...
```

---

## Phase 4: Unified API (Week 4)

**Goal**: Merge Search, SQL, Admin, and Schema Registry APIs on port 6092.

### 4.1 Unified API Structure
- [ ] Create `crates/chronik-server/src/unified_api/mod.rs`
- [ ] Create `UnifiedApi` struct holding all API components
- [ ] Implement `UnifiedApi::new()` with dependency injection
- [ ] Create unified router combining all endpoint groups

### 4.2 Endpoint Migration
- [ ] Move Search API endpoints under `/_search/*` namespace
- [ ] Move Admin API endpoints under `/admin/*` namespace
- [ ] Move Schema Registry endpoints under `/schemas/*` and `/subjects/*`
- [ ] Ensure backward compatibility with existing ES-compatible paths
- [ ] Add deprecation headers for old Admin API port (10001)

### 4.3 SQL Endpoints
- [ ] Create `src/unified_api/sql_handler.rs`
- [ ] Implement `POST /_sql` endpoint
- [ ] Implement `POST /_sql/explain` endpoint
- [ ] Implement `GET /_sql/tables` endpoint
- [ ] Implement `GET /_sql/describe/:table` endpoint
- [ ] Add request/response JSON schemas

### 4.4 Server Integration
- [ ] Update `main.rs` to start unified API on port 6092
- [ ] Remove separate Search API startup (port 6092 → unified)
- [ ] Remove separate Admin API startup (port 10001 → unified 6092)
- [ ] Add configuration for unified API port
- [ ] Integration test: all endpoints accessible on single port

### 4.5 Authentication & Middleware
- [ ] Unify authentication between Admin and public endpoints
- [ ] Admin endpoints require API key (existing behavior)
- [ ] Search/SQL endpoints optionally require API key
- [ ] Add rate limiting middleware
- [ ] Add request logging middleware

**Phase 4 Checklist**: 0/18 complete

**Notes**:
```
Session notes go here...
```

---

## Phase 5: Arrow Flight (Week 4-5)

**Goal**: High-performance streaming query results via Arrow Flight protocol.

### 5.1 Flight Service Setup
- [ ] Create `src/flight.rs` in chronik-columnar
- [ ] Implement `FlightService` struct
- [ ] Set up gRPC server with tonic
- [ ] Add Flight service to unified API router

### 5.2 Flight SQL Implementation
- [ ] Implement `get_flight_info()` for query metadata
- [ ] Implement `do_get()` for streaming RecordBatches
- [ ] Implement `do_put()` for data ingestion (optional)
- [ ] Add query result caching for repeated `do_get()` calls

### 5.3 Integration
- [ ] Mount Flight service on unified API port (6092)
- [ ] Add Flight endpoint documentation
- [ ] Create Python client example
- [ ] Create Rust client example
- [ ] Performance benchmark: Flight vs REST JSON

**Phase 5 Checklist**: 0/10 complete

**Notes**:
```
Session notes go here...
```

---

## Phase 6: Testing & Polish (Week 5)

**Goal**: Comprehensive testing, benchmarks, and documentation.

### 6.1 Integration Tests
- [ ] Test: Create topic with columnar config via Kafka API
- [ ] Test: Produce messages → Parquet files generated
- [ ] Test: SQL query returns correct results
- [ ] Test: Predicate pushdown reduces scan size
- [ ] Test: Unified API backward compatibility
- [ ] Test: Arrow Flight streaming query

### 6.2 Performance Benchmarks
- [ ] Benchmark: Parquet write throughput (msgs/sec)
- [ ] Benchmark: SQL query latency by data size
- [ ] Benchmark: Arrow Flight vs REST performance
- [ ] Document benchmark results in `docs/BENCHMARKS.md`

### 6.3 Documentation
- [ ] Update `CLAUDE.md` with columnar storage section
- [ ] Create user guide: `docs/COLUMNAR_STORAGE_GUIDE.md`
- [ ] Document SQL syntax and supported functions
- [ ] Add API reference for new endpoints

**Phase 6 Checklist**: 0/12 complete

**Notes**:
```
Session notes go here...
```

---

## Configuration Reference

### Topic-Level Columnar Config Keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `columnar.enabled` | bool | `false` | Enable columnar storage for topic |
| `columnar.format` | string | `parquet` | Format: `parquet`, `arrow`, `orc` |
| `columnar.compression` | string | `zstd` | Compression: `zstd`, `snappy`, `lz4`, `gzip`, `none` |
| `columnar.compression.level` | int | `3` | Compression level (zstd: 1-22) |
| `columnar.row_group_size` | int | `100000` | Rows per row group |
| `columnar.page_size` | int | `1048576` | Page size in bytes (1MB) |
| `columnar.bloom_filter` | bool | `true` | Enable bloom filters |
| `columnar.bloom_filter.columns` | string | `offset,timestamp` | Columns for bloom filter |
| `columnar.dictionary` | bool | `true` | Enable dictionary encoding |
| `columnar.statistics` | string | `full` | Statistics: `full`, `chunk`, `none` |
| `columnar.partitioning` | string | `none` | Time partitioning: `none`, `hourly`, `daily` |

### Server Configuration

```toml
[unified_api]
port = 6092                    # Unified API port (Search + SQL + Admin)
search_enabled = true          # Enable /_search endpoints
sql_enabled = true             # Enable /_sql endpoints
admin_enabled = true           # Enable /admin endpoints
schema_registry_enabled = true # Enable /schemas endpoints
arrow_flight_enabled = true    # Enable Arrow Flight on same port

sql_max_rows = 100000          # Max rows per SQL query
sql_timeout_ms = 30000         # SQL query timeout
```

---

## File Checklist

### New Files to Create

| File | Phase | Status |
|------|-------|--------|
| `crates/chronik-columnar/Cargo.toml` | 1 | [ ] |
| `crates/chronik-columnar/src/lib.rs` | 1 | [ ] |
| `crates/chronik-columnar/src/config.rs` | 1 | [ ] |
| `crates/chronik-columnar/src/schema.rs` | 2 | [ ] |
| `crates/chronik-columnar/src/converter.rs` | 2 | [ ] |
| `crates/chronik-columnar/src/writer.rs` | 2 | [ ] |
| `crates/chronik-columnar/src/reader.rs` | 3 | [ ] |
| `crates/chronik-columnar/src/query_engine.rs` | 3 | [ ] |
| `crates/chronik-columnar/src/udfs.rs` | 3 | [ ] |
| `crates/chronik-columnar/src/flight.rs` | 5 | [ ] |
| `crates/chronik-server/src/unified_api/mod.rs` | 4 | [ ] |
| `crates/chronik-server/src/unified_api/sql_handler.rs` | 4 | [ ] |
| `crates/chronik-server/src/unified_api/router.rs` | 4 | [ ] |
| `docs/COLUMNAR_STORAGE_GUIDE.md` | 6 | [ ] |

### Files to Modify

| File | Phase | Changes | Status |
|------|-------|---------|--------|
| `Cargo.toml` (root) | 1 | Add chronik-columnar to workspace | [ ] |
| `crates/chronik-server/Cargo.toml` | 1 | Add chronik-columnar dependency | [ ] |
| `crates/chronik-protocol/src/create_topics/topic_validator.rs` | 1 | Validate columnar configs | [ ] |
| `crates/chronik-storage/src/wal_indexer.rs` | 2 | Add Parquet writing | [ ] |
| `crates/chronik-storage/src/segment_index.rs` | 2 | Track Parquet files | [ ] |
| `crates/chronik-server/src/main.rs` | 4 | Start unified API | [ ] |
| `CLAUDE.md` | 6 | Document columnar storage | [ ] |

---

## Session Log

### Session 1: ___________
**Duration**: ___ hours
**Tasks Completed**:
-

**Blockers/Issues**:
-

**Next Steps**:
-

---

### Session 2: ___________
**Duration**: ___ hours
**Tasks Completed**:
-

**Blockers/Issues**:
-

**Next Steps**:
-

---

### Session 3: ___________
**Duration**: ___ hours
**Tasks Completed**:
-

**Blockers/Issues**:
-

**Next Steps**:
-

---

### Session 4: ___________
**Duration**: ___ hours
**Tasks Completed**:
-

**Blockers/Issues**:
-

**Next Steps**:
-

---

### Session 5: ___________
**Duration**: ___ hours
**Tasks Completed**:
-

**Blockers/Issues**:
-

**Next Steps**:
-

---

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2024-XX-XX | Use port 6092 for unified API | Backward compatibility with existing Search API clients |
| 2024-XX-XX | Arrow/Parquet over custom format | Ecosystem compatibility, proven reliability |
| 2024-XX-XX | Per-topic opt-in | Gradual adoption, no breaking changes |
| 2024-XX-XX | Include Arrow Flight | High-performance streaming for analytics use cases |

---

## Risk Register

| Risk | Impact | Mitigation | Status |
|------|--------|------------|--------|
| Arrow/Parquet version conflicts | High | Pin specific versions, test extensively | Open |
| DataFusion query compatibility | Medium | Limit SQL to well-tested subset | Open |
| Performance regression on write path | High | Async Parquet writing, don't block Kafka produce | Open |
| Port 6092 conflicts with existing deployments | Low | Document migration path, support both ports temporarily | Open |

---

## Quick Reference

### Commands for Development

```bash
# Build columnar crate
cargo build -p chronik-columnar

# Run columnar tests
cargo test -p chronik-columnar

# Check all crates compile
cargo check --workspace

# Run specific integration test
cargo test --test columnar_integration

# Benchmark Parquet writing
cargo bench -p chronik-columnar --bench parquet_write
```

### Creating a Topic with Columnar Storage

```bash
# Via Kafka CLI
kafka-topics.sh --bootstrap-server localhost:9092 \
  --create --topic orders \
  --partitions 6 \
  --config columnar.enabled=true \
  --config columnar.format=parquet \
  --config columnar.compression=zstd

# Via Admin API (after Phase 4)
curl -X POST http://localhost:6092/admin/topics \
  -H "X-API-Key: $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name": "orders", "partitions": 6, "config": {"columnar.enabled": "true"}}'
```

### SQL Query Example

```bash
curl -X POST http://localhost:6092/_sql \
  -H "Content-Type: application/json" \
  -d '{"query": "SELECT * FROM orders LIMIT 10"}'
```
