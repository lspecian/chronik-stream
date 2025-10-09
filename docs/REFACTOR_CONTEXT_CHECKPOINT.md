# Refactor Context Checkpoint - 2025-10-07

## Current Status

**Phase 1**: ✅ COMPLETE
**Phase 1.5** (CRC Fix Integration): ✅ COMPLETE
**Phase 2**: ⏳ IN PROGRESS (Tantivy segment storage - API compatibility issues)

## What's Been Completed

### Phase 1: CanonicalRecord (✅ 100% Complete)
- File: `crates/chronik-storage/src/canonical_record.rs` (1088 lines)
- All 9 unit tests passing
- Deterministic CRC calculation verified
- Round-trip testing validated

### Phase 1.5: CRC Fix Integration (✅ Complete)
- File: `crates/chronik-server/src/produce_handler.rs`
- Lines modified: ~738-763, 840, 884, 943
- **Implementation:**
  ```rust
  // Decode incoming batch
  let canonical_record = CanonicalRecord::from_kafka_batch(records_data)?;

  // Re-encode with correct CRC
  let re_encoded_bytes = canonical_record.to_kafka_batch()?;

  // Use re-encoded bytes everywhere (buffering, replication, fetch updates)
  raw_bytes: re_encoded_bytes.to_vec()
  ```
- **Status:** Compiled successfully, ready for testing

### Phase 2: Tantivy Segment Storage (⏳ In Progress)
- **Goal:** Store CanonicalRecord in Tantivy indexes for searchable storage
- **Problem:** Hit Tantivy 0.24 API compatibility issues + file corruption from system reminders in Edit tool
- **Next Steps:** Recreate tantivy_segment.rs with correct Tantivy 0.24 API usage

## Technical Details for Phase 2

### Tantivy 0.24 API Specifics
```rust
// CORRECT imports for Tantivy 0.24:
use tantivy::{
    schema::*, Index, IndexWriter, IndexReader,
    collector::TopDocs, query::RangeQuery, Term,
    DateTime as TantivyDateTime,
};
use tantivy::schema::Document as TantivyDoc;  // Document is schema::Document in 0.24

// CORRECT Document creation:
let mut doc = TantivyDoc::default();  // NOT Document::new()

// CORRECT RangeQuery for i64:
let start_term = Term::from_field_i64(field, start_value);
let end_term = Term::from_field_i64(field, end_value);
let query = RangeQuery::new(
    std::ops::Bound::Included(start_term),
    std::ops::Bound::Excluded(end_term),
);

// CORRECT RangeQuery for DateTime:
let start_term = Term::from_field_date(field, tantivy_dt);
let end_term = Term::from_field_date(field, tantivy_dt);
let query = RangeQuery::new(
    std::ops::Bound::Included(start_term),
    std::ops::Bound::Excluded(end_term),
);

// DateTime conversion (chrono::DateTime<Utc> → TantivyDateTime):
let tantivy_dt = TantivyDateTime::from_timestamp_millis(timestamp_ms);
```

### Schema Design for CanonicalRecord
```rust
pub struct SchemaFields {
    // Record-level (indexed for queries)
    pub offset: Field,           // i64, INDEXED | STORED | FAST
    pub timestamp: Field,         // Date, INDEXED | STORED | FAST
    pub key: Field,               // Bytes, STORED
    pub value: Field,             // Bytes, STORED
    pub headers_json: Field,      // Text, TEXT | STORED
    pub attributes: Field,        // i64, STORED

    // Batch-level metadata (stored for reconstruction)
    pub base_offset: Field,
    pub partition_leader_epoch: Field,
    pub producer_id: Field,       // i64, STORED | FAST (for idempotence queries)
    pub producer_epoch: Field,
    pub base_sequence: Field,
    pub sequence: Field,          // Per-record sequence number
    pub is_transactional: Field,  // bool, STORED
    pub is_control: Field,
    pub compression: Field,       // u64 enum, STORED
    pub timestamp_type: Field,
}
```

