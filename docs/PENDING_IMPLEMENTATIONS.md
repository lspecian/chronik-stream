# Pending Implementations

This document catalogs all TODOs, stubs, and incomplete implementations in the Chronik Stream codebase. Items are categorized by priority and include implementation requirements.

**Last Updated**: 2025-12-08
**Version**: v2.2.20

---

## Completion Status

### ✅ Phase 0: Dead Code Cleanup - COMPLETE (v2.2.17)
- Removed ~10,405 lines of dead code across 22 files + 1 crate
- See commit: "Major codebase cleanup: remove obsolete files and update docs"

### ✅ Phase 1: Data Integrity - COMPLETE (v2.2.18)
- **Segment Checksums**: Implemented CRC32 checksums in `segment.rs`
  - `calculate_checksum()` hashes header fields + all data sections
  - Checksum written during `serialize()`, verified during `deserialize()`
  - Corruption detection with clear error messages
- **LZ4/Snappy Compression**: Implemented in `optimized_segment.rs`
  - LZ4: Uses `lz4_flex` frame encoder/decoder
  - Snappy: Uses `snap` raw encoder/decoder
  - All compression types now fully functional
- **Column Checksums**: Implemented in optimized segment footer
  - Per-column CRC32 checksums
  - File checksum over all content before footer
  - Compression ratio tracking

### ✅ Phase 1b: Hot-Path Logging Fix - COMPLETE (v2.2.18)
- **Performance Fix**: Changed INFO logs to TRACE in hot paths
  - +168% throughput improvement (146K → 391K msg/s)
  - 99% lower p99 latency (46ms → 0.5ms)
- **Files Fixed**: kafka_handler.rs, produce_handler.rs, fetch_handler.rs, segment_reader.rs, record_filter.rs

### ✅ Phase 2: Security - COMPLETE (v2.2.18)
- **Backup Encryption**: Implemented in `chronik-backup/src/encryption.rs`
  - AES-256-GCM: AEAD encryption with authentication tag
  - ChaCha20-Poly1305: Alternative AEAD cipher
  - AES-256-CTR-HMAC-SHA256: Encrypt-then-MAC construction
  - Key derivation: PBKDF2-SHA256 and Argon2id
  - KeyManager trait with MemoryKeyManager implementation
  - Envelope format with nonce, ciphertext, tag, and metadata
  - AAD (Additional Authenticated Data) support
  - 14 comprehensive tests for all encryption modes

### ✅ Phase 3: Topic Deletion - COMPLETE (v2.2.18)
- **Topic Deletion**: Implemented full Kafka-compatible topic deletion
  - `delete_topic()` in `WalMetadataStore` cleans up all associated data:
    - Topic metadata removal
    - Partition assignments cleanup
    - Partition offsets cleanup
    - Consumer offsets cleanup
    - Segment metadata cleanup
  - `handle_delete_topics()` in protocol handler calls metadata store
  - Proper error codes: `NONE` (0) on success, `UNKNOWN_TOPIC_OR_PARTITION` (3) for missing topics
  - `TopicDeleted` event published to event bus for cluster replication
  - Tested with Python kafka-python client

### ✅ Phase 3b: API Completeness - COMPLETE (v2.2.19)
- **Configuration Updates (AlterConfigs/IncrementalAlterConfigs)**:
  - `handle_alter_configs()` now persists config changes to metadata store
  - Applies retention.ms, segment.bytes, and custom configs
  - Validates config values before applying
  - `handle_incremental_alter_configs()` already fully implemented
  - Tested with Python kafka-python client
- **Consumer Group Improvements**:
  - `persist_group()` properly converts members HashMap to Vec<GroupMember>
  - Serializes member assignment to Kafka wire format
  - Partition leader lookup via `metadata_store.get_partition_leader()`
  - Batch offset fetching fetches all topics/partitions in one operation

### ✅ Phase 4: Performance Optimizations - COMPLETE (v2.2.20)
- **Segment Cache Layer**: Already implemented in `chronik-storage/src/segment_cache.rs`
  - LRU eviction with configurable max entries and size
  - TTL-based cache invalidation
  - Warmup capabilities for hot segments
  - Cache stats tracking (hit rate, memory usage)
- **Vector Search HNSW**: Upgraded from brute-force O(n) to real HNSW O(log n)
  - Uses `instant-distance` crate for HNSW graph
  - Supports Euclidean, Cosine, and Inner Product distance metrics
  - Configurable M, ef_construction, and ef_search parameters
  - Serialization/deserialization for persistence
  - Fallback to brute-force if index not built
