# Chronik Stream - Layered Storage Architecture Refactor

**Version:** v2.0.0 (Major Breaking Change)
**Date:** 2025-10-07
**Status:** PLANNING

---

## Executive Summary

This refactor eliminates the dual storage architecture (raw_kafka_batches + indexed_records) and replaces it with a clean layered storage model inspired by Quickwit/Lucene architecture. The goal is to **solve the CRC corruption bug permanently** while improving storage efficiency and search capabilities.

### Core Principle
**"Encode once, decode once, CRC matches"**

We will:
1. Receive Kafka wire format from clients
2. **Decode to internal canonical format** (preserving all fields precisely)
3. Store in Tantivy segments (searchable, structured)
4. **Re-encode to Kafka wire format** on fetch (achieving identical CRC)

---

## Problem Analysis

### Current Architecture Issues

1. **Dual Storage Complexity**
   - `raw_kafka_batches`: Attempts to preserve original wire bytes (but fails with CRC corruption)
   - `indexed_records`: Structured format for search
   - Storage overhead: 2x data size
   - Synchronization issues between formats

2. **CRC Corruption Root Cause**
   - We try to preserve "raw" bytes but they get corrupted somewhere in the pipeline
   - Batch concatenation issues
   - Byte-level corruption during storage/retrieval
   - No deterministic encode/decode cycle

3. **Architectural Mess**
   - `segment.rs`: v1, v2, v3 formats all coexisting
   - `segment_writer.rs`: `write_dual_format()` complexity
   - Optional Tantivy integration makes code paths complex
   - No clear separation between WAL and searchable storage

### Why Current CRC Attempts Failed

**v1.3.29-v1.3.33 attempts:**
- Tried to preserve raw Kafka wire bytes
- Failed because wire format is NOT our internal representation
- Byte-level copying introduces subtle corruption
- No guarantee that decode(encode(x)) == x

**The Solution:**
- DON'T preserve raw bytes
- Instead: **Implement deterministic encode/decode**
- Store canonical internal format
- Re-encode identically on fetch

---

## New Architecture: Layered Storage

```
┌─────────────────────────────────────────────────────────────┐
│                    KAFKA PROTOCOL LAYER                      │
│  (Receive: Kafka Wire Format → Decode to Internal Format)   │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                     WAL (Durability)                         │
│  - Write decoded records in internal format                  │
│  - Crash recovery replays to restore state                   │
│  - Segments rotate based on size/time                        │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                 WAL INDEXER (Background Task)                │
│  - Periodically reads sealed WAL segments                    │
│  - Transforms to Tantivy indexes                             │
│  - Updates segment index registry                            │
│  - Deletes old WAL after successful index                    │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│              TANTIVY SEGMENTS (Object Store)                 │
│  - Searchable, compressed, indexed                           │
│  - Schema: _offset, _ts, _key, _value, _headers             │
│  - Stored in object store (S3/GCS/Local)                     │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                    SEGMENT INDEX                             │
│  - Registry of all segments per topic-partition              │
│  - Offset ranges, timestamp ranges                           │
│  - Enables efficient range queries                           │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                    FETCH HANDLER                             │
│  1. Check active WAL (recent data)                           │
│  2. Query Tantivy segments (older data)                      │
│  3. Re-encode to Kafka wire format with correct CRC          │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                    KAFKA PROTOCOL LAYER                      │
│  (Return: Kafka Wire Format with CRC that matches original)  │
└─────────────────────────────────────────────────────────────┘
```

---

## Implementation Plan

### Phase 1: Internal Record Format (Core Foundation)

**Goal:** Define canonical internal representation that preserves ALL Kafka record fields.

**File:** `crates/chronik-storage/src/canonical_record.rs` (NEW)

