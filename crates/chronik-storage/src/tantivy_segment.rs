//! Tantivy-based segment storage for CanonicalRecord (Tantivy 0.24 API).

use crate::canonical_record::{CanonicalRecord, CanonicalRecordEntry, RecordHeader, CompressionType, TimestampType};
use crate::object_store::{ObjectStore, PutOptions};
use chronik_common::{Result, Error};
use tantivy::{
    doc, schema::*, Index, IndexWriter, IndexReader,
    collector::TopDocs, query::RangeQuery, Term,
    DateTime as TantivyDateTime, TantivyDocument,
};
use serde::{Serialize, Deserialize};
use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{Write, Read};
use std::sync::Arc;
use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use flate2::read::GzDecoder;

/// Tantivy schema fields for storing CanonicalRecord
#[derive(Debug, Clone)]
pub struct SchemaFields {
    pub offset: Field,
    pub timestamp: Field,
    pub key: Field,
    pub value: Field,
    pub headers_json: Field,
    pub attributes: Field,
    pub base_offset: Field,
    pub partition_leader_epoch: Field,
    pub producer_id: Field,
    pub producer_epoch: Field,
    pub base_sequence: Field,
    pub sequence: Field,
    pub is_transactional: Field,
    pub is_control: Field,
    pub compression: Field,
    pub timestamp_type: Field,
}

impl SchemaFields {
    pub fn build_schema() -> (Schema, Self) {
        let mut schema_builder = Schema::builder();

        let offset = schema_builder.add_i64_field("_offset", INDEXED | STORED | FAST);
        let timestamp = schema_builder.add_date_field("_ts", INDEXED | STORED | FAST);
        let key = schema_builder.add_bytes_field("_key", STORED);
        let value = schema_builder.add_bytes_field("_value", STORED);
        let headers_json = schema_builder.add_text_field("_headers", TEXT | STORED);
        let attributes = schema_builder.add_i64_field("_attributes", STORED);
        let base_offset = schema_builder.add_i64_field("_base_offset", STORED);
        let partition_leader_epoch = schema_builder.add_i64_field("_partition_leader_epoch", STORED);
        let producer_id = schema_builder.add_i64_field("_producer_id", STORED | FAST);
        let producer_epoch = schema_builder.add_i64_field("_producer_epoch", STORED);
        let base_sequence = schema_builder.add_i64_field("_base_sequence", STORED);
        let sequence = schema_builder.add_i64_field("_sequence", STORED);
        let is_transactional = schema_builder.add_bool_field("_is_transactional", STORED);
        let is_control = schema_builder.add_bool_field("_is_control", STORED);
        let compression = schema_builder.add_u64_field("_compression", STORED);
        let timestamp_type = schema_builder.add_u64_field("_timestamp_type", STORED);

        let schema = schema_builder.build();

        (schema, SchemaFields {
            offset, timestamp, key, value, headers_json, attributes,
            base_offset, partition_leader_epoch, producer_id, producer_epoch,
            base_sequence, sequence, is_transactional, is_control,
            compression, timestamp_type,
        })
    }
}

/// Metadata for a Tantivy segment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMetadata {
    pub topic: String,
    pub partition: i32,
    pub base_offset: i64,
    pub last_offset: i64,
    pub record_count: usize,
    pub min_timestamp: i64,
    pub max_timestamp: i64,
    pub created_at: i64,
    pub compression: CompressionType,
}

/// Writer for creating Tantivy segments
pub struct TantivySegmentWriter {
    index: Index,
    writer: IndexWriter,
    schema_fields: SchemaFields,
    metadata: SegmentMetadata,
}