- **Benchmarks**: Enhanced with vector search benchmarks
  - HNSW vs brute-force search comparison (1K, 10K, 50K vectors)
  - HNSW index build time benchmarks
  - Added to existing segment comparison benchmarks

---

## ~~Dead Code to Delete (Immediate Cleanup)~~ - COMPLETED

The codebase contains **~10,405 lines of dead code** across 22 files + 1 entire crate that should be removed.

### chronik-server/src/ - 9 files (2,998 lines)

| File | Lines | Status | Notes |
|------|-------|--------|-------|
| `handler.rs` | 965 | DEAD | Declared but nothing imported. Working impl is `kafka_handler.rs` |
| `schema_registry_api.rs` | 585 | DEAD | Not declared in main.rs |
| `fetch_handler_test.rs` | 565 | DEAD | Not declared in main.rs |
| `connection_state.rs` | 464 | DEAD | Not declared (different from `replication/connection_state.rs`) |
| `schema_registry.rs` | 374 | DEAD | Not declared in main.rs |
| `consumer_coordinator.rs` | 251 | DEAD | Not declared in main.rs |
| `tls.rs` | 227 | DEAD | Not declared in main.rs |
| `sasl_test.rs` | 213 | DEAD | Not declared in main.rs |
| `shutdown.rs` | 176 | DEAD | Not declared in main.rs |
| `sasl_demo.rs` | 143 | DEAD | Not declared in main.rs |

### chronik-protocol/src/ - 11 files (4,292 lines)

| File | Lines | Status | Notes |
|------|-------|--------|-------|
| `consumer_group_coordinator.rs` | 812 | DEAD | Not declared in lib.rs |
| `protocol_v2.rs` | 742 | DEAD | Not declared in lib.rs |
| `transaction_coordinator.rs` | 468 | DEAD | Not declared in lib.rs |
| `acl_manager.rs` | 446 | DEAD | Not declared in lib.rs |
| `client_compat.rs` | 406 | DEAD | Not declared in lib.rs |
| `quota_manager.rs` | 352 | DEAD | Not declared in lib.rs |
| `alter_client_quotas_types.rs` | 263 | DEAD | Not declared in lib.rs |
| `describe_client_quotas_types.rs` | 257 | DEAD | Not declared in lib.rs |
| `transaction_types.rs` | 198 | DEAD | Not declared in lib.rs |
| `delete_records_types.rs` | 177 | DEAD | Not declared in lib.rs |
| `debug.rs` | 171 | DEAD | Not declared in lib.rs |

### chronik-wal/src/ - 2 files (1,142 lines)

| File | Lines | Status | Notes |
|------|-------|--------|-------|
| `replication.rs` | 665 | DEAD | Exported but never imported. Working impl is `chronik-server/src/wal_replication.rs` |
| `concurrency_test.rs` | 477 | DEAD | Not declared in lib.rs |

### chronik-auth/ crate - ~1,973 lines

| Component | Lines | Status | Notes |
|-----------|-------|--------|-------|
| Entire crate | ~1,973 | DEAD | Only consumer is dead `handler.rs`. SASL is in `chronik-protocol/src/sasl.rs` |

### Summary

| Location | Files | Lines |
|----------|-------|-------|
| chronik-server/src/ | 10 | 3,963 |
| chronik-protocol/src/ | 11 | 4,292 |
| chronik-wal/src/ | 2 | 1,142 |
| chronik-auth/ | entire crate | ~1,973 |
| **Total** | **23+** | **~10,405** |

### Actions

**Immediate (safe to delete)**:
1. Delete all 10 dead files in `chronik-server/src/`
2. Delete all 11 dead files in `chronik-protocol/src/`
3. Delete `chronik-wal/src/replication.rs` and `concurrency_test.rs`, update `lib.rs`
4. Remove `mod handler;` from `chronik-server/src/main.rs`

**Evaluate first**:
5. `chronik-auth/` crate - Either delete entirely OR wire into `kafka_handler.rs` if ACL enforcement is wanted

**Why this happened**: Alternative implementations were created in different locations, but the old stubs were never cleaned up. Files not declared in `mod` statements are simply ignored by the Rust compiler - they exist but are never compiled.

---

## Priority 0: Critical Core Functionality

### 1. ~~WAL Replication~~ ✅ IMPLEMENTED

**Status**: **WORKING** - The working implementation is in `chronik-server/src/wal_replication.rs` (2,405 lines)

| Component | Status |
|-----------|--------|
| TCP streaming replication | ✅ Working |
| Frame protocol (magic numbers) | ✅ Working |
| Heartbeats (10s interval, 30s timeout) | ✅ Working |
| Automatic reconnection with backoff | ✅ Working |
| ISR tracking | ✅ Working |
| Lock-free MPMC queue | ✅ Working |