```rust
/// Canonical internal record format that preserves all Kafka RecordBatch fields
/// This is the single source of truth for record storage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalRecord {
    // Batch-level fields
    pub base_offset: i64,
    pub partition_leader_epoch: i32,
    pub producer_id: i64,
    pub producer_epoch: i16,
    pub base_sequence: i32,
    pub is_transactional: bool,
    pub is_control: bool,
    pub compression: CompressionType,
    pub timestamp_type: TimestampType,

    // Record-level fields (per message in batch)
    pub records: Vec<CanonicalRecordEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalRecordEntry {
    pub offset: i64,  // Absolute offset
    pub timestamp: i64,  // Absolute timestamp
    pub key: Option<Bytes>,
    pub value: Option<Bytes>,
    pub headers: Vec<(String, Option<Bytes>)>,
}

impl CanonicalRecord {
    /// Decode from Kafka wire format (preserve ALL fields)
    pub fn from_kafka_batch(wire_bytes: &[u8]) -> Result<Self>;

    /// Encode to Kafka wire format (deterministic, CRC matches)
    pub fn to_kafka_batch(&self) -> Result<Bytes>;
}
```

**Key Principle:**
- `from_kafka_batch(x).to_kafka_batch()` MUST produce identical bytes (including CRC)
- This is achievable because we preserve ALL fields with NO lossy transformations

**Tests:**
- Round-trip test: `assert_eq!(original_bytes, decode(encode(decode(original_bytes))))`
- CRC validation: Decode → Encode → Verify CRC matches
- Test with real Kafka producer output

---

### Phase 2: Tantivy Schema & Indexing

**Goal:** Store `CanonicalRecord` in Tantivy segments for efficient querying.

**File:** `crates/chronik-storage/src/tantivy_segment.rs` (NEW)

```rust
/// Tantivy schema for Chronik records
pub fn build_chronik_schema() -> (Schema, SchemaFields) {
    let mut schema_builder = Schema::builder();

    SchemaFields {
        offset: schema_builder.add_i64_field("_offset", INDEXED | STORED),
        timestamp: schema_builder.add_date_field("_ts", INDEXED | STORED),
        key: schema_builder.add_bytes_field("_key", STORED),
        value: schema_builder.add_bytes_field("_value", STORED),
        headers: schema_builder.add_json_field("_headers", TEXT | STORED),
        // Batch metadata
        producer_id: schema_builder.add_i64_field("_producer_id", INDEXED | STORED),
        base_sequence: schema_builder.add_i64_field("_base_sequence", INDEXED | STORED),
        is_transactional: schema_builder.add_bool_field("_transactional", INDEXED),
        // ... other fields
    }

    (schema_builder.build(), fields)
}

pub struct TantivySegmentWriter {
    index: Index,
    writer: IndexWriter,
    schema: Schema,
    fields: SchemaFields,
}

impl TantivySegmentWriter {
    /// Write a CanonicalRecord to the Tantivy index
    pub fn write_record(&mut self, record: &CanonicalRecord) -> Result<()> {
        for entry in &record.records {
            let mut doc = Document::new();
            doc.add_i64(self.fields.offset, entry.offset);
            doc.add_date(self.fields.timestamp, DateTime::from_timestamp_millis(entry.timestamp));
            if let Some(key) = &entry.key {
                doc.add_bytes(self.fields.key, key.as_ref());
            }
            if let Some(value) = &entry.value {
                doc.add_bytes(self.fields.value, value.as_ref());
            }
            // Add batch metadata
            doc.add_i64(self.fields.producer_id, record.producer_id);
            doc.add_bool(self.fields.is_transactional, record.is_transactional);
            // ...

            self.writer.add_document(doc)?;
        }
        Ok(())
    }

    /// Commit and serialize the index to object store
    pub fn commit_to_object_store(self, object_store: &ObjectStore, path: &str) -> Result<SegmentMetadata>;
}
```

**Segment Storage Location:**
```
{object_store_root}/
  segments/
    {topic}/
      {partition}/
        {segment_id}/
          tantivy_index.tar.gz  (compressed Tantivy directory)
          metadata.json          (SegmentMetadata)
```

---

### Phase 3: WAL Refactor

**Goal:** WAL writes `CanonicalRecord` instead of raw wire bytes.

**File:** `crates/chronik-wal/src/wal_record.rs` (MODIFY)

