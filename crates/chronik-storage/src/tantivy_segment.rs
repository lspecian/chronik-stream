//! Tantivy-based segment storage for CanonicalRecord (Tantivy 0.24 API).

use crate::canonical_record::{CanonicalRecord, CanonicalRecordEntry, RecordHeader, CompressionType, TimestampType};
use crate::object_store::{ObjectStore, PutOptions};
use crate::text_analysis;
use chronik_common::{Result, Error};
use tantivy::{
    doc, schema::*, Index, IndexWriter, IndexReader,
    collector::TopDocs, query::RangeQuery, Term,
    DateTime as TantivyDateTime, TantivyDocument,
};
use serde::{Serialize, Deserialize};

/// Document-store blocks kept decompressed while copying an index.
///
/// The copy reads documents in store order, so a handful of blocks is enough to
/// stop consecutive documents re-decompressing the block they share. Zero would
/// make the copy decompress once per document.
const STORE_BLOCK_CACHE: usize = 16;
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
    /// Tokenized, searchable copy of the record value (UTF-8, lossy). The raw
    /// `_value` bytes field is STORED-only for exact offset retrieval; this
    /// `value` TEXT field is what full-text `/_search` matches against, mirroring
    /// the (now-disabled) realtime indexer's `value` field so cold segments are
    /// content-searchable, not just offset-retrievable.
    pub value_text: Field,
    /// Partition, INDEXED so a hit can be uniquely identified by
    /// `(partition, offset)` — offset alone collides across partitions.
    pub partition: Field,
    /// Field-level index of a JSON-object value (see [`crate::json_index`]).
    /// LAST in the schema on purpose: readers rebuild these field ids from
    /// code and apply them to archives written before it existed, so every
    /// earlier id must stay where it was. Not stored; rebuilt from `_value`
    /// whenever documents are copied out of the stored form.
    pub json: Field,
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
        // Content-searchable value + indexed partition (see field docs).
        let value_text = schema_builder.add_text_field("value", TEXT | STORED);
        let partition = schema_builder.add_i64_field("partition", INDEXED | STORED | FAST);
        let json = schema_builder.add_json_field(
            crate::json_index::JSON_FIELD,
            crate::json_index::json_field_options(),
        );

        let schema = schema_builder.build();

        (schema, SchemaFields {
            offset, timestamp, key, value, headers_json, attributes,
            base_offset, partition_leader_epoch, producer_id, producer_epoch,
            base_sequence, sequence, is_transactional, is_control,
            compression, timestamp_type, value_text, partition, json,
        })
    }
}