The dead stub in `chronik-wal/src/replication.rs` should be deleted (see Dead Code section above).

---

### 2. Transaction Coordinator (chronik-protocol/src/transaction_coordinator.rs) - **DEAD CODE**

**NOTE**: This file is NOT declared in lib.rs and is therefore dead code. If transaction support is needed, it would need to be either:
1. Declared in lib.rs and integrated with kafka_handler.rs, OR
2. Re-implemented fresh in the working codebase

The original stub had these TODOs (for reference only):

**Status**: Basic state machine works, but doesn't actually coordinate with partitions

| Line | Item | Description |
|------|------|-------------|
| 252 | Coordinate with partition leaders | end_txn() doesn't send to partitions |
| 314 | Send transaction markers | Missing WriteTxnMarkers implementation |
| 400 | Send abort markers | Abort path doesn't notify partitions |
| 455 | Write markers to partition logs | Transaction markers not persisted |

**Implementation Requirements**:
```
1. end_transaction() needs to:
   - Build WriteTxnMarkersRequest for each partition in transaction
   - Send to partition leaders (via protocol handler or direct)
   - Wait for acknowledgments from all partitions
   - Handle partial failures with retry logic
   - Only mark transaction complete after all partitions confirm

2. WriteTxnMarkers format:
   - TransactionResult (COMMIT/ABORT)
   - ProducerId, ProducerEpoch
   - CoordinatorEpoch
   - List of (Topic, Partition, ProducedOffset)

3. Partition leader handling:
   - Receive WriteTxnMarkers request
   - Write control batch to partition log
   - Update transaction state in local cache
   - Return success/failure

Dependencies:
- ProduceHandler needs WriteTxnMarkers handling
- Control batch format already defined in Kafka protocol
- Metadata store has commit_transaction/abort_transaction methods
```

**Effort Estimate**: 3-4 days

---

## Priority 1: Security & Compliance

### 3. ACL Persistence (chronik-protocol/src/acl_manager.rs) - **DEAD CODE**

**NOTE**: This file is NOT declared in lib.rs and is therefore dead code.

**Status**: ACLs work in-memory but don't persist across restarts (if it were wired in)

| Line | Item | Description |
|------|------|-------------|
| 130 | ACL persistence | add_acl() doesn't persist to metadata store |
| 340 | ACL deletion persistence | delete_acls() doesn't persist |
| 434 | ACL recovery | No recovery from metadata store on startup |

**Implementation Requirements**:
```
1. MetadataStore trait needs ACL methods:
   async fn save_acl(&self, acl: AclBinding) -> Result<()>;
   async fn delete_acl(&self, filter: AclFilter) -> Result<Vec<AclBinding>>;
   async fn list_acls(&self) -> Result<Vec<AclBinding>>;

2. AclManager changes:
   - add_acl(): Call metadata_store.save_acl() after in-memory insert
   - delete_acls(): Call metadata_store.delete_acl() after in-memory delete
   - new(): Call load_acls() on construction

3. WAL metadata store needs:
   - New command types: CreateAcl, DeleteAcl
   - Serialization for AclBinding struct
   - Recovery handling in replay_command()

Dependencies:
- AclBinding struct already defined
- MetadataStore trait exists (needs extension)
- WalMetadataStore handles other metadata
```

**Effort Estimate**: 1-2 days

---

### ~~4. Backup Encryption (chronik-backup/src/encryption.rs)~~ ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - Full encryption support implemented

Implementation:
- `encrypt()` and `decrypt()` with KeyManager integration
- AES-256-GCM, ChaCha20-Poly1305, AES-256-CTR-HMAC-SHA256 support
- `encrypt_with_key()` and `decrypt_with_key()` for direct key usage
- PBKDF2-SHA256 and Argon2id key derivation
- `MemoryKeyManager` implementation for testing
- `BackupManager` updated with `key_manager` field
- Tests: 14 comprehensive tests for all encryption types

---

### 5. GSSAPI/Kerberos Authentication (chronik-auth/src/sasl.rs) - **DEAD CRATE**

**NOTE**: The entire `chronik-auth` crate is dead code (only consumer is dead `handler.rs`).
Working SASL authentication is in `chronik-protocol/src/sasl.rs` with PLAIN and SCRAM-SHA-256/512.

**Status**: Mechanism defined but returns "not implemented"

| Line | Item | Description |
|------|------|-------------|
| 170 | GSSAPI authenticate | Returns error immediately |
| 191 | GSSAPI process | Returns error immediately |