```rust
/// WAL record types (v2 format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalRecord {
    /// Produce: Store canonical record for recovery
    Produce {
        topic: String,
        partition: i32,
        canonical_record: CanonicalRecord,  // Changed from raw bytes
    },
    // ... other record types unchanged
}
```

**File:** `crates/chronik-wal/src/manager.rs` (MODIFY)

```rust
impl WalManager {
    /// Append a produce record (stores canonical format)
    pub async fn append_produce(
        &mut self,
        topic: &str,
        partition: i32,
        canonical_record: CanonicalRecord,
    ) -> Result<u64> {
        let record = WalRecord::Produce {
            topic: topic.to_string(),
            partition,
            canonical_record,
        };
        self.append_record(record).await
    }

    /// Recovery: Reconstruct state from canonical records
    pub async fn recover(&mut self) -> Result<Vec<(String, i32, CanonicalRecord)>> {
        // Read all sealed segments
        // Deserialize CanonicalRecord from each entry
        // Return for replay
    }
}
```

---

### Phase 4: WAL Indexer (Background Task)

**Goal:** Periodically convert sealed WAL segments to Tantivy indexes.

**File:** `crates/chronik-storage/src/wal_indexer.rs` (NEW)

```rust
pub struct WalIndexer {
    wal_manager: Arc<RwLock<WalManager>>,
    object_store: Arc<ObjectStore>,
    segment_index: Arc<RwLock<SegmentIndex>>,
    config: WalIndexerConfig,
}

impl WalIndexer {
    /// Background task that runs periodically
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.config.index_interval);

        loop {
            interval.tick().await;

            if let Err(e) = self.index_sealed_segments().await {
                error!("WAL indexing failed: {}", e);
            }
        }
    }

    /// Index all sealed WAL segments
    async fn index_sealed_segments(&self) -> Result<()> {
        // 1. Get list of sealed WAL segments
        let sealed_segments = self.wal_manager.read().await.get_sealed_segments()?;

        for wal_segment in sealed_segments {
            // 2. Read canonical records from WAL
            let records = wal_segment.read_all_records()?;

            // 3. Create Tantivy index
            let mut tantivy_writer = TantivySegmentWriter::new()?;
            for (topic, partition, canonical_record) in records {
                tantivy_writer.write_record(&canonical_record)?;
            }

            // 4. Commit to object store
            let segment_metadata = tantivy_writer.commit_to_object_store(
                &self.object_store,
                &format!("segments/{}/{}/{}", topic, partition, segment_id)
            )?;

            // 5. Update segment index
            self.segment_index.write().await.add_segment(segment_metadata)?;

            // 6. Delete WAL segment (data now safely in Tantivy)
            wal_segment.delete()?;

            info!("Indexed WAL segment {} to Tantivy", wal_segment.id());
        }

        Ok(())
    }
}
```

---

### Phase 5: Segment Index

**Goal:** Maintain registry of all Tantivy segments for efficient range queries.

**File:** `crates/chronik-storage/src/segment_index.rs` (NEW)

```rust
/// Metadata for a Tantivy segment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMetadata {
    pub segment_id: Uuid,
    pub topic: String,
    pub partition: i32,
    pub offset_range: (i64, i64),  // (min_offset, max_offset)
    pub timestamp_range: (i64, i64),  // (min_ts, max_ts)
    pub record_count: u64,
    pub size_bytes: u64,
    pub object_store_path: String,
    pub created_at: i64,
}

/// In-memory + persistent index of all segments
pub struct SegmentIndex {
    /// Segments organized by topic-partition
    segments: HashMap<(String, i32), Vec<SegmentMetadata>>,
    /// Persistent storage (JSON file or metadata store)
    persistence: Arc<dyn SegmentIndexPersistence>,
}

impl SegmentIndex {
    /// Find segments that overlap with the given offset range
    pub fn find_segments_by_offset(
        &self,
        topic: &str,
        partition: i32,
        start_offset: i64,
        end_offset: i64,
    ) -> Vec<SegmentMetadata>;

    /// Find segments that overlap with the given timestamp range
    pub fn find_segments_by_timestamp(
        &self,
        topic: &str,
        partition: i32,
        start_ts: i64,
        end_ts: i64,
    ) -> Vec<SegmentMetadata>;
}
```