impl TantivySegmentWriter {
    pub fn new(topic: String, partition: i32, base_offset: i64) -> Result<Self> {
        let (schema, schema_fields) = SchemaFields::build_schema();
        let index = Index::create_in_ram(schema);
        let writer = index.writer(50_000_000)
            .map_err(|e| Error::Internal(format!("Failed to create Tantivy writer: {}", e)))?;

        let metadata = SegmentMetadata {
            topic, partition, base_offset,
            last_offset: base_offset - 1,
            record_count: 0,
            min_timestamp: i64::MAX,
            max_timestamp: i64::MIN,
            created_at: chrono::Utc::now().timestamp(),
            compression: CompressionType::None,
        };

        Ok(Self { index, writer, schema_fields, metadata })
    }

    pub fn write_batch(&mut self, canonical: &CanonicalRecord) -> Result<()> {
        for record in &canonical.records {
            let mut doc = TantivyDocument::new();

            doc.add_i64(self.schema_fields.offset, record.offset);

            let dt = TantivyDateTime::from_timestamp_millis(record.timestamp);
            doc.add_date(self.schema_fields.timestamp, dt);

            if let Some(ref k) = record.key {
                doc.add_bytes(self.schema_fields.key, k);
            }
            if let Some(ref v) = record.value {
                doc.add_bytes(self.schema_fields.value, v);
            }

            let headers_json = serde_json::to_string(&record.headers)
                .map_err(|e| Error::Internal(format!("Failed to serialize headers: {}", e)))?;
            doc.add_text(self.schema_fields.headers_json, headers_json);

            doc.add_i64(self.schema_fields.attributes, record.attributes as i64);
            doc.add_i64(self.schema_fields.base_offset, canonical.base_offset);
            doc.add_i64(self.schema_fields.partition_leader_epoch, canonical.partition_leader_epoch as i64);
            doc.add_i64(self.schema_fields.producer_id, canonical.producer_id);
            doc.add_i64(self.schema_fields.producer_epoch, canonical.producer_epoch as i64);
            doc.add_i64(self.schema_fields.base_sequence, canonical.base_sequence as i64);

            let record_index = canonical.records.iter().position(|r| r.offset == record.offset).unwrap_or(0);
            let sequence = canonical.base_sequence + record_index as i32;
            doc.add_i64(self.schema_fields.sequence, sequence as i64);

            doc.add_bool(self.schema_fields.is_transactional, canonical.is_transactional);
            doc.add_bool(self.schema_fields.is_control, canonical.is_control);
            doc.add_u64(self.schema_fields.compression, canonical.compression.clone() as u64);
            doc.add_u64(self.schema_fields.timestamp_type, canonical.timestamp_type.clone() as u64);

            self.writer.add_document(doc)
                .map_err(|e| Error::Internal(format!("Failed to add document: {}", e)))?;

            self.metadata.last_offset = record.offset;
            self.metadata.record_count += 1;
            self.metadata.min_timestamp = self.metadata.min_timestamp.min(record.timestamp);
            self.metadata.max_timestamp = self.metadata.max_timestamp.min(record.timestamp);
        }

        Ok(())
    }

    pub fn commit(mut self) -> Result<(Index, SegmentMetadata)> {
        self.writer.commit()
            .map_err(|e| Error::Internal(format!("Failed to commit: {}", e)))?;
        Ok((self.index, self.metadata))
    }