**Implementation Requirements**:
```
1. Add dependencies:
   libgssapi = "0.6"  # Rust bindings to GSSAPI

2. Implementation complexity: HIGH
   - Requires Kerberos infrastructure (KDC)
   - Multi-step authentication flow
   - Service principal configuration
   - Keytab management

3. Steps:
   - Accept security context from client
   - Verify client principal against KDC
   - Extract authenticated identity
   - Handle context continuation tokens

4. Configuration needed:
   - CHRONIK_GSSAPI_KEYTAB=/path/to/keytab
   - CHRONIK_GSSAPI_PRINCIPAL=kafka/hostname@REALM

Recommendation: Lower priority - PLAIN and SCRAM cover most use cases.
Consider implementing only if enterprise Kerberos integration is required.
```

**Effort Estimate**: 5-7 days (if needed)

---

## ~~Priority 2: Data Integrity & Storage~~ - COMPLETED (v2.2.18)

### ~~6. Segment Checksums (chronik-storage)~~ - ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - CRC32 checksums now calculated and verified

Implementation:
- `segment.rs`: `calculate_checksum()` hashes magic, version, sizes, metadata, and all data sections
- Checksum computed during `serialize()` and verified during `deserialize()`
- Clear error message on corruption: "Checksum mismatch: expected 0x..., got 0x..."
- Tests: `test_segment_checksum_roundtrip`, `test_segment_checksum_corruption_detection`

---

### ~~7. LZ4/Snappy Compression (chronik-storage/src/optimized_segment.rs)~~ - ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - All compression types now fully functional

Implementation:
- LZ4: Uses `lz4_flex::frame::{FrameEncoder, FrameDecoder}`
- Snappy: Uses `snap::raw::{Encoder, Decoder}`
- Zstd: Already worked (using `zstd::stream`)
- Tests: `test_lz4_compression_roundtrip`, `test_snappy_compression_roundtrip`, `test_zstd_compression_roundtrip`

Also implemented column checksums and file checksum in optimized segment footer.

---

### 8. Schema Registry Persistence (chronik-server/src/schema_registry.rs) - **DEAD CODE**

**NOTE**: This file is NOT declared in main.rs and is therefore dead code.

**Status**: Schemas stored in-memory only, lost on restart (if it were wired in)

| Line | Item | Description |
|------|------|-------------|
| 113 | load_schemas() | Returns empty collections |
| 122 | load_configs() | Returns empty HashMap |
| 184 | Persist on register | Only stores in-memory |
| 236 | Persist on delete | Only deletes from memory |

**Implementation Requirements**:
```
1. MetadataStore trait extensions:
   async fn save_schema(&self, schema: Schema) -> Result<()>;
   async fn get_schema(&self, id: i32) -> Result<Option<Schema>>;
   async fn list_schemas(&self) -> Result<Vec<Schema>>;
   async fn delete_schema(&self, subject: &str, version: i32) -> Result<()>;
   async fn save_subject_config(&self, subject: &str, config: SubjectConfig) -> Result<()>;
   async fn get_subject_config(&self, subject: &str) -> Result<Option<SubjectConfig>>;

2. Schema struct already has all needed fields:
   - id, version, subject, schema, schema_type, references

3. WAL command types needed:
   - RegisterSchema(Schema)
   - DeleteSchemaVersion(subject, version)
   - SetSubjectConfig(subject, SubjectConfig)

4. Recovery:
   - On startup, replay WAL to rebuild in-memory maps
   - Assign next_id from max(schema_ids) + 1

Dependencies:
- Schema and SchemaType structs defined
- SchemaReference for schema dependencies
- SubjectConfig for compatibility settings
```

**Effort Estimate**: 1-2 days

---

## Priority 3: Kafka API Completeness

### ~~9. Topic Deletion~~ ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - Full Kafka-compatible topic deletion implemented (v2.2.20)

**Implementation**:
- `WalMetadataStore::delete_topic()` now fully cleans up:
  - Topic metadata (`topics` map)
  - Partition assignments (`partition_assignments` DashMap)
  - Partition offsets (`partition_offsets` map)
  - Consumer offsets for the topic (`consumer_offsets` map)
  - Segment metadata (`segments` map)
- `handle_delete_topics()` in `chronik-protocol/src/handler.rs`:
  - Checks if topic exists before deletion
  - Returns proper error codes:
    - `NONE` (0) on success
    - `UNKNOWN_TOPIC_OR_PARTITION` (3) for non-existent topics
    - `UNKNOWN_SERVER_ERROR` (-1) on internal errors
  - Supports error messages for API version ≥ 5