---

### Phase 6: Fetch Handler Refactor

**Goal:** Fetch from WAL (recent) + Tantivy (older), then re-encode to Kafka format.

**File:** `crates/chronik-server/src/fetch_handler.rs` (MAJOR REFACTOR)

```rust
impl FetchHandler {
    /// Fetch records for a partition
    pub async fn fetch_partition(
        &self,
        topic: &str,
        partition: i32,
        fetch_offset: i64,
        max_bytes: i32,
    ) -> Result<FetchResponsePartition> {
        let mut canonical_records = Vec::new();
        let mut bytes_fetched = 0;

        // PHASE 1: Try active WAL segment first (most recent data)
        if let Some(wal_records) = self.fetch_from_wal(topic, partition, fetch_offset).await? {
            for canonical_record in wal_records {
                canonical_records.push(canonical_record);
                bytes_fetched += canonical_record.estimated_size();

                if bytes_fetched >= max_bytes as usize {
                    break;
                }
            }
        }

        // PHASE 2: If not enough data, fetch from Tantivy segments
        if bytes_fetched < max_bytes as usize {
            let segments = self.segment_index.find_segments_by_offset(
                topic,
                partition,
                fetch_offset,
                i64::MAX,
            );

            for segment_metadata in segments {
                let tantivy_records = self.read_from_tantivy_segment(
                    &segment_metadata,
                    fetch_offset,
                    max_bytes - bytes_fetched as i32,
                ).await?;

                canonical_records.extend(tantivy_records);
                bytes_fetched += tantivy_records.iter()
                    .map(|r| r.estimated_size())
                    .sum::<usize>();

                if bytes_fetched >= max_bytes as usize {
                    break;
                }
            }
        }

        // PHASE 3: Re-encode to Kafka wire format (with correct CRC)
        let kafka_batches: Vec<Bytes> = canonical_records.iter()
            .map(|r| r.to_kafka_batch())
            .collect::<Result<Vec<_>>>()?;

        // Concatenate all batches (Kafka fetch response supports multiple batches)
        let mut records_bytes = BytesMut::new();
        for batch in kafka_batches {
            records_bytes.extend_from_slice(&batch);
        }

        Ok(FetchResponsePartition {
            partition,
            error_code: 0,
            high_watermark,
            records: records_bytes.freeze().to_vec(),
            // ...
        })
    }

    /// Read records from a Tantivy segment
    async fn read_from_tantivy_segment(
        &self,
        segment_metadata: &SegmentMetadata,
        start_offset: i64,
        max_bytes: i32,
    ) -> Result<Vec<CanonicalRecord>> {
        // 1. Download Tantivy index from object store
        let index_bytes = self.object_store.get(&segment_metadata.object_store_path).await?;

        // 2. Deserialize Tantivy index
        let temp_dir = TempDir::new()?;
        extract_tantivy_tar(&index_bytes, temp_dir.path())?;
        let index = Index::open_in_dir(temp_dir.path())?;
        let reader = index.reader()?;
        let searcher = reader.searcher();

        // 3. Query for records in offset range
        let query = RangeQuery::new_i64_bounds(
            "_offset".to_string(),
            Bound::Included(start_offset),
            Bound::Unbounded,
        );

        let top_docs = searcher.search(&query, &TopDocs::with_limit(max_bytes as usize))?;

        // 4. Reconstruct CanonicalRecord from Tantivy documents
        let mut records = Vec::new();
        for (_score, doc_address) in top_docs {
            let doc = searcher.doc(doc_address)?;
            let canonical_record = CanonicalRecord::from_tantivy_doc(&doc, &self.schema)?;
            records.push(canonical_record);
        }

        Ok(records)
    }
}
```

---

### Phase 7: Produce Handler Refactor

**Goal:** Decode Kafka wire → Store CanonicalRecord in WAL.

**File:** `crates/chronik-server/src/produce_handler.rs` (MAJOR REFACTOR)