    /// Commit and write the index to a directory, then serialize to tar.gz
    pub fn commit_and_serialize(mut self, output_dir: &Path) -> Result<(PathBuf, SegmentMetadata)> {
        // Create temp directory for index files
        let temp_dir = tempfile::tempdir()
            .map_err(|e| Error::Internal(format!("Failed to create temp dir: {}", e)))?;
        let index_dir = temp_dir.path().join("index");
        fs::create_dir_all(&index_dir)
            .map_err(|e| Error::Internal(format!("Failed to create index dir: {}", e)))?;

        // Create disk-based index
        let (schema, _fields) = SchemaFields::build_schema();
        let disk_index = Index::create_in_dir(&index_dir, schema.clone())
            .map_err(|e| Error::Internal(format!("Failed to create disk index: {}", e)))?;

        let mut disk_writer: IndexWriter<TantivyDocument> = disk_index.writer(50_000_000)
            .map_err(|e| Error::Internal(format!("Failed to create disk writer: {}", e)))?;

        // Read all documents from in-memory index and write to disk
        self.writer.commit()
            .map_err(|e| Error::Internal(format!("Failed to commit in-memory index: {}", e)))?;

        let reader = self.index.reader()
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;
        let searcher = reader.searcher();

        // Get all documents by searching for all offsets
        let offset_field = self.schema_fields.offset;
        let all_docs_query = tantivy::query::AllQuery;
        let top_docs = searcher.search(&all_docs_query, &tantivy::collector::TopDocs::with_limit(1000000))
            .map_err(|e| Error::Internal(format!("Failed to search all docs: {}", e)))?;

        for (_score, doc_address) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_address)
                .map_err(|e| Error::Internal(format!("Failed to retrieve doc: {}", e)))?;
            disk_writer.add_document(doc)
                .map_err(|e| Error::Internal(format!("Failed to add doc to disk index: {}", e)))?;
        }

        disk_writer.commit()
            .map_err(|e| Error::Internal(format!("Failed to commit disk index: {}", e)))?;

        // Write metadata.json
        let metadata_path = temp_dir.path().join("metadata.json");
        let metadata_json = serde_json::to_string_pretty(&self.metadata)
            .map_err(|e| Error::Internal(format!("Failed to serialize metadata: {}", e)))?;
        fs::write(&metadata_path, metadata_json)
            .map_err(|e| Error::Internal(format!("Failed to write metadata: {}", e)))?;

        // Create tar.gz archive
        fs::create_dir_all(output_dir)
            .map_err(|e| Error::Internal(format!("Failed to create output dir: {}", e)))?;

        let segment_filename = format!("segment_{}_{}_{}_{}.tar.gz",
            self.metadata.topic, self.metadata.partition, self.metadata.base_offset, self.metadata.last_offset);
        let output_path = output_dir.join(&segment_filename);

        let tar_file = File::create(&output_path)
            .map_err(|e| Error::Internal(format!("Failed to create tar file: {}", e)))?;
        let enc = GzEncoder::new(tar_file, Compression::default());
        let mut tar = tar::Builder::new(enc);

        // Add index directory
        tar.append_dir_all("index", &index_dir)
            .map_err(|e| Error::Internal(format!("Failed to add index to tar: {}", e)))?;

        // Add metadata
        let mut metadata_file = File::open(&metadata_path)
            .map_err(|e| Error::Internal(format!("Failed to open metadata: {}", e)))?;
        tar.append_file("metadata.json", &mut metadata_file)
            .map_err(|e| Error::Internal(format!("Failed to add metadata to tar: {}", e)))?;

        tar.finish()
            .map_err(|e| Error::Internal(format!("Failed to finish tar: {}", e)))?;

        Ok((output_path, self.metadata))
    }

    /// Commit, serialize, and upload to object store
    pub async fn commit_and_upload(
        self,
        object_store: Arc<dyn ObjectStore>,
        key_prefix: &str,
    ) -> Result<(String, SegmentMetadata)> {
        // Create temp directory for serialization
        let temp_dir = tempfile::tempdir()
            .map_err(|e| Error::Internal(format!("Failed to create temp dir: {}", e)))?;

        // Serialize to tar.gz
        let (tar_gz_path, metadata) = self.commit_and_serialize(temp_dir.path())?;

        // Read file into memory
        let file_data = fs::read(&tar_gz_path)
            .map_err(|e| Error::Internal(format!("Failed to read tar.gz: {}", e)))?;

        // Generate object store key
        let object_key = format!("{}/segment_{}_{}_{}_{}.tar.gz",
            key_prefix,
            metadata.topic,
            metadata.partition,
            metadata.base_offset,
            metadata.last_offset
        );

        // Upload to object store
        object_store.put(&object_key, Bytes::from(file_data))
            .await
            .map_err(|e| Error::Internal(format!("Failed to upload to object store: {}", e)))?;

        Ok((object_key, metadata))
    }
}