### Module Structure
```rust
pub struct TantivySegmentWriter {
    index: Index,
    writer: IndexWriter,
    schema_fields: SchemaFields,
    metadata: SegmentMetadata,
    records_written: usize,
}

impl TantivySegmentWriter {
    pub fn new(topic: String, partition: i32, base_offset: i64) -> Result<Self>;
    pub fn write_batch(&mut self, canonical: &CanonicalRecord) -> Result<()>;
    fn write_record(&mut self, canonical: &CanonicalRecord, record: &CanonicalRecordEntry) -> Result<()>;
    pub fn commit(self) -> Result<(Index, SegmentMetadata)>;
}

pub struct TantivySegmentReader {
    index: Index,
    reader: IndexReader,
    schema_fields: SchemaFields,
    metadata: SegmentMetadata,
}

impl TantivySegmentReader {
    pub fn open(index_path: &Path, metadata: SegmentMetadata) -> Result<Self>;
    pub fn query_by_offset_range(&self, start: i64, end: i64) -> Result<Vec<CanonicalRecordEntry>>;
    pub fn query_by_timestamp_range(&self, start_ts: i64, end_ts: i64) -> Result<Vec<CanonicalRecordEntry>>;
    fn doc_to_record(&self, doc: &TantivyDoc) -> Result<CanonicalRecordEntry>;
}
```

## Remaining Work

### Immediate (Phase 2)
1. ✅ Kill old background server processes
2. ⏳ Recreate `tantivy_segment.rs` with correct Tantivy 0.24 API
3. ⏳ Write unit tests for TantivySegmentWriter
4. ⏳ Implement segment serialization (tar.gz packaging)
5. ⏳ Implement object store upload/download

### Phase 3: WAL Refactor (~2 days)
- Modify `chronik-wal/src/wal_record.rs` to store CanonicalRecord
- Update `wal/manager.rs` for canonical format
- Handle version migration (v2 → v3)

### Phase 4: WAL Indexer (~2 days)
- Create `chronik-storage/src/wal_indexer.rs`
- Background task: WAL segment → Tantivy segment
- Implement automatic indexing on WAL seal

### Phase 5: Segment Index (~1 day)
- Create `chronik-storage/src/segment_index.rs`
- Registry of all Tantivy segments per topic-partition
- Query router: WAL (recent) + Tantivy segments (historical)

### Phase 6: Fetch Handler Refactor (~3 days)
- Modify `chronik-server/src/fetch_handler.rs`
- Query WAL + Tantivy for offset range
- Reconstruct CanonicalRecord → encode → return

### Phase 7: Produce Handler Final (~2 days)
- Full integration with WAL indexer
- Remove old dual-storage code

### Phase 8: Integration Testing (~3 days)
- Test with Java/Python/Go clients
- Performance benchmarking
- CRC validation with real clients

### Phase 9: Cleanup (~1 day)
- Remove old segment formats (v1/v2/v3)
- Update documentation
- Final release

## Files Modified So Far

### Created
- `crates/chronik-storage/src/canonical_record.rs` (1088 lines) ✅
- `crates/chronik-storage/src/tantivy_segment.rs` (CORRUPTED - needs recreation)
- `docs/REFACTOR_PLAN_LAYERED_STORAGE.md` (6000+ lines)
- `docs/REFACTOR_IMPLEMENTATION_TRACKER.md` (600+ lines)
- `docs/releases/PHASE1_COMPLETE.md`
- `tests/integration/canonical_crc_test.rs` (247 lines)

### Modified
- `crates/chronik-storage/src/lib.rs` (added canonical_record, tantivy_segment exports)
- `crates/chronik-server/src/produce_handler.rs` (CRC fix integration)
- `tests/integration/mod.rs` (registered canonical_crc_test)

## Background Processes to Clean Up

Kill these before continuing:
- Shell 5559e9
- Shell d1c914
- Shell 41d331
- Shell ffe274
- Shell f9085f

Use: `pkill -9 -f chronik-server`

## Next Action

**Recreate `tantivy_segment.rs` with:**
1. Correct Tantivy 0.24 API usage (see above)
2. Full Writer implementation
3. Full Reader implementation
4. Unit tests
5. Export in lib.rs

**Then continue with Phase 2.3:** Segment serialization (tar.gz)