```rust
impl ProduceHandler {
    pub async fn handle_partition_produce(
        &self,
        topic: &str,
        partition: i32,
        records_data: &[u8],  // Kafka wire format from client
    ) -> Result<ProduceResponsePartition> {
        // 1. Decode Kafka wire format to CanonicalRecord
        let canonical_record = CanonicalRecord::from_kafka_batch(records_data)?;

        // 2. Assign offsets
        let base_offset = self.get_next_offset(topic, partition).await?;
        let mut canonical_record = canonical_record;
        for (i, entry) in canonical_record.records.iter_mut().enumerate() {
            entry.offset = base_offset + i as i64;
        }

        // 3. Write to WAL (durability)
        self.wal_manager.write().await.append_produce(
            topic,
            partition,
            canonical_record.clone(),
        ).await?;

        // 4. Add to in-memory buffer (for immediate fetch)
        self.add_to_buffer(topic, partition, canonical_record).await;

        // 5. Update high watermark
        let last_offset = base_offset + canonical_record.records.len() as i64 - 1;
        self.update_high_watermark(topic, partition, last_offset).await?;

        Ok(ProduceResponsePartition {
            partition,
            error_code: 0,
            base_offset,
            log_append_time_ms: -1,
            log_start_offset: 0,
        })
    }
}
```

---

## Files to Delete (Cleanup)

After refactor, these files/features become obsolete:

1. **Dual Storage Code:**
   - `segment.rs`: Remove `raw_kafka_batches` field, `add_raw_kafka_batch()` methods
   - `segment_writer.rs`: Remove `write_dual_format()`, replace with `write_canonical()`
   - Remove `--dual-storage` flag

2. **Legacy Segment Formats:**
   - Remove v1, v2 compatibility code
   - Keep only v4 (Tantivy-based)

3. **Experimental Files:**
   - Delete any CRC preservation attempts from v1.3.29-v1.3.33

---

## Testing Strategy

### Unit Tests

1. **Canonical Format Round-Trip:**
   ```rust
   #[test]
   fn test_canonical_roundtrip() {
       let original_kafka_bytes = produce_kafka_batch_from_real_client();
       let canonical = CanonicalRecord::from_kafka_batch(&original_kafka_bytes).unwrap();
       let re_encoded = canonical.to_kafka_batch().unwrap();
       assert_eq!(original_kafka_bytes, re_encoded);
   }
   ```

2. **CRC Validation:**
   ```rust
   #[test]
   fn test_crc_deterministic() {
       let canonical = create_test_canonical_record();
       let encoded1 = canonical.to_kafka_batch().unwrap();
       let encoded2 = canonical.to_kafka_batch().unwrap();
       assert_eq!(encoded1, encoded2);  // Deterministic

       // Verify CRC is valid
       let crc = extract_crc_from_batch(&encoded1);
       let computed_crc = compute_crc(&encoded1);
       assert_eq!(crc, computed_crc);
   }
   ```

3. **WAL → Tantivy → Fetch:**
   ```rust
   #[test]
   async fn test_wal_indexer_integration() {
       // 1. Produce records (written to WAL)
       let producer = create_test_producer();
       producer.send(topic, records).await.unwrap();

       // 2. Trigger WAL indexing
       wal_indexer.index_sealed_segments().await.unwrap();

       // 3. Fetch records (should read from Tantivy)
       let consumer = create_test_consumer();
       let fetched = consumer.fetch(topic, 0).await.unwrap();

       // 4. Verify CRC matches
       verify_crc_valid(&fetched);
   }
   ```

### Integration Tests

**Test with Real Kafka Clients:**

```bash
# 1. Start Chronik with refactored storage
cargo run --release --bin chronik-server -- standalone

# 2. Produce with Java client
java -cp ksql/confluent-7.5.0/share/java/kafka/*:. TestCRCValidation localhost:9092

# Expected: ✓ ALL TESTS PASSED - CRC validation working correctly!
```

**Test with KSQLDB:**

```sql
-- Create stream
CREATE STREAM test_stream (id INT, name VARCHAR) WITH (kafka_topic='test', value_format='JSON');

-- Insert data
INSERT INTO test_stream VALUES (1, 'Alice');

-- Query (triggers fetch from Tantivy segments)
SELECT * FROM test_stream EMIT CHANGES;

-- Expected: Records returned with valid CRC
```