/// Reader for querying Tantivy segments
pub struct TantivySegmentReader {
    index: Index,
    reader: IndexReader,
    schema_fields: SchemaFields,
    metadata: SegmentMetadata,
}

impl TantivySegmentReader {
    pub fn open(index_path: &Path, metadata: SegmentMetadata) -> Result<Self> {
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let index = Index::open_in_dir(index_path)
            .map_err(|e| Error::Internal(format!("Failed to open index: {}", e)))?;
        let reader = index.reader()
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;

        Ok(Self { index, reader, schema_fields, metadata })
    }

    /// Deserialize a segment from a tar.gz file
    pub fn from_tar_gz(tar_gz_path: &Path) -> Result<Self> {
        // Create temp directory for extraction
        let temp_dir = tempfile::tempdir()
            .map_err(|e| Error::Internal(format!("Failed to create temp dir: {}", e)))?;

        // Open and decompress tar.gz
        let tar_file = File::open(tar_gz_path)
            .map_err(|e| Error::Internal(format!("Failed to open tar.gz: {}", e)))?;
        let dec = GzDecoder::new(tar_file);
        let mut archive = tar::Archive::new(dec);

        // Extract all files
        archive.unpack(temp_dir.path())
            .map_err(|e| Error::Internal(format!("Failed to unpack tar: {}", e)))?;

        // Read metadata
        let metadata_path = temp_dir.path().join("metadata.json");
        let metadata_content = fs::read_to_string(&metadata_path)
            .map_err(|e| Error::Internal(format!("Failed to read metadata: {}", e)))?;
        let metadata: SegmentMetadata = serde_json::from_str(&metadata_content)
            .map_err(|e| Error::Internal(format!("Failed to parse metadata: {}", e)))?;

        // Open index
        let index_path = temp_dir.path().join("index");
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let index = Index::open_in_dir(&index_path)
            .map_err(|e| Error::Internal(format!("Failed to open index: {}", e)))?;
        let reader = index.reader()
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;

        Ok(Self { index, reader, schema_fields, metadata })
    }

    /// Deserialize a segment from tar.gz bytes (in-memory)
    /// This is more efficient than from_tar_gz() when data is already in memory
    pub fn from_tar_gz_bytes(tar_gz_bytes: &[u8]) -> Result<Self> {
        // Create temp directory for extraction
        let temp_dir = tempfile::tempdir()
            .map_err(|e| Error::Internal(format!("Failed to create temp dir: {}", e)))?;

        // Decompress tar.gz from memory
        let dec = GzDecoder::new(tar_gz_bytes);
        let mut archive = tar::Archive::new(dec);

        // Extract all files
        archive.unpack(temp_dir.path())
            .map_err(|e| Error::Internal(format!("Failed to unpack tar: {}", e)))?;

        // Read metadata
        let metadata_path = temp_dir.path().join("metadata.json");
        let metadata_content = fs::read_to_string(&metadata_path)
            .map_err(|e| Error::Internal(format!("Failed to read metadata: {}", e)))?;
        let metadata: SegmentMetadata = serde_json::from_str(&metadata_content)
            .map_err(|e| Error::Internal(format!("Failed to parse metadata: {}", e)))?;

        // Open index
        let index_path = temp_dir.path().join("index");
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let index = Index::open_in_dir(&index_path)
            .map_err(|e| Error::Internal(format!("Failed to open index: {}", e)))?;
        let reader = index.reader()
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;

        Ok(Self { index, reader, schema_fields, metadata })
    }