/// Index a JSON-object record value into the segment's JSON field; anything
/// else (non-JSON, arrays, scalars) is left to the `value` text field.
fn add_json_document(doc: &mut TantivyDocument, json_field: Field, value: &[u8]) {
    if let Some(object) = crate::json_index::json_object(value) {
        doc.add_object(
            json_field,
            object.into_iter().map(|(k, v)| (k, v.into())).collect(),
        );
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

/// Diagnostic: number of TantivySegmentWriter instances currently alive.
/// Each holds a live Tantivy IndexWriter (50MB budget + ~9 threads), so a
/// sustained high count means segment-indexing jobs are piling up while each
/// holds a writer — the memory profile that OOM-killed nodes at scale.
pub static LIVE_SEGMENT_WRITERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// RAII guard for [`LIVE_SEGMENT_WRITERS`]. A member (not a Drop impl on the
/// struct itself) so `commit(self)` can still move fields out.
struct WriterCountGuard;
impl Drop for WriterCountGuard {
    fn drop(&mut self) {
        LIVE_SEGMENT_WRITERS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Writer for creating Tantivy segments
pub struct TantivySegmentWriter {
    index: Index,
    writer: IndexWriter,
    schema_fields: SchemaFields,
    metadata: SegmentMetadata,
    _count_guard: WriterCountGuard,
}

impl TantivySegmentWriter {
    pub fn new(topic: String, partition: i32, base_offset: i64) -> Result<Self> {
        let (schema, schema_fields) = SchemaFields::build_schema();
        let index = Index::create_in_ram(schema);
        text_analysis::register_analyzer(&index);
        let writer = index.writer(50_000_000)
            .map_err(|e| Error::Internal(format!("Failed to create Tantivy writer: {}", e)))?;
        let live = LIVE_SEGMENT_WRITERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if live > 8 {
            tracing::warn!(live, "TantivySegmentWriter instances piling up (each = 50MB writer budget + ~9 threads)");
        }

        let metadata = SegmentMetadata {
            topic, partition, base_offset,
            last_offset: base_offset - 1,
            record_count: 0,
            min_timestamp: i64::MAX,
            max_timestamp: i64::MIN,
            created_at: chrono::Utc::now().timestamp(),
            compression: CompressionType::None,
        };

        Ok(Self { index, writer, schema_fields, metadata, _count_guard: WriterCountGuard })
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
                // Tokenized, searchable copy for full-text `/_search`.
                doc.add_text(self.schema_fields.value_text, &String::from_utf8_lossy(v));
                add_json_document(&mut doc, self.schema_fields.json, v);
            }

            doc.add_i64(self.schema_fields.partition, self.metadata.partition as i64);

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
        text_analysis::register_analyzer(&disk_index);

        let mut disk_writer: IndexWriter<TantivyDocument> = disk_index.writer(50_000_000)
            .map_err(|e| Error::Internal(format!("Failed to create disk writer: {}", e)))?;

        // Read all documents from in-memory index and write to disk
        self.writer.commit()
            .map_err(|e| Error::Internal(format!("Failed to commit in-memory index: {}", e)))?;

        let reader = self.index.reader()
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;
        let searcher = reader.searcher();

        // Copy every document to the disk index by walking the segments
        // directly.
        //
        // This used to run `AllQuery` through `TopDocs::with_limit(1_000_000)`,
        // which silently wrote only the first million documents: a WAL segment
        // holding more than that produced a cold index missing the rest, with no
        // error and a record_count in the metadata that disagreed with what the
        // index could answer. A scoring collector is also the wrong instrument
        // for "every document" — it heap-sorts by a score none of these
        // documents have. Walking `segment_readers()` is exact, unbounded, and
        // cheaper.
        let offset_field = self.schema_fields.offset;
        let mut copied: usize = 0;
        for segment_reader in searcher.segment_readers() {
            let store = segment_reader
                .get_store_reader(STORE_BLOCK_CACHE)
                .map_err(|e| Error::Internal(format!("Failed to open document store: {}", e)))?;
            // Passing the alive bitset skips deleted documents. Nothing deletes
            // from these indexes today, but a copy that ignored it would
            // resurrect anything that ever did.
            for doc in store.iter::<TantivyDocument>(segment_reader.alive_bitset()) {
                let mut doc = doc
                    .map_err(|e| Error::Internal(format!("Failed to retrieve doc: {}", e)))?;
                // The document store holds only STORED fields; the JSON field
                // is not stored, so the copy would ship an archive in which
                // field queries match nothing. Rebuild it from the raw value.
                let raw_value = doc
                    .get_first(self.schema_fields.value)
                    .and_then(|v| v.as_bytes())
                    .map(|b| b.to_vec());
                if let Some(raw_value) = raw_value {
                    add_json_document(&mut doc, self.schema_fields.json, &raw_value);
                }
                disk_writer.add_document(doc)
                    .map_err(|e| Error::Internal(format!("Failed to add doc to disk index: {}", e)))?;
                copied += 1;
            }
        }

        // The metadata we are about to ship says how many records this segment
        // holds. If the index does not hold that many, the segment would serve
        // confident partial answers forever.
        if copied != self.metadata.record_count {
            return Err(Error::Internal(format!(
                "Tantivy index for {}:{} holds {} documents but the segment metadata says {} — \
                 refusing to publish an index that cannot answer for its own offset range",
                self.metadata.topic, self.metadata.partition, copied, self.metadata.record_count
            )));
        }

        disk_writer.commit()
            .map_err(|e| Error::Internal(format!("Failed to commit disk index: {}", e)))?;

        // Join Tantivy's background merge threads before anything reads the
        // directory.
        //
        // Issue #57: `commit()` returning does NOT mean the index directory has
        // stopped changing. Tantivy merges on background threads, and when a
        // merge finishes it deletes the segment files it merged away (the
        // garbage collector then removes whatever else became unreferenced).
        // The tar below lists the directory and *then* opens each entry, so a
        // file deleted in between surfaced as a bare
        //   Failed to add index to tar: No such file or directory (os error 2)
        // failing the whole index upload — every run, for whichever topic's
        // segments were large enough to trigger a merge, while a smaller topic
        // on the identical binary indexed fine. That looked topic-specific and
        // was really size-specific.
        //
        // Consuming the writer joins those threads and drops the write lock, so
        // from here the directory is stable. This is the fix; the retry inside
        // `write_archive` is belt-and-braces, not the cure.
        disk_writer.wait_merging_threads()
            .map_err(|e| Error::Internal(format!("Failed to join Tantivy merge threads: {}", e)))?;

        // Write metadata.json
        let metadata_path = temp_dir.path().join("metadata.json");
        let metadata_json = serde_json::to_string_pretty(&self.metadata)
            .map_err(|e| Error::Internal(format!("Failed to serialize metadata: {}", e)))?;
        fs::write(&metadata_path, metadata_json)
            .map_err(|e| Error::Internal(format!("Failed to write metadata: {}", e)))?;

        // Create tar.gz archive
        fs::create_dir_all(output_dir)
            .map_err(|e| Error::Internal(format!("Failed to create output dir: {}", e)))?;

        let segment_filename = format!("segment_{}_{}_{}_{}.tar",
            self.metadata.topic, self.metadata.partition, self.metadata.base_offset, self.metadata.last_offset);
        let output_path = output_dir.join(&segment_filename);

        Self::write_archive(&output_path, &index_dir, &metadata_path)?;

        // The archive's term filter, beside it (`<archive>.terms`), so a search
        // can skip this archive without opening it. Optional: an archive
        // without one is simply always opened.
        match crate::term_filter::TermFilter::build(&disk_index) {
            Ok(filter) => {
                let path = PathBuf::from(crate::term_filter::term_filter_key(&output_path.to_string_lossy()));
                if let Err(e) = fs::write(&path, filter.to_bytes()) {
                    tracing::warn!("could not write the term filter {}: {}", path.display(), e);
                }
            }
            Err(e) => tracing::warn!("could not build the term filter for {}: {}", output_path.display(), e),
        }

        Ok((output_path, self.metadata))
    }

    /// How many times the archive is rebuilt when a file vanishes under it.
    const TAR_ATTEMPTS: u32 = 3;

    /// Pack the committed index directory and its metadata into `output_path`.
    ///
    /// Retried when, and only when, the archive fails because a file was not
    /// found. Tar lists a directory and then opens each entry, so anything that
    /// removes a file in between fails the whole archive (see the merge-thread
    /// note in `commit_and_serialize`). Each attempt re-lists the directory, so
    /// a retry over a settled directory succeeds — the alternative is a segment
    /// that is never indexed, retried forever, which is what issue #57 reported.
    ///
    /// Any other error is returned immediately: retrying a full disk or a
    /// permission problem just burns I/O on the indexer's thread.
    pub(crate) fn write_archive(output_path: &Path, index_dir: &Path, metadata_path: &Path) -> Result<()> {
        let mut last: Option<std::io::Error> = None;

        for attempt in 1..=Self::TAR_ATTEMPTS {
            match Self::try_write_archive(output_path, index_dir, metadata_path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        attempt,
                        max_attempts = Self::TAR_ATTEMPTS,
                        index_dir = %index_dir.display(),
                        error = %e,
                        "A file disappeared while archiving the Tantivy index — rebuilding the archive"
                    );
                    last = Some(e);
                }
                Err(e) => {
                    return Err(Error::Internal(format!(
                        "Failed to archive Tantivy index at {}: {}",
                        index_dir.display(),
                        e
                    )));
                }
            }
        }

        // Name the directory and its contents. The bare `No such file or
        // directory (os error 2)` this replaces gave the reader nothing to go
        // on: not which file, not which directory, not whether the index had
        // been written at all.
        Err(Error::Internal(format!(
            "Failed to archive Tantivy index at {} after {} attempts: {}. A file listed in \
             the index directory was gone by the time it was read. The directory now holds: \
             [{}]",
            index_dir.display(),
            Self::TAR_ATTEMPTS,
            last.map(|e| e.to_string())
                .unwrap_or_else(|| "no error recorded".to_string()),
            Self::describe_dir(index_dir),
        )))
    }

    /// One attempt at the archive. Returns the raw `io::Error` so the caller can
    /// tell a vanished file from a full disk.
    fn try_write_archive(
        output_path: &Path,
        index_dir: &Path,
        metadata_path: &Path,
    ) -> std::io::Result<()> {
        // A plain tar, not gzip: readers open it in place (`TarDirectory`)
        // instead of unpacking it. Tantivy's files are compressed already.
        let tar_file = std::io::BufWriter::new(File::create(output_path)?);
        let mut tar = tar::Builder::new(tar_file);

        tar.append_dir_all("index", index_dir)?;

        let mut metadata_file = File::open(metadata_path)?;
        tar.append_file("metadata.json", &mut metadata_file)?;

        // Finish the tar trailer and flush, surfacing any error: letting the
        // writer drop flushes on a best-effort basis and swallows it, so a
        // truncated archive would only surface later, as a corrupt segment.
        let file = tar.into_inner()?.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;

        Ok(())
    }

    /// Directory contents for an error message, truncated so a wide index does
    /// not produce an unreadable log line.
    fn describe_dir(dir: &Path) -> String {
        match fs::read_dir(dir) {
            Err(e) => format!("unreadable: {}", e),
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                let total = names.len();
                names.truncate(20);
                if total > names.len() {
                    format!("{}, … {} more", names.join(", "), total - names.len())
                } else if total == 0 {
                    "empty".to_string()
                } else {
                    names.join(", ")
                }
            }
        }
    }

    /// Commit, serialize, and upload to object store
    pub async fn commit_and_upload(
        self,
        object_store: Arc<dyn ObjectStore>,
        key_prefix: &str,
    ) -> Result<(String, SegmentMetadata)> {
        // v2.2.7: Move blocking I/O to dedicated threadpool to prevent blocking tokio workers
        // This fixes the 391ms disk I/O stall that was blocking all WAL writes
        let (tar_gz_path, metadata) = tokio::task::spawn_blocking(move || {
            // Set lower I/O priority for Tantivy (best-effort priority 4)
            // This prevents Tantivy commits from starving WAL fsyncs (which use priority 0)
            #[cfg(target_os = "linux")]
            {
                use libc::{syscall, SYS_ioprio_set};
                const IOPRIO_WHO_PROCESS: i32 = 1;  // Target current thread
                const IOPRIO_CLASS_SHIFT: i32 = 13;
                const IOPRIO_CLASS_BE: i32 = 2; // Best-effort
                const IOPRIO_LEVEL_LOW: i32 = 4; // Lower than WAL (which uses 0)
                let ioprio = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | IOPRIO_LEVEL_LOW;
                unsafe { syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ioprio) };
            }

            // Create temp directory for serialization
            let temp_dir = tempfile::tempdir()
                .map_err(|e| Error::Internal(format!("Failed to create temp dir: {}", e)))?;

            // Serialize to tar.gz (BLOCKING: 391ms fsync, but now won't block WAL)
            self.commit_and_serialize(temp_dir.path())
        })
        .await
        .map_err(|e| Error::Internal(format!("Task join error: {}", e)))??;

        // Read file into memory (also blocking I/O - move to threadpool)
        let tar_gz_path_clone = tar_gz_path.clone();
        let file_data = tokio::task::spawn_blocking(move || {
            fs::read(&tar_gz_path_clone)
                .map_err(|e| Error::Internal(format!("Failed to read tar.gz: {}", e)))
        })
        .await
        .map_err(|e| Error::Internal(format!("Task join error: {}", e)))??;

        // Generate object store key
        let object_key = format!("{}/segment_{}_{}_{}_{}.tar",
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

/// A reader for an archived segment, which never changes once written.
///
/// Tantivy's default reload policy watches the index directory for new
/// commits with a polling thread per index. An archive never gets one, and the
/// cold search path keeps hundreds of archives open at a time, so that would be
/// hundreds of threads polling directories for nothing.
fn archived_reader(index: &Index) -> tantivy::Result<IndexReader> {
    index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::Manual)
        .try_into()
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
        text_analysis::register_analyzer(&index);
        let reader = archived_reader(&index)
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;

        Ok(Self { index, reader, schema_fields, metadata })
    }

    /// Borrow the underlying Tantivy index — used by the search layer to run
    /// full-text queries against a cold segment via the shared query builder.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// A searcher over the segment, from the reader opened with it.
    pub fn searcher(&self) -> tantivy::Searcher {
        self.reader.searcher()
    }

    /// Segment metadata (topic, partition, offset range, record count).
    pub fn metadata(&self) -> &SegmentMetadata {
        &self.metadata
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
        text_analysis::register_analyzer(&index);
        let reader = archived_reader(&index)
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
        text_analysis::register_analyzer(&index);
        let reader = archived_reader(&index)
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;

        Ok(Self { index, reader, schema_fields, metadata })
    }

    /// Open an archive written as a plain tar, in place: the index files are
    /// slices of `bytes` (a memory-mapped file or a downloaded object), with
    /// nothing unpacked.
    pub fn open_tar(bytes: tantivy::directory::OwnedBytes) -> Result<Self> {
        use crate::tar_directory::TarDirectory;
        let metadata_bytes = TarDirectory::read_entry(&bytes, "metadata.json")
            .map_err(|e| Error::Internal(format!("Failed to read archive: {}", e)))?
            .ok_or_else(|| Error::Internal("archive has no metadata.json".to_string()))?;
        let metadata: SegmentMetadata = serde_json::from_slice(metadata_bytes.as_slice())
            .map_err(|e| Error::Internal(format!("Failed to parse metadata: {}", e)))?;
        let directory = TarDirectory::open(bytes, "index")
            .map_err(|e| Error::Internal(format!("Failed to read archive: {}", e)))?;
        let index = Index::open(directory)
            .map_err(|e| Error::Internal(format!("Failed to open index: {}", e)))?;
        text_analysis::register_analyzer(&index);
        let reader = archived_reader(&index)
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;
        let (_schema, schema_fields) = SchemaFields::build_schema();
        Ok(Self { index, reader, schema_fields, metadata })
    }

    /// Open an archive from its downloaded bytes, whichever format it is in: a
    /// plain tar opens in place; a `.tar.gz` written by an earlier version is
    /// unpacked as before.
    pub fn from_archive_bytes(bytes: bytes::Bytes) -> Result<Self> {
        if crate::tar_directory::is_gzip(&bytes) {
            Self::from_tar_gz_bytes(&bytes)
        } else {
            Self::open_tar(crate::tar_directory::owned(bytes))
        }
    }

    /// Open a plain-tar archive on local disk in place (memory-mapped).
    /// `Ok(None)` for a `.tar.gz`, which has to be unpacked to be read.
    pub fn open_tar_file(path: &Path) -> Result<Option<Self>> {
        let bytes = crate::tar_directory::map_file(path)
            .map_err(|e| Error::Internal(format!("Failed to map {}: {}", path.display(), e)))?;
        if crate::tar_directory::is_gzip(bytes.as_slice()) {
            return Ok(None);
        }
        Self::open_tar(bytes).map(Some)
    }

    /// Open a local archive in either format: a plain tar in place, a
    /// `.tar.gz` by unpacking it.
    pub fn open_archive_file(path: &Path) -> Result<Self> {
        match Self::open_tar_file(path)? {
            Some(reader) => Ok(reader),
            None => Self::from_tar_gz(path),
        }
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

        // Either format: a plain tar is read from the downloaded bytes in
        // place, a .tar.gz (an earlier version's) is unpacked.
        Self::from_archive_bytes(data)
    }

    /// Download a segment from the object store and extract it into `dest_dir`
    /// (a caller-owned, PERSISTENT directory), then open it.
    ///
    /// Unlike [`from_object_store`], which extracts into a `TempDir` that is
    /// deleted the instant it returns — leaving the reader backed by unlinked
    /// files — the extracted index here lives for the reader's whole lifetime,
    /// and the directory doubles as a cross-query cache: if `dest_dir` already
    /// holds a fully-extracted index (marked by a `.ready` sentinel) the
    /// download and unpack are skipped. Cache key correctness is the caller's
    /// job — key `dest_dir` by the segment's object key, which is immutable for
    /// a given offset range (a growing topic produces new keys, so the cache
    /// never serves stale data for real appends).
    pub async fn from_object_store_cached(
        object_store: Arc<dyn ObjectStore>,
        object_key: &str,
        dest_dir: &Path,
    ) -> Result<Self> {
        let index_dir = dest_dir.join("index");
        let ready = dest_dir.join(".ready");

        if !ready.exists() {
            let data = object_store.get(object_key)
                .await
                .map_err(|e| Error::Internal(format!("Failed to download from object store: {}", e)))?;
            let dest = dest_dir.to_path_buf();
            // Extraction is blocking I/O — keep it off the async worker.
            tokio::task::spawn_blocking(move || -> Result<()> {
                let _ = fs::remove_dir_all(&dest);
                fs::create_dir_all(&dest)
                    .map_err(|e| Error::Internal(format!("Failed to create cache dir: {}", e)))?;
                let dec = GzDecoder::new(data.as_ref());
                let mut archive = tar::Archive::new(dec);
                archive.unpack(&dest)
                    .map_err(|e| Error::Internal(format!("Failed to unpack tar: {}", e)))?;
                fs::write(dest.join(".ready"), b"")
                    .map_err(|e| Error::Internal(format!("Failed to write ready marker: {}", e)))?;
                Ok(())
            })
            .await
            .map_err(|e| Error::Internal(format!("Extraction task join error: {}", e)))??;
        }

        let metadata_content = fs::read_to_string(dest_dir.join("metadata.json"))
            .map_err(|e| Error::Internal(format!("Failed to read cached metadata: {}", e)))?;
        let metadata: SegmentMetadata = serde_json::from_str(&metadata_content)
            .map_err(|e| Error::Internal(format!("Failed to parse cached metadata: {}", e)))?;
        Self::open(&index_dir, metadata)
    }

    pub fn query_by_offset_range(&self, start: i64, end: i64) -> Result<Vec<CanonicalRecordEntry>> {
        let searcher = self.reader.searcher();
        let start_term = Term::from_field_i64(self.schema_fields.offset, start);
        let end_term = Term::from_field_i64(self.schema_fields.offset, end);
        let query = RangeQuery::new(
            std::ops::Bound::Included(start_term),
            std::ops::Bound::Excluded(end_term),
        );

        // Collect every document in the range, not the top 10,000 of them.
        //
        // `TopDocs::with_limit(10_000)` silently capped this: a segment holding
        // more than 10,000 records in the requested range returned 10,000 and
        // reported no truncation, so a caller reading a range back got a short
        // answer indistinguishable from a complete one. These documents carry no
        // score to rank by, so there was never a meaningful "top" — the cap only
        // decided how much to drop. `DocSetCollector` returns the whole matching
        // set.
        let doc_addresses = searcher
            .search(&query, &tantivy::collector::DocSetCollector)
            .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;

        let mut records = Vec::with_capacity(doc_addresses.len());
        for doc_address in doc_addresses {
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

    pub(super) fn create_test_canonical_record(base_offset: i64, num_records: usize) -> CanonicalRecord {
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
            compressed_records_wire_bytes: None,
            original_v1_wire_format: None,
            original_v2_wire_format: None,
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
        assert!(tar_gz_path.to_str().unwrap().ends_with(".tar"));

        // Deserialize
        let reader = TantivySegmentReader::open_archive_file(&tar_gz_path).unwrap();

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

    /// The archive is built by copying STORED documents out of the writer's
    /// in-memory index. The JSON field is not stored, so a copy that only
    /// moved stored fields shipped archives in which field queries matched
    /// nothing; it has to be rebuilt from `_value` on the way through.
    #[test]
    fn json_values_stay_searchable_by_field_after_archiving() {
        use tantivy::collector::Count;
        use tantivy::query::TermQuery;
        use tantivy::schema::IndexRecordOption;

        let temp_dir = tempfile::tempdir().unwrap();
        let mut writer = TantivySegmentWriter::new("t".to_string(), 0, 0).unwrap();
        let mut canonical = create_test_canonical_record(0, 30);
        for (i, r) in canonical.records.iter_mut().enumerate() {
            let marker = if i == 7 { "quixoticbellwether" } else { "ordinary" };
            r.value = Some(format!(r#"{{"id":{},"marker":"{}"}}"#, i, marker).into_bytes());
        }
        // One non-JSON value: indexed as text only, must not break the copy.
        canonical.records[3].value = Some(b"quixoticbellwether as plain text".to_vec());
        writer.write_batch(&canonical).unwrap();
        let (tar_gz_path, _) = writer.commit_and_serialize(&temp_dir.path().join("out")).unwrap();

        let reader = TantivySegmentReader::open_archive_file(&tar_gz_path).unwrap();
        let index = reader.index();
        let json = index.schema().get_field(crate::json_index::JSON_FIELD).unwrap();

        // Archiving also wrote the term filter beside the archive, and it knows
        // the marker word under its JSON path — as the analyzed term the
        // archive's dictionary holds — and not a word that is absent.
        let filter_bytes = std::fs::read(crate::term_filter::term_filter_key(&tar_gz_path.to_string_lossy()))
            .expect("term filter written beside the archive");
        let filter = crate::term_filter::TermFilter::from_bytes(&filter_bytes).expect("intact filter");
        let analyzed = |word: &str| {
            let mut a = crate::text_analysis::build_analyzer();
            let mut s = a.token_stream(word);
            let mut out = String::new();
            while s.advance() {
                out = s.token().text.clone();
            }
            out
        };
        let mut term = tantivy::Term::from_field_json_path(json, "marker", true);
        term.append_type_and_str(&analyzed("quixoticbellwether"));
        assert!(filter.may_contain(json.field_id(), term.serialized_value_bytes()));
        let mut absent = tantivy::Term::from_field_json_path(json, "marker", true);
        absent.append_type_and_str(&analyzed("zzzneverwrittenzzz"));
        assert!(!filter.may_contain(json.field_id(), absent.serialized_value_bytes()));
        // from_tar_gz unpacks into a temp dir it has already removed; its own
        // open reader still works, a new one cannot take the lock.
        let searcher = reader.reader.searcher();
        let count = |path: &str, word: &str| {
            // Terms are analyzed at index time (lowercase, stop words, stem).
            let mut analyzer = crate::text_analysis::build_analyzer();
            let mut stream = analyzer.token_stream(word);
            let token = stream.next().expect("one token").text.clone();
            let mut term = tantivy::Term::from_field_json_path(json, path, true);
            term.append_type_and_str(&token);
            searcher
                .search(&TermQuery::new(term, IndexRecordOption::Basic), &Count)
                .unwrap()
        };
        assert_eq!(count("marker", "quixoticbellwether"), 1, "only record 7's marker holds it");
        assert_eq!(count("marker", "ordinary"), 28, "30 minus record 7 and the plain-text one");

        // Offset reads use field ids rebuilt from code: still correct.
        let rows = reader.query_by_offset_range(0, 30).unwrap();
        assert_eq!(rows.len(), 30);
        assert_eq!(rows[7].value.as_deref(), Some(br#"{"id":7,"marker":"quixoticbellwether"}"#.as_slice()));
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
        let reader = TantivySegmentReader::open_archive_file(&tar_gz_path).unwrap();
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
        let reader = TantivySegmentReader::open_archive_file(&tar_gz_path).unwrap();
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

/// Issue #57: archiving the committed index raced Tantivy's background merge
/// threads, and when it lost, the only thing the operator saw was
/// `Failed to add index to tar: No such file or directory (os error 2)`.
#[cfg(test)]
mod archive_tests {
    use super::*;

    fn metadata_file(dir: &Path) -> PathBuf {
        let path = dir.join("metadata.json");
        fs::write(&path, b"{}").unwrap();
        path
    }

    /// The failure an operator actually has to diagnose. A bare ENOENT names
    /// neither the file nor the directory nor whether an index was written at
    /// all, so it reads as "the image is missing tar" — which is where issue
    /// #57's analysis went. The message must say where it was looking and what
    /// it found there.
    #[test]
    fn a_failed_archive_names_the_directory_and_its_contents() {
        let temp = tempfile::tempdir().unwrap();
        let metadata_path = metadata_file(temp.path());
        let missing = temp.path().join("index-that-was-never-created");

        let err = TantivySegmentWriter::write_archive(
            &temp.path().join("out.tar.gz"),
            &missing,
            &metadata_path,
        )
        .expect_err("archiving a directory that does not exist must fail");

        let msg = err.to_string();
        assert!(
            msg.contains("index-that-was-never-created"),
            "the error must name the directory it could not archive: {msg}"
        );
        assert!(
            msg.contains("attempts"),
            "the error must say the archive was retried, not that it failed once: {msg}"
        );
        assert!(
            msg.contains("unreadable") || msg.contains("empty"),
            "the error must report what the directory held: {msg}"
        );
    }

    /// An empty-but-present index directory is a different fault from a missing
    /// one, and the message has to distinguish them — this is the shape a
    /// half-written index leaves behind.
    #[test]
    fn an_empty_index_directory_is_reported_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let empty = temp.path().join("index");
        fs::create_dir_all(&empty).unwrap();

        // `append_dir_all` over an existing empty directory succeeds, so this
        // asserts the diagnostic helper rather than the archive.
        assert_eq!(TantivySegmentWriter::describe_dir(&empty), "empty");
        assert!(TantivySegmentWriter::describe_dir(&temp.path().join("gone"))
            .starts_with("unreadable"));
    }

    /// The archive must be a complete, independently readable gzip stream. The
    /// previous code called `tar.finish()` and let the `GzEncoder` drop, which
    /// flushes best-effort and discards the error, so a truncated archive would
    /// have surfaced later as a corrupt cold segment rather than here.
    #[test]
    fn the_archive_is_a_complete_tar_holding_the_whole_index() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("segments");

        let mut writer = TantivySegmentWriter::new("logs".to_string(), 3, 0).unwrap();
        writer
            .write_batch(&tests::create_test_canonical_record(0, 250))
            .unwrap();
        let (tar_gz_path, metadata) = writer.commit_and_serialize(&output_dir).unwrap();

        // A plain tar now (opened in place, not unpacked). List it whole: an
        // archive cut short by an unflushed writer fails here.
        let file = File::open(&tar_gz_path).unwrap();
        assert!(!crate::tar_directory::is_gzip(&std::fs::read(&tar_gz_path).unwrap()), "no longer gzip");
        let mut archive = tar::Archive::new(file);
        let entries: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            entries.iter().any(|p| p == "metadata.json"),
            "metadata.json must be in the archive, got {entries:?}"
        );
        assert!(
            entries.iter().any(|p| p.contains("meta.json")),
            "the Tantivy index's own meta.json must be in the archive, got {entries:?}"
        );

        // And the index must still open and answer from the archive, which is
        // what the cold read path does.
        let reader = TantivySegmentReader::open_archive_file(&tar_gz_path).unwrap();
        assert_eq!(reader.metadata().record_count, 250);
        assert_eq!(metadata.record_count, 250);
        assert_eq!(reader.query_by_offset_range(0, 250).unwrap().len(), 250);
    }

    /// Archives written by earlier versions are `.tar.gz`; they still open,
    /// from a file and from downloaded bytes (the fetch path), and a plain tar
    /// opens from bytes too.
    #[test]
    fn archives_of_either_format_open() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let mut writer = TantivySegmentWriter::new("logs".to_string(), 0, 0).unwrap();
        writer.write_batch(&tests::create_test_canonical_record(0, 40)).unwrap();
        let (tar_path, _) = writer.commit_and_serialize(&temp.path().join("out")).unwrap();
        let tar = std::fs::read(&tar_path).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar).unwrap();
        let gz = gz.finish().unwrap();
        let gz_path = temp.path().join("old.tar.gz");
        std::fs::write(&gz_path, &gz).unwrap();

        for reader in [
            TantivySegmentReader::open_archive_file(&tar_path).unwrap(),
            TantivySegmentReader::open_archive_file(&gz_path).unwrap(),
            TantivySegmentReader::from_archive_bytes(bytes::Bytes::from(tar)).unwrap(),
            TantivySegmentReader::from_archive_bytes(bytes::Bytes::from(gz)).unwrap(),
        ] {
            assert_eq!(reader.query_by_offset_range(0, 40).unwrap().len(), 40);
        }
        assert!(TantivySegmentReader::open_tar_file(&gz_path).unwrap().is_none(), "gzip is not opened in place");
    }

    /// The same path driven through enough batches to make Tantivy flush more
    /// than one in-index segment, so `commit()` has real merge work pending when
    /// the archive starts. This is the production shape that failed: a topic
    /// whose segments were large enough to trigger a merge, while a smaller
    /// topic on the identical binary archived fine.
    #[test]
    fn an_index_with_merge_work_pending_still_archives() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("segments");

        let mut writer = TantivySegmentWriter::new("logs".to_string(), 0, 0).unwrap();
        let per_batch = 2_000;
        let batches = 12;
        for b in 0..batches {
            writer
                .write_batch(&tests::create_test_canonical_record(
                    (b * per_batch) as i64,
                    per_batch,
                ))
                .unwrap();
        }

        let (tar_gz_path, metadata) = writer
            .commit_and_serialize(&output_dir)
            .expect("an index with pending merges must archive, not fail with ENOENT");

        assert_eq!(metadata.record_count, batches * per_batch);

        let reader = TantivySegmentReader::open_archive_file(&tar_gz_path).unwrap();
        assert_eq!(
            reader
                .query_by_offset_range(0, (batches * per_batch) as i64)
                .unwrap()
                .len(),
            batches * per_batch
        );
    }
}