- `TopicDeleted` event published to event bus for cluster replication
- Tested with Python kafka-python client

---

### ~~10. Consumer Group Enhancements~~ ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - All consumer group improvements implemented (v2.2.21)

**Implementation**:
- `persist_group()` in `consumer_group.rs` now properly converts members:
  - Converts `HashMap<String, GroupMember>` to `Vec<chronik_common::metadata::GroupMember>`
  - Serializes member assignment to Kafka wire format using `serialize_assignment()`
  - Includes metadata from protocols
- Partition leader lookup in `get_partitions_for_topics()`:
  - Calls `metadata_store.get_partition_leader()` for each partition
  - Returns proper leader ID in partition info
- Batch offset fetching in `fetch_offsets_optional()`:
  - Fetches all topics if none specified via `metadata_store.list_topics()`
  - Gets partition count from topic metadata
  - Queries `metadata_store.get_consumer_offset()` for each partition
  - Returns -1 offset for partitions with no committed offset

---

### ~~11. Configuration Updates~~ ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - AlterConfigs and IncrementalAlterConfigs now persist changes (v2.2.21)

**Implementation**:
- `handle_alter_configs()` in `chronik-protocol/src/handler.rs`:
  - Gets existing topic from metadata store
  - Applies all provided configs (retention.ms, segment.bytes, custom configs)
  - Validates config values before applying
  - Persists via `metadata_store.update_topic()`
  - Returns proper error codes for validation failures
- `handle_incremental_alter_configs()` already implemented (lines 3037-3224):
  - Supports SET, DELETE, APPEND, SUBTRACT operations
  - Per-config operation handling
  - Config validation and persistence
- Tested with Python kafka-python client

---

## Priority 4: Performance & Features

### ~~12. Vector Search HNSW (chronik-storage/src/vector_search.rs)~~ ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - Upgraded to real HNSW using `instant-distance` crate (v2.2.20)

**Implementation**:
- Added `instant-distance = "0.6"` dependency
- `RealHnswIndex` struct with HnswMap for O(log n) search
- Supports Euclidean, Cosine, and Inner Product distance metrics
- Configurable parameters (M=16, ef_construction=200, ef_search=50)
- Point wrappers: `EuclideanPoint`, `CosinePoint`, `DotProductPoint`
- Serialization via bincode for persistence
- Brute-force fallback when index not built
- 8 comprehensive tests including all distance metrics

---

### ~~13. Segment Cache Layer (chronik-storage/src/segment_cache.rs)~~ ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - Already implemented in `chronik-storage/src/segment_cache.rs`

**Implementation**:
- `SegmentCache` struct with LRU eviction
- `SegmentCacheConfig` for max_entries, max_size_bytes, TTL, warmup options
- `CacheStats` for hit rate, miss rate, eviction count, memory tracking
- `EvictionPolicy` enum (Lru, Lfu, Ttl)
- `CachedSegmentReader` wrapper for cached access
- Background eviction task for TTL enforcement
- Integration with `SegmentReader`
- Comprehensive test coverage

---

### ~~14. Benchmarks~~ ✅ IMPLEMENTED

**Status**: ✅ **COMPLETE** - Comprehensive benchmark suite available

**Implementation**:
- **chronik-bench** binary (`crates/chronik-bench/`):
  - Produce benchmark mode
  - Consume benchmark mode
  - Round-trip benchmark mode (end-to-end latency)
  - Metadata benchmark mode (topic create/delete)
  - HDR histogram for latency tracking (p50, p90, p95, p99, p999)
  - Progress reporting with rates and latencies
  - Configurable concurrency, compression, rate limiting

- **Storage benchmarks** (`crates/chronik-storage/benches/segment_comparison.rs`):
  - Segment build performance
  - Time range query performance
  - Bloom filter performance
  - Storage efficiency comparison
  - **NEW in v2.2.20**: Vector search benchmarks
    - HNSW vs brute-force search (1K, 10K, 50K vectors)
    - HNSW index build time

**Running benchmarks**:
```bash
# Run storage benchmarks
cargo bench -p chronik-storage

# Run Kafka protocol benchmarks
cargo run --bin chronik-bench -- --mode round-trip --topic bench-test
```

---

## Test Mock Implementations (Not Production Issues)

The 33 `unimplemented!()` macros in `coordinator_manager.rs` are in `MockMetadataStore` for unit tests only. These are intentional and don't need implementation.

---

## Summary

### Dead Code (~10,405 lines)

The codebase has significant dead code that should be cleaned up first:

| Location | Dead Files | Lines |
|----------|------------|-------|
| chronik-server/src/ | 10 | 3,963 |
| chronik-protocol/src/ | 11 | 4,292 |
| chronik-wal/src/ | 2 | 1,142 |
| chronik-auth/ (entire crate) | - | ~1,973 |
| **Total** | **23+** | **~10,405** |

### Working vs Dead Features

| Feature | Status | Location |
|---------|--------|----------|
| WAL Replication | ✅ **Working** | `chronik-server/src/wal_replication.rs` |
| SASL Auth (PLAIN, SCRAM) | ✅ **Working** | `chronik-protocol/src/sasl.rs` |
| Kafka Protocol Handler | ✅ **Working** | `chronik-server/src/kafka_handler.rs` |
| Transaction Coordinator | ❌ **Dead Code** | `chronik-protocol/src/transaction_coordinator.rs` |
| ACL Manager | ❌ **Dead Code** | `chronik-protocol/src/acl_manager.rs` |
| Schema Registry | ❌ **Dead Code** | `chronik-server/src/schema_registry.rs` |
| GSSAPI Auth | ❌ **Dead Crate** | `chronik-auth/` |

### Actual TODOs in Working Code

Many TODOs in the original analysis are in dead code. The actual pending work in **working code** includes:

| Area | Priority | Status |
|------|----------|--------|
| Segment checksums | P2 | ✅ **COMPLETE** (v2.2.18) |
| LZ4/Snappy compression | P2 | ✅ **COMPLETE** (v2.2.18) |
| Backup encryption | P1 | ✅ **COMPLETE** (v2.2.19) |
| Topic deletion | P3 | ✅ **COMPLETE** (v2.2.18) |
| Consumer group improvements | P3 | ✅ **COMPLETE** (v2.2.19) |
| Configuration updates | P3 | ✅ **COMPLETE** (v2.2.19) |
| Segment cache | P4 | ✅ **COMPLETE** (v2.2.20) |
| Vector search HNSW | P4 | ✅ **COMPLETE** (v2.2.20) |
| Benchmarks | P4 | ✅ **COMPLETE** (v2.2.20) |

---

## Recommended Action Order

### ~~Phase 0: Cleanup (1 day)~~ - ✅ COMPLETE
1. ~~**Delete all dead code** (~10,405 lines)~~ - Done
2. ~~Update this document to remove dead code references~~ - Done

### ~~Phase 1: Data Integrity (1-2 days)~~ - ✅ COMPLETE (v2.2.18)
3. ~~Segment checksums (P2, 1 day)~~ - Done
4. ~~LZ4/Snappy compression (P2, 0.5 days)~~ - Done

### ~~Phase 2: Security (2-3 days)~~ - ✅ COMPLETE (v2.2.19)
5. ~~Backup encryption (P1, 2 days)~~ - Done

### ~~Phase 3: Topic Deletion~~ - ✅ COMPLETE (v2.2.20)
6. ~~Topic deletion (P3, 2 days)~~ - Done

### ~~Phase 3b: API Completeness~~ - ✅ COMPLETE (v2.2.21)
7. ~~Configuration updates (P3, 2-3 days)~~ - Done
8. ~~Consumer group improvements (P3, 1 day)~~ - Done

### ~~Phase 4: Performance~~ - ✅ COMPLETE (v2.2.20)
9. ~~Segment cache (P4, 1-2 days)~~ - Done (already existed)
10. ~~Vector search HNSW (P4, 3-5 days)~~ - Done (upgraded to instant-distance)
11. ~~Benchmarks (P4)~~ - Done (added vector search benchmarks)

### Phase 5: Enterprise Features (if needed)
12. TLS for Kafka Protocol (2-3 days)
13. ACL Authorization System (3-5 days)
14. Schema Registry (5-7 days)
15. GSSAPI/Kerberos Authentication (5-7 days)

---

## Future Features (Not Started)

These features were **never implemented** - the deleted code was just non-functional stubs. Building these requires fresh implementation from scratch.

### TLS for Kafka Protocol

**Status**: ❌ Not implemented - Dead stub files were deleted in cleanup

**What Exists**:
- SASL authentication works (PLAIN, SCRAM-SHA-256/512) in `chronik-protocol/src/sasl.rs`
- Admin API has TLS env vars defined but warns "not available" (no actual TLS code)
- `rustls` is used by `opendal` for S3/GCS connections only