    /// Download segment from object store and open
    pub async fn from_object_store(
        object_store: Arc<dyn ObjectStore>,
        object_key: &str,
    ) -> Result<Self> {
        // Download from object store
        let data = object_store.get(object_key)
            .await
            .map_err(|e| Error::Internal(format!("Failed to download from object store: {}", e)))?;

        // Write to temp file
        let temp_dir = tempfile::tempdir()
            .map_err(|e| Error::Internal(format!("Failed to create temp dir: {}", e)))?;
        let tar_gz_path = temp_dir.path().join("segment.tar.gz");

        fs::write(&tar_gz_path, data.as_ref())
            .map_err(|e| Error::Internal(format!("Failed to write temp file: {}", e)))?;

        // Deserialize from tar.gz
        Self::from_tar_gz(&tar_gz_path)
    }

    pub fn query_by_offset_range(&self, start: i64, end: i64) -> Result<Vec<CanonicalRecordEntry>> {
        let searcher = self.reader.searcher();
        let start_term = Term::from_field_i64(self.schema_fields.offset, start);
        let end_term = Term::from_field_i64(self.schema_fields.offset, end);
        let query = RangeQuery::new(
            std::ops::Bound::Included(start_term),
            std::ops::Bound::Excluded(end_term),
        );

        let top_docs = searcher.search(&query, &TopDocs::with_limit(10000))
            .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;

        let mut records = Vec::new();
        for (_score, doc_address) in top_docs {
            let doc = searcher.doc(doc_address)
                .map_err(|e| Error::Internal(format!("Failed to retrieve doc: {}", e)))?;
            records.push(self.doc_to_record(&doc)?);
        }

        records.sort_by_key(|r| r.offset);
        Ok(records)
    }

    fn doc_to_record(&self, doc: &TantivyDocument) -> Result<CanonicalRecordEntry> {
        let offset = doc.get_first(self.schema_fields.offset)
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::Internal("Missing offset".into()))?;

        let timestamp_dt = doc.get_first(self.schema_fields.timestamp)
            .and_then(|v| v.as_datetime())
            .ok_or_else(|| Error::Internal("Missing timestamp".into()))?;
        let timestamp = timestamp_dt.into_timestamp_millis();