---

## Migration Guide

### For Existing Deployments

**⚠️ BREAKING CHANGE:** v2.0.0 requires data migration.

**Option 1: Fresh Start (Recommended for Dev/Test)**
```bash
# Backup old data
mv data data.backup

# Start with new storage
cargo run --release --bin chronik-server -- standalone
```

**Option 2: Data Migration (Production)**
```bash
# Run migration tool
cargo run --release --bin chronik-migrate -- \
  --old-data ./data \
  --new-data ./data-v2 \
  --preserve-offsets

# Verify migration
cargo run --release --bin chronik-verify -- --data ./data-v2

# Switch to new version
mv data data.old && mv data-v2 data
```

---

## Performance Expectations

### Storage Efficiency

| Metric | Old (Dual Storage) | New (Tantivy) | Improvement |
|--------|-------------------|---------------|-------------|
| Disk Usage | 2x (raw + indexed) | 1x (compressed Tantivy) | **50% reduction** |
| Write Amplification | High (write twice) | Low (write once) | **2x faster writes** |
| Query Performance | Slow (scan raw bytes) | Fast (indexed) | **10-100x faster** |

### Memory Usage

- **WAL buffer:** 10-100 MB (configurable)
- **Tantivy cache:** 50-500 MB (configurable)
- **Segment index:** 1-10 MB (in-memory metadata)

### Latency

- **Produce:** <5ms (WAL write only)
- **Fetch (recent):** <1ms (from WAL buffer)
- **Fetch (older):** <50ms (from Tantivy, includes object store latency)

---

## Success Criteria

✅ **CRC Validation:** Java clients pass CRC validation (0 errors)
✅ **KSQLDB Compatibility:** All KSQL operations work without errors
✅ **Storage Efficiency:** 50% reduction in disk usage vs. dual storage
✅ **Search Performance:** 10x faster range queries vs. scanning
✅ **Zero Data Loss:** WAL ensures durability, segments are searchable
✅ **Clean Architecture:** No dual storage, single storage format

---

## Timeline Estimate

- **Phase 1:** Canonical Format - 2 days
- **Phase 2:** Tantivy Integration - 3 days
- **Phase 3:** WAL Refactor - 2 days
- **Phase 4:** WAL Indexer - 2 days
- **Phase 5:** Segment Index - 1 day
- **Phase 6:** Fetch Handler - 3 days
- **Phase 7:** Produce Handler - 2 days
- **Testing & Debugging:** 3 days
- **Documentation:** 1 day

**Total:** ~19 days (3-4 weeks)

---

## Risk Mitigation

### Risk 1: Encode/Decode CRC Mismatch

**Mitigation:** Extensive unit tests with real Kafka client data, fuzzing

### Risk 2: Tantivy Performance Issues

**Mitigation:** Benchmark early, optimize indexing strategy, use compression

### Risk 3: Data Loss During Migration

**Mitigation:** Run migration tool in dry-run mode, verify before switching

---

## Open Questions

1. **Compression:** Should we compress Tantivy indexes before uploading to object store?
   - **Recommendation:** Yes, use tar.gz for ~50% size reduction

2. **Segment Compaction:** When to merge small Tantivy segments?
   - **Recommendation:** Background task merges segments when >10 segments per partition

3. **WAL Retention:** How long to keep WAL after indexing?
   - **Recommendation:** Delete immediately after successful Tantivy upload + verification

4. **Object Store Caching:** Should we cache Tantivy indexes locally?
   - **Recommendation:** Yes, LRU cache for recently accessed segments

---

## Conclusion

This refactor:
- ✅ **Solves CRC corruption permanently** through deterministic encode/decode
- ✅ **Eliminates dual storage complexity**
- ✅ **Improves storage efficiency** by 50%
- ✅ **Enables fast search** with Tantivy
- ✅ **Maintains Kafka protocol compatibility**
- ✅ **Follows industry best practices** (Quickwit/Lucene architecture)

**Next Step:** Review this plan, then proceed with implementation starting from Phase 1.