**Implementation Requirements**:
```
Effort: 2-3 days

1. Add dependencies to chronik-server/Cargo.toml:
   tokio-rustls = "0.25"
   rustls-pemfile = "2.0"

2. Create TLS configuration:
   pub struct TlsConfig {
       pub enabled: bool,
       pub cert_path: PathBuf,
       pub key_path: PathBuf,
       pub client_auth: Option<ClientAuthConfig>,  // Optional mTLS
   }

3. Environment variables:
   CHRONIK_TLS_ENABLED=true
   CHRONIK_TLS_CERT=/path/to/server.crt
   CHRONIK_TLS_KEY=/path/to/server.key
   CHRONIK_TLS_CA=/path/to/ca.crt  # For mTLS client verification

4. Modify integrated_server.rs:
   - Load certificates on startup
   - Create TlsAcceptor from rustls ServerConfig
   - Wrap TcpListener.accept() with TLS handshake
   - Handle TLS errors gracefully

5. Update connection handling:
   - Detect TLS vs plaintext (Kafka SASL_SSL vs SASL_PLAINTEXT)
   - Support both on different ports if needed

6. Testing:
   - Test with kafka-python ssl_context
   - Test with Java clients using SSL truststore
   - Test certificate rotation
```

**Files to Create/Modify**:
- `crates/chronik-server/src/tls.rs` (new)
- `crates/chronik-server/src/integrated_server/builder.rs`
- `crates/chronik-config/src/lib.rs`

---

### ACL Authorization System

**Status**: ❌ Not implemented - Dead stub files were deleted in cleanup

**What Exists**:
- SASL provides authenticated identity (username from PLAIN/SCRAM)
- No authorization checks anywhere in the codebase
- Kafka ACL APIs (CreateAcls, DeleteAcls, DescribeAcls) return errors

**Implementation Requirements**:
```
Effort: 3-5 days

1. Define ACL data structures:
   pub struct AclBinding {
       pub resource_type: ResourceType,    // Topic, Group, Cluster, TransactionalId
       pub resource_name: String,          // "*" for wildcard
       pub pattern_type: PatternType,      // Literal, Prefixed
       pub principal: String,              // "User:alice" or "User:*"
       pub host: String,                   // "*" for any
       pub operation: AclOperation,        // Read, Write, Create, Delete, Alter, Describe, All
       pub permission: AclPermission,      // Allow, Deny
   }

2. Add ACL storage to MetadataStore trait:
   async fn create_acl(&self, binding: AclBinding) -> Result<()>;
   async fn delete_acls(&self, filter: AclFilter) -> Result<Vec<AclBinding>>;
   async fn list_acls(&self, filter: AclFilter) -> Result<Vec<AclBinding>>;

3. Create AclManager:
   pub struct AclManager {
       acls: RwLock<Vec<AclBinding>>,
       metadata_store: Arc<dyn MetadataStore>,
       superusers: HashSet<String>,  // Users that bypass ACL checks
   }

   impl AclManager {
       pub fn check_permission(
           &self,
           principal: &str,
           resource: &Resource,
           operation: AclOperation,
       ) -> bool { ... }
   }

4. Integrate into request handlers:
   // In kafka_handler.rs, before processing each request:
   if !self.acl_manager.check_permission(&session.principal, &resource, operation) {
       return Err(Error::AuthorizationFailed(...));
   }

5. Implement Kafka ACL APIs:
   - CreateAcls (API Key 30)
   - DeleteAcls (API Key 31)
   - DescribeAcls (API Key 29)

6. Add WAL commands for ACL persistence:
   enum MetadataCommand {
       // ... existing commands
       CreateAcl(AclBinding),
       DeleteAcl(AclFilter),
   }

7. Configuration:
   CHRONIK_ACL_ENABLED=true
   CHRONIK_SUPERUSERS=admin,kafka  # Comma-separated list
```

**Files to Create/Modify**:
- `crates/chronik-server/src/acl.rs` (new)
- `crates/chronik-common/src/metadata/traits.rs`
- `crates/chronik-server/src/kafka_handler.rs`
- `crates/chronik-protocol/src/handler.rs`

---

### Schema Registry

**Status**: ❌ Not implemented - Dead stub files were deleted in cleanup

**What Exists**:
- No schema registry functionality
- Kafka clients that require schema registry will fail
- No Avro/Protobuf/JSON Schema support