        let key = doc.get_first(self.schema_fields.key)
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec());

        let value = doc.get_first(self.schema_fields.value)
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec());

        let headers_json = doc.get_first(self.schema_fields.headers_json)
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Internal("Missing headers".into()))?;
        let headers: Vec<RecordHeader> = serde_json::from_str(headers_json)
            .map_err(|e| Error::Internal(format!("Failed to deserialize headers: {}", e)))?;

        let attributes = doc.get_first(self.schema_fields.attributes)
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::Internal("Missing attributes".into()))? as i8;

        Ok(CanonicalRecordEntry { offset, timestamp, key, value, headers, attributes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_canonical_record(base_offset: i64, num_records: usize) -> CanonicalRecord {
        let mut records = Vec::new();
        for i in 0..num_records {
            records.push(CanonicalRecordEntry {
                offset: base_offset + i as i64,
                timestamp: 1700000000000 + (i as i64 * 1000),
                key: Some(format!("key-{}", i).into_bytes()),
                value: Some(format!("value-{}", i).into_bytes()),
                headers: vec![
                    RecordHeader {
                        key: "header1".to_string(),
                        value: Some(format!("hval-{}", i).into_bytes()),
                    }
                ],
                attributes: 0,
            });
        }

        CanonicalRecord {
            base_offset,
            partition_leader_epoch: 0,
            producer_id: 1234,
            producer_epoch: 5,
            base_sequence: 100,
            is_transactional: false,
            is_control: false,
            compression: CompressionType::None,
            timestamp_type: TimestampType::CreateTime,
            base_timestamp: 1700000000000,
            max_timestamp: 1700000000000 + ((num_records - 1) as i64 * 1000),
            records,
        }
    }

    #[test]
    fn test_write_single_batch() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(0, 10);

        writer.write_batch(&canonical).unwrap();

        assert_eq!(writer.metadata.record_count, 10);
        assert_eq!(writer.metadata.last_offset, 9);
        assert_eq!(writer.metadata.base_offset, 0);
    }

    #[test]
    fn test_write_multiple_batches() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();

        let batch1 = create_test_canonical_record(0, 100);
        let batch2 = create_test_canonical_record(100, 100);
        let batch3 = create_test_canonical_record(200, 100);

        writer.write_batch(&batch1).unwrap();
        writer.write_batch(&batch2).unwrap();
        writer.write_batch(&batch3).unwrap();

        assert_eq!(writer.metadata.record_count, 300);
        assert_eq!(writer.metadata.last_offset, 299);
        assert_eq!(writer.metadata.base_offset, 0);
    }

    #[test]
    fn test_write_and_commit() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(0, 50);

        writer.write_batch(&canonical).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        assert_eq!(metadata.record_count, 50);
        assert_eq!(metadata.last_offset, 49);
        assert_eq!(metadata.topic, "test-topic");
        assert_eq!(metadata.partition, 0);

        // Verify index is valid
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert!(searcher.num_docs() > 0);
    }

    #[test]
    fn test_metadata_tracking() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 5, 1000).unwrap();

        assert_eq!(writer.metadata.topic, "test-topic");
        assert_eq!(writer.metadata.partition, 5);
        assert_eq!(writer.metadata.base_offset, 1000);
        assert_eq!(writer.metadata.last_offset, 999); // base_offset - 1
        assert_eq!(writer.metadata.record_count, 0);
        assert_eq!(writer.metadata.min_timestamp, i64::MAX);
        assert_eq!(writer.metadata.max_timestamp, i64::MIN);

        let canonical = create_test_canonical_record(1000, 10);
        writer.write_batch(&canonical).unwrap();

        assert_eq!(writer.metadata.last_offset, 1009);
        assert_eq!(writer.metadata.record_count, 10);
        assert_eq!(writer.metadata.min_timestamp, 1700000000000);
        assert!(writer.metadata.max_timestamp <= 1700000009000);
    }

    #[test]
    fn test_null_key_value_handling() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();

        let mut canonical = create_test_canonical_record(0, 1);
        canonical.records[0].key = None;
        canonical.records[0].value = None;

        writer.write_batch(&canonical).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        assert_eq!(metadata.record_count, 1);

        // Verify document was stored
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 1);
    }

    #[test]
    fn test_empty_headers() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();

        let mut canonical = create_test_canonical_record(0, 1);
        canonical.records[0].headers = vec![];

        writer.write_batch(&canonical).unwrap();
        let (index, _metadata) = writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 1);
    }

    #[test]
    fn test_large_batch_1000_records() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(0, 1000);

        writer.write_batch(&canonical).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        assert_eq!(metadata.record_count, 1000);
        assert_eq!(metadata.last_offset, 999);

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 1000);
    }

    #[test]
    fn test_transactional_batch() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();

        let mut canonical = create_test_canonical_record(0, 10);
        canonical.is_transactional = true;
        canonical.is_control = false;

        writer.write_batch(&canonical).unwrap();
        let (_index, metadata) = writer.commit().unwrap();

        assert_eq!(metadata.record_count, 10);
    }

    #[test]
    fn test_compression_types() {
        for compression in &[
            CompressionType::None,
            CompressionType::Gzip,
            CompressionType::Snappy,
            CompressionType::Lz4,
            CompressionType::Zstd,
        ] {
            let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();

            let mut canonical = create_test_canonical_record(0, 5);
            canonical.compression = compression.clone();

            writer.write_batch(&canonical).unwrap();
            let (_index, metadata) = writer.commit().unwrap();

            assert_eq!(metadata.record_count, 5);
        }
    }

    #[test]
    fn test_round_trip_write_and_query() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(0, 100);

        writer.write_batch(&canonical).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        // Create reader from in-memory index
        let reader = index.reader().unwrap();
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let test_reader = TantivySegmentReader {
            index,
            reader,
            schema_fields,
            metadata,
        };

        // Query for offset range 10-20
        let results = test_reader.query_by_offset_range(10, 20).unwrap();
        assert_eq!(results.len(), 10);

        // Verify offsets are correct and sorted
        for (i, record) in results.iter().enumerate() {
            assert_eq!(record.offset, 10 + i as i64);
            assert_eq!(record.key, Some(format!("key-{}", 10 + i).into_bytes()));
            assert_eq!(record.value, Some(format!("value-{}", 10 + i).into_bytes()));
        }
    }

    #[test]
    fn test_query_by_offset_range_boundaries() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(100, 50);

        writer.write_batch(&canonical).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let test_reader = TantivySegmentReader {
            index,
            reader,
            schema_fields,
            metadata,
        };

        // Test exact boundaries
        let results = test_reader.query_by_offset_range(100, 150).unwrap();
        assert_eq!(results.len(), 50);
        assert_eq!(results.first().unwrap().offset, 100);
        assert_eq!(results.last().unwrap().offset, 149);

        // Test partial range
        let results = test_reader.query_by_offset_range(110, 120).unwrap();
        assert_eq!(results.len(), 10);
        assert_eq!(results.first().unwrap().offset, 110);
        assert_eq!(results.last().unwrap().offset, 119);

        // Test out of range
        let results = test_reader.query_by_offset_range(200, 300).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_query_preserves_headers() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(0, 5);

        writer.write_batch(&canonical).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let test_reader = TantivySegmentReader {
            index,
            reader,
            schema_fields,
            metadata,
        };

        let results = test_reader.query_by_offset_range(0, 5).unwrap();
        assert_eq!(results.len(), 5);

        for (i, record) in results.iter().enumerate() {
            assert_eq!(record.headers.len(), 1);
            assert_eq!(record.headers[0].key, "header1");
            assert_eq!(record.headers[0].value, Some(format!("hval-{}", i).into_bytes()));
        }
    }

    #[test]
    fn test_query_null_key_value() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();

        let mut canonical = create_test_canonical_record(0, 5);
        canonical.records[2].key = None;
        canonical.records[2].value = None;

        writer.write_batch(&canonical).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let test_reader = TantivySegmentReader {
            index,
            reader,
            schema_fields,
            metadata,
        };

        let results = test_reader.query_by_offset_range(0, 5).unwrap();
        assert_eq!(results.len(), 5);

        // Check record with null key/value
        assert!(results[2].key.is_none());
        assert!(results[2].value.is_none());

        // Check other records are intact
        assert!(results[0].key.is_some());
        assert!(results[0].value.is_some());
    }

    #[test]
    fn test_large_query_1000_records() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(0, 1000);

        writer.write_batch(&canonical).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let test_reader = TantivySegmentReader {
            index,
            reader,
            schema_fields,
            metadata,
        };

        // Query all records
        let results = test_reader.query_by_offset_range(0, 1000).unwrap();
        assert_eq!(results.len(), 1000);

        // Verify sorting
        for i in 0..1000 {
            assert_eq!(results[i].offset, i as i64);
        }

        // Query middle range
        let results = test_reader.query_by_offset_range(400, 600).unwrap();
        assert_eq!(results.len(), 200);
        assert_eq!(results.first().unwrap().offset, 400);
        assert_eq!(results.last().unwrap().offset, 599);
    }

    #[test]
    fn test_multiple_batches_query() {
        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();

        let batch1 = create_test_canonical_record(0, 100);
        let batch2 = create_test_canonical_record(100, 100);
        let batch3 = create_test_canonical_record(200, 100);

        writer.write_batch(&batch1).unwrap();
        writer.write_batch(&batch2).unwrap();
        writer.write_batch(&batch3).unwrap();
        let (index, metadata) = writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let (_schema, schema_fields) = SchemaFields::build_schema();
        let test_reader = TantivySegmentReader {
            index,
            reader,
            schema_fields,
            metadata,
        };

        // Query across batch boundaries
        let results = test_reader.query_by_offset_range(90, 110).unwrap();
        assert_eq!(results.len(), 20);
        assert_eq!(results.first().unwrap().offset, 90);
        assert_eq!(results.last().unwrap().offset, 109);

        // Query all batches
        let results = test_reader.query_by_offset_range(0, 300).unwrap();
        assert_eq!(results.len(), 300);
    }

    #[test]
    fn test_serialize_and_deserialize() {
        let temp_dir = tempfile::tempdir().unwrap();
        let output_dir = temp_dir.path().join("segments");

        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(0, 50);

        writer.write_batch(&canonical).unwrap();
        let (tar_gz_path, metadata) = writer.commit_and_serialize(&output_dir).unwrap();

        // Verify tar.gz file exists
        assert!(tar_gz_path.exists());
        assert!(tar_gz_path.to_str().unwrap().ends_with(".tar.gz"));

        // Deserialize
        let reader = TantivySegmentReader::from_tar_gz(&tar_gz_path).unwrap();

        // Verify metadata
        assert_eq!(reader.metadata.topic, "test-topic");
        assert_eq!(reader.metadata.partition, 0);
        assert_eq!(reader.metadata.record_count, 50);
        assert_eq!(reader.metadata.base_offset, 0);
        assert_eq!(reader.metadata.last_offset, 49);

        // Verify can query
        let results = reader.query_by_offset_range(10, 20).unwrap();
        assert_eq!(results.len(), 10);
        assert_eq!(results.first().unwrap().offset, 10);
    }

    #[test]
    fn test_serialize_large_segment() {
        let temp_dir = tempfile::tempdir().unwrap();
        let output_dir = temp_dir.path().join("segments");

        let mut writer = TantivySegmentWriter::new("large-topic".to_string(), 5, 1000).unwrap();
        let canonical = create_test_canonical_record(1000, 500);

        writer.write_batch(&canonical).unwrap();
        let (tar_gz_path, metadata) = writer.commit_and_serialize(&output_dir).unwrap();

        assert!(tar_gz_path.exists());
        assert_eq!(metadata.record_count, 500);

        // Verify file is compressed (should be smaller than raw data)
        let file_size = std::fs::metadata(&tar_gz_path).unwrap().len();
        assert!(file_size > 0);

        // Deserialize and query
        let reader = TantivySegmentReader::from_tar_gz(&tar_gz_path).unwrap();
        let results = reader.query_by_offset_range(1000, 1500).unwrap();
        assert_eq!(results.len(), 500);
    }

    #[test]
    fn test_round_trip_preserves_all_data() {
        let temp_dir = tempfile::tempdir().unwrap();
        let output_dir = temp_dir.path().join("segments");

        let mut writer = TantivySegmentWriter::new("test-topic".to_string(), 0, 0).unwrap();
        let canonical = create_test_canonical_record(0, 20);

        writer.write_batch(&canonical).unwrap();
        let (tar_gz_path, _metadata) = writer.commit_and_serialize(&output_dir).unwrap();

        // Deserialize and verify all records
        let reader = TantivySegmentReader::from_tar_gz(&tar_gz_path).unwrap();
        let results = reader.query_by_offset_range(0, 20).unwrap();

        assert_eq!(results.len(), 20);
        for (i, record) in results.iter().enumerate() {
            assert_eq!(record.offset, i as i64);
            assert_eq!(record.key, Some(format!("key-{}", i).into_bytes()));
            assert_eq!(record.value, Some(format!("value-{}", i).into_bytes()));
            assert_eq!(record.headers.len(), 1);
            assert_eq!(record.headers[0].key, "header1");
        }
    }
}