**Implementation Requirements**:
```
Effort: 5-7 days

1. Add dependencies:
   apache-avro = "0.16"       # Avro schema parsing and validation
   prost = "0.12"             # Protobuf (already have for Raft)
   jsonschema = "0.17"        # JSON Schema validation

2. Define schema storage:
   pub struct Schema {
       pub id: i32,
       pub version: i32,
       pub subject: String,
       pub schema_type: SchemaType,  // Avro, Protobuf, JsonSchema
       pub schema: String,           # Raw schema definition
       pub references: Vec<SchemaReference>,
   }

3. Add to MetadataStore trait:
   async fn register_schema(&self, subject: &str, schema: &str, schema_type: SchemaType) -> Result<i32>;
   async fn get_schema(&self, id: i32) -> Result<Option<Schema>>;
   async fn get_schema_by_subject(&self, subject: &str, version: i32) -> Result<Option<Schema>>;
   async fn list_subjects(&self) -> Result<Vec<String>>;
   async fn list_versions(&self, subject: &str) -> Result<Vec<i32>>;
   async fn delete_subject(&self, subject: &str) -> Result<Vec<i32>>;
   async fn get_compatibility(&self, subject: &str) -> Result<CompatibilityLevel>;
   async fn set_compatibility(&self, subject: &str, level: CompatibilityLevel) -> Result<()>;

4. Implement REST API (separate HTTP server or same as Admin API):
   GET  /subjects                           # List all subjects
   GET  /subjects/{subject}/versions        # List versions for subject
   GET  /subjects/{subject}/versions/{ver}  # Get schema by subject+version
   POST /subjects/{subject}/versions        # Register new schema
   GET  /schemas/ids/{id}                   # Get schema by global ID
   POST /compatibility/subjects/{subject}/versions/{ver}  # Check compatibility
   GET  /config                             # Get global compatibility
   PUT  /config                             # Set global compatibility
   GET  /config/{subject}                   # Get subject compatibility
   PUT  /config/{subject}                   # Set subject compatibility
   DELETE /subjects/{subject}               # Delete subject

5. Implement compatibility checking:
   pub enum CompatibilityLevel {
       None,
       Backward,      # New schema can read old data
       Forward,       # Old schema can read new data
       Full,          # Both backward and forward
       BackwardTransitive,
       ForwardTransitive,
       FullTransitive,
   }

   fn check_compatibility(
       new_schema: &Schema,
       existing_schemas: &[Schema],
       level: CompatibilityLevel,
   ) -> Result<bool> { ... }

6. Optional: Integrate with Produce handler:
   - Validate messages against registered schemas
   - Add schema ID to message headers
   - Cache schemas for performance

7. Configuration:
   CHRONIK_SCHEMA_REGISTRY_ENABLED=true
   CHRONIK_SCHEMA_REGISTRY_PORT=8081
   CHRONIK_SCHEMA_COMPATIBILITY=BACKWARD  # Default compatibility level
```

**Files to Create**:
- `crates/chronik-server/src/schema_registry/mod.rs`
- `crates/chronik-server/src/schema_registry/api.rs`
- `crates/chronik-server/src/schema_registry/compatibility.rs`
- `crates/chronik-server/src/schema_registry/storage.rs`

---

### GSSAPI/Kerberos Authentication

**Status**: ❌ Not implemented - Dead crate `chronik-auth` was deleted

**What Exists**:
- SASL PLAIN and SCRAM-SHA-256/512 work in `chronik-protocol/src/sasl.rs`
- No Kerberos/GSSAPI support

**Implementation Requirements**:
```
Effort: 5-7 days (complex due to Kerberos infrastructure requirements)

1. Add dependencies:
   libgssapi = "0.6"  # Rust bindings to system GSSAPI

2. Requires external infrastructure:
   - Kerberos KDC (Key Distribution Center)
   - Service principal created: kafka/hostname@REALM
   - Keytab file for service authentication

3. Implement GSSAPI mechanism in sasl.rs:
   pub struct GssapiAuthenticator {
       service_principal: String,
       keytab_path: PathBuf,
   }

   impl GssapiAuthenticator {
       pub fn authenticate(&self, token: &[u8]) -> Result<(Vec<u8>, Option<String>)> {
           // Accept security context
           // Verify client principal
           // Return response token and authenticated principal
       }
   }

4. Multi-step authentication flow:
   - Client sends initial GSSAPI token
   - Server accepts context, may require multiple round-trips
   - Extract authenticated principal name

5. Configuration:
   CHRONIK_SASL_GSSAPI_KEYTAB=/etc/security/keytabs/kafka.keytab
   CHRONIK_SASL_GSSAPI_PRINCIPAL=kafka/hostname@REALM

Recommendation: Lower priority unless enterprise Kerberos integration is required.
PLAIN + SCRAM covers most use cases. GSSAPI adds significant operational complexity.
```

---

## Notes

- All effort estimates assume familiarity with the codebase
- Dependencies are mostly internal; no major external crates needed
- Most stubs have well-defined interfaces; implementation is straightforward
- Test coverage exists for some areas; new tests needed for others
