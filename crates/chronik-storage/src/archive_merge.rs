//! Merge runs of small archived index segments into larger ones.
//!
//! The WalIndexer archives whatever a run found, so a topic written in small
//! bursts piles up small archives: thunderbird's nodes hold 6,000–13,000, the
//! median 26–263 KB. Every one is a separate index to open, filter and search,
//! and a search across all topics pays that per archive. This merges, per
//! partition, runs of consecutive small archives into one, with Tantivy's own
//! segment merge (no re-tokenizing; stored documents copied as they are).
//!
//! Safety:
//! - Only archives whose offset ranges are contiguous are merged: a merged
//!   archive covers exactly the offsets its sources covered (fetch and the
//!   legacy-index retirement trust an archive's range).
//! - The catalogue swaps the sources for the merge in one locked step, and
//!   only if every source is still registered (`replace_segments`): a
//!   truncation or topic delete that ran meanwhile wins and the merge is
//!   abandoned, so deleted records never come back. No offset is ever covered
//!   by two registered archives.
//! - Order: upload the merge and its term filter, swap, then (after a grace
//!   period, so a search or fetch that just looked up a source still finds
//!   it) delete the sources. A crash before the swap leaves an unreferenced
//!   upload; after it, unreferenced sources. Never a missing record.

use crate::object_store::ObjectStore;
use crate::segment_index::{SegmentIndex, SegmentMetadata};
use crate::tantivy_segment::{SegmentMetadata as ArchiveMetadata, TantivySegmentReader, TantivySegmentWriter};
use crate::term_filter::{term_filter_key, TermFilter};
use chronik_common::{Error, Result};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// What a merge pass may combine.
#[derive(Debug, Clone, Copy)]
pub struct MergePolicy {
    /// A merged archive is kept under this size (`CHRONIK_ARCHIVE_MERGE_TARGET_BYTES`).
    pub target_bytes: u64,
    /// Fewest archives worth merging in one run.
    pub min_run: usize,
    /// Unreferenced archive files older than this are swept (crash leftovers;
    /// `CHRONIK_ARCHIVE_ORPHAN_AGE_SECS`).
    pub orphan_age: Duration,
    /// How long merged-away archives stay readable before deletion.
    pub grace: Duration,
}

impl Default for MergePolicy {
    fn default() -> Self {
        Self {
            target_bytes: 64 * 1024 * 1024,
            min_run: 4,
            orphan_age: Duration::from_secs(3600),
            grace: Duration::from_secs(60),
        }
    }
}

/// What a pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MergeOutcome {
    /// Merged archives created.
    pub merges: usize,
    /// Archives merged away.
    pub archives_merged: usize,
    /// Runs abandoned: a source vanished, or the sources could not be merged.
    pub abandoned: usize,
}

/// The runs worth merging among a partition's archives: consecutive,
/// contiguous, each under half the target, together under the target, at
/// least `min_run` long.
pub fn plan(mut archives: Vec<SegmentMetadata>, policy: &MergePolicy) -> Vec<Vec<SegmentMetadata>> {
    archives.sort_by_key(|s| s.min_offset);
    let small = |s: &SegmentMetadata| s.size_bytes < policy.target_bytes / 2;
    let mut runs = Vec::new();
    let mut run: Vec<SegmentMetadata> = Vec::new();
    let mut run_bytes = 0u64;
    for s in archives {
        let continues = run.last().is_some_and(|last: &SegmentMetadata| s.min_offset == last.max_offset + 1)
            && small(&s)
            && run_bytes + s.size_bytes <= policy.target_bytes;
        if !continues {
            if run.len() >= policy.min_run {
                runs.push(std::mem::take(&mut run));
            }
            run.clear();
            run_bytes = 0;
            if !small(&s) {
                continue;
            }
        }
        run_bytes += s.size_bytes;
        run.push(s);
    }
    if run.len() >= policy.min_run {
        runs.push(run);
    }
    runs
}

/// Archive files in the archives' directories that the catalogue does not
/// reference and that are older than `min_age`, deleted. They are what a crash
/// leaves: a merge uploaded but never swapped in, or merged-away sources never
/// deleted. The age keeps the sweep off an archive the indexer has just
/// uploaded and is about to register. Returns how many files went.
pub async fn sweep_orphans(store: &Arc<dyn ObjectStore>, index: &Arc<SegmentIndex>, min_age: Duration) -> usize {
    use std::collections::{BTreeMap, HashSet};
    let norm = |k: &str| k.trim_start_matches('/').to_string();
    let mut dirs: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for s in index.get_all_segments().await.unwrap_or_default() {
        if let Some((dir, _)) = s.object_store_path.rsplit_once('/') {
            dirs.entry(format!("{dir}/")).or_default().insert(norm(&s.object_store_path));
        }
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut swept = 0;
    for (dir, referenced) in dirs {
        let Ok(objects) = store.list(&dir).await else { continue };
        for o in objects {
            let archive = o.key.strip_suffix(crate::term_filter::TERM_FILTER_SUFFIX).unwrap_or(&o.key);
            let is_archive = archive.ends_with(".tar") || archive.ends_with(".tar.gz");
            let file = archive.rsplit('/').next().unwrap_or("");
            if !is_archive || !file.starts_with("segment-") || referenced.contains(&norm(archive)) {
                continue;
            }
            if now_ms.saturating_sub(o.last_modified) < min_age.as_millis() as u64 {
                continue;
            }
            if store.delete(&o.key).await.is_ok() {
                swept += 1;
            }
        }
    }
    if swept > 0 {
        info!(files = swept, "swept archive files no catalogue entry references");
    }
    swept
}

/// Test-only: abort the process at a point of a merge, to prove a crash there
/// loses nothing (`CHRONIK_TEST_ARCHIVE_MERGE_ABORT=after-upload|after-swap`).
fn abort_if_testing(point: &str) {
    if std::env::var("CHRONIK_TEST_ARCHIVE_MERGE_ABORT").ok().as_deref() == Some(point) {
        warn!("CHRONIK_TEST_ARCHIVE_MERGE_ABORT: aborting {point}");
        std::process::abort();
    }
}

/// Merge what `plan` finds in every partition. Returns what was done.
pub async fn merge_all(store: &Arc<dyn ObjectStore>, index: &Arc<SegmentIndex>, policy: &MergePolicy) -> MergeOutcome {
    sweep_orphans(store, index, policy.orphan_age).await;
    let mut outcome = MergeOutcome::default();
    let all = index.get_all_segments().await.unwrap_or_default();
    let mut by_partition: std::collections::BTreeMap<(String, i32), Vec<SegmentMetadata>> = Default::default();
    for s in all {
        by_partition.entry((s.topic.clone(), s.partition)).or_default().push(s);
    }
    for ((topic, partition), archives) in by_partition {
        for run in plan(archives, policy) {
            match merge_run(store, index, &run, policy).await {
                Ok((0, _)) => outcome.abandoned += 1,
                Ok((merges, archives)) => {
                    outcome.merges += merges;
                    outcome.archives_merged += archives;
                }
                Err(e) => {
                    outcome.abandoned += 1;
                    warn!(topic = %topic, partition, "archive merge failed: {}", e);
                }
            }
        }
    }
    if outcome.merges > 0 || outcome.abandoned > 0 {
        info!(
            merges = outcome.merges,
            archives_merged = outcome.archives_merged,
            abandoned = outcome.abandoned,
            "archive merge pass"
        );
    }
    outcome
}

/// Merge one run: `(merged archives created, archives merged away)`, `(0, 0)`
/// when it was abandoned (a source no longer
/// registered, or sources of different schemas).
pub async fn merge_run(
    store: &Arc<dyn ObjectStore>,
    index: &Arc<SegmentIndex>,
    run: &[SegmentMetadata],
    policy: &MergePolicy,
) -> Result<(usize, usize)> {
    let (first, last) = (run.first().expect("a run"), run.last().expect("a run"));
    let topic = first.topic.clone();
    let partition = first.partition;
    let key = merged_key(&first.object_store_path, first.min_offset, last.max_offset);

    // Open every source (kept alive until the merge is written).
    let work = tempfile::tempdir().map_err(|e| Error::Internal(format!("merge temp dir: {e}")))?;
    let mut readers = Vec::with_capacity(run.len());
    for (i, s) in run.iter().enumerate() {
        readers.push(open_source(store, &s.object_store_path, &work.path().join(format!("src-{i}"))).await?);
    }
    let schema = readers[0].index().schema();
    if readers.iter().any(|r| r.index().schema() != schema) {
        // Archives written by different versions (fields were added over
        // time) cannot be merged together: merge each stretch of one schema.
        let schemas: Vec<_> = readers.iter().map(|r| r.index().schema()).collect();
        drop(readers);
        let mut done = (0, 0);
        let mut start = 0;
        for i in 1..=run.len() {
            if i == run.len() || schemas[i] != schemas[start] {
                if i - start >= policy.min_run {
                    let (m, a) = Box::pin(merge_run(store, index, &run[start..i], policy)).await?;
                    done = (done.0 + m, done.1 + a);
                }
                start = i;
            }
        }
        debug!(topic = %topic, partition, merges = done.0, "archives of different schemas merged by stretch");
        return Ok(done);
    }

    let meta = ArchiveMetadata {
        topic: topic.clone(),
        partition,
        base_offset: first.min_offset,
        last_offset: last.max_offset,
        record_count: run.iter().map(|s| s.record_count).sum(),
        min_timestamp: run.iter().map(|s| s.min_timestamp).min().unwrap_or(0),
        max_timestamp: run.iter().map(|s| s.max_timestamp).max().unwrap_or(0),
        created_at: chrono::Utc::now().timestamp(),
        compression: crate::canonical_record::CompressionType::None,
    };
    let out_dir = work.path().join("merged");
    let archive_path = work.path().join("merged.tar");
    let (bytes, filter) = {
        let indices: Vec<tantivy::Index> = readers.iter().map(|r| r.index().clone()).collect();
        let (out_dir, archive_path, meta) = (out_dir.clone(), archive_path.clone(), meta.clone());
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Option<TermFilter>)> {
            let index_dir = out_dir.join("index");
            std::fs::create_dir_all(&index_dir).map_err(|e| Error::Internal(format!("{e}")))?;
            let directory = tantivy::directory::MmapDirectory::open(&index_dir)
                .map_err(|e| Error::Internal(format!("{e}")))?;
            let merged = tantivy::indexer::merge_indices(&indices, directory)
                .map_err(|e| Error::Internal(format!("merge: {e}")))?;
            crate::text_analysis::register_analyzer(&merged);
            let filter = TermFilter::build(&merged).ok();
            let metadata_path = out_dir.join("metadata.json");
            std::fs::write(&metadata_path, serde_json::to_vec_pretty(&meta).map_err(|e| Error::Internal(format!("{e}")))?)
                .map_err(|e| Error::Internal(format!("{e}")))?;
            TantivySegmentWriter::write_archive(&archive_path, &index_dir, &metadata_path)?;
            let bytes = std::fs::read(&archive_path).map_err(|e| Error::Internal(format!("{e}")))?;
            Ok((bytes, filter))
        })
        .await
        .map_err(|e| Error::Internal(format!("merge task: {e}")))??
    };
    drop(readers);

    let size_bytes = bytes.len() as u64;
    store
        .put(&key, bytes::Bytes::from(bytes))
        .await
        .map_err(|e| Error::Internal(format!("upload {key}: {e}")))?;
    if let Some(f) = &filter {
        let _ = store.put(&term_filter_key(&key), bytes::Bytes::from(f.to_bytes())).await;
    }
    abort_if_testing("after-upload");

    let merged = SegmentMetadata {
        segment_id: format!("{}-{}-{}-{}", topic, partition, first.min_offset, last.max_offset),
        topic: topic.clone(),
        partition,
        min_offset: first.min_offset,
        max_offset: last.max_offset,
        record_count: meta.record_count,
        min_timestamp: meta.min_timestamp,
        max_timestamp: meta.max_timestamp,
        object_store_path: key.clone(),
        size_bytes,
        created_at: meta.created_at,
        compression: first.compression.clone(),
    };
    let old_ids: Vec<String> = run.iter().map(|s| s.segment_id.clone()).collect();
    if !index.replace_segments(&old_ids, merged).await? {
        // A source went (truncation, topic delete): the merge must not bring
        // its records back.
        let _ = store.delete(&key).await;
        let _ = store.delete(&term_filter_key(&key)).await;
        return Ok((0, 0));
    }
    abort_if_testing("after-swap");
    info!(topic = %topic, partition, archives = run.len(), min_offset = first.min_offset, max_offset = last.max_offset, size_bytes, "merged archives");

    // The sources, once nothing that looked them up a moment ago still needs them.
    let store = store.clone();
    let doomed: Vec<String> = run.iter().map(|s| s.object_store_path.clone()).filter(|p| *p != key).collect();
    let grace = policy.grace;
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        for path in doomed {
            let _ = store.delete(&path).await;
            let _ = store.delete(&term_filter_key(&path)).await;
        }
    });
    Ok((1, run.len()))
}

/// The merged archive's key: the sources' directory, `segment-<min>-<max>.tar`.
fn merged_key(source_key: &str, min: i64, max: i64) -> String {
    match source_key.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/segment-{min}-{max}.tar"),
        None => format!("segment-{min}-{max}.tar"),
    }
}

/// Open a source archive for merging: a local plain tar in place; anything
/// else downloaded — a plain tar read from memory, a `.tar.gz` unpacked into
/// `scratch` (kept until the merge is written).
async fn open_source(store: &Arc<dyn ObjectStore>, key: &str, scratch: &std::path::Path) -> Result<TantivySegmentReader> {
    if let Some(path) = store.local_path(key).filter(|p| p.is_file()) {
        if let Some(reader) = TantivySegmentReader::open_tar_file(&path)? {
            return Ok(reader);
        }
    }
    let bytes = store.get(key).await.map_err(|e| Error::Internal(format!("download {key}: {e}")))?;
    if !crate::tar_directory::is_gzip(&bytes) {
        return TantivySegmentReader::open_tar(crate::tar_directory::owned(bytes));
    }
    let scratch = scratch.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<TantivySegmentReader> {
        std::fs::create_dir_all(&scratch).map_err(|e| Error::Internal(format!("{e}")))?;
        tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_ref()))
            .unpack(&scratch)
            .map_err(|e| Error::Internal(format!("unpack: {e}")))?;
        let metadata: ArchiveMetadata = serde_json::from_slice(
            &std::fs::read(scratch.join("metadata.json")).map_err(|e| Error::Internal(format!("{e}")))?,
        )
        .map_err(|e| Error::Internal(format!("{e}")))?;
        TantivySegmentReader::open(&scratch.join("index"), metadata)
    })
    .await
    .map_err(|e| Error::Internal(format!("open task: {e}")))?
}

/// Run merge passes in the background: first after `CHRONIK_ARCHIVE_MERGE_DELAY_SECS`
/// (default 300), then every `CHRONIK_ARCHIVE_MERGE_INTERVAL_SECS` (default 900).
/// `CHRONIK_ARCHIVE_MERGE=false` turns it off.
pub fn spawn(store: Arc<dyn ObjectStore>, index: Arc<SegmentIndex>) {
    let env = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string());
    if matches!(env("CHRONIK_ARCHIVE_MERGE").as_deref(), Some("false" | "0" | "off" | "no")) {
        return;
    }
    let secs = |name: &str, default: u64| env(name).and_then(|v| v.parse().ok()).unwrap_or(default);
    let mut policy = MergePolicy::default();
    if let Some(t) = env("CHRONIK_ARCHIVE_MERGE_TARGET_BYTES").and_then(|v| v.parse().ok()) {
        policy.target_bytes = t;
    }
    policy.grace = Duration::from_secs(secs("CHRONIK_ARCHIVE_MERGE_GRACE_SECS", 60));
    policy.orphan_age = Duration::from_secs(secs("CHRONIK_ARCHIVE_ORPHAN_AGE_SECS", 3600));
    let delay = Duration::from_secs(secs("CHRONIK_ARCHIVE_MERGE_DELAY_SECS", 300));
    let interval = Duration::from_secs(secs("CHRONIK_ARCHIVE_MERGE_INTERVAL_SECS", 900));
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        loop {
            merge_all(&store, &index, &policy).await;
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(lo: i64, hi: i64, size: u64) -> SegmentMetadata {
        SegmentMetadata {
            segment_id: format!("t-0-{lo}-{hi}"),
            topic: "t".into(),
            partition: 0,
            min_offset: lo,
            max_offset: hi,
            record_count: (hi - lo + 1) as usize,
            min_timestamp: 0,
            max_timestamp: 0,
            object_store_path: format!("idx/t/partition-0/segment-{lo}-{hi}.tar"),
            size_bytes: size,
            created_at: 0,
            compression: String::new(),
        }
    }

    #[test]
    fn plans_only_contiguous_runs_of_small_archives() {
        let policy = MergePolicy { target_bytes: 1000, min_run: 3, orphan_age: Duration::ZERO, grace: Duration::ZERO };
        let archives = vec![
            seg(0, 9, 100), seg(10, 19, 100), seg(20, 29, 100), // run A
            seg(30, 39, 900),                                   // too big: breaks the run
            seg(40, 49, 100), seg(50, 59, 100),                 // too short
            seg(61, 69, 100), seg(70, 79, 100), seg(80, 89, 100), seg(90, 99, 100), // gap at 60: run B
        ];
        let runs = plan(archives, &policy);
        let spans: Vec<(i64, i64)> = runs.iter().map(|r| (r[0].min_offset, r.last().unwrap().max_offset)).collect();
        assert_eq!(spans, vec![(0, 29), (61, 99)]);
        // The target caps a run.
        let many: Vec<_> = (0..20).map(|i| seg(i * 10, i * 10 + 9, 300)).collect();
        for run in plan(many, &policy) {
            assert!(run.iter().map(|s| s.size_bytes).sum::<u64>() <= 1000);
        }
    }

    use crate::canonical_record::{CanonicalRecord, CanonicalRecordEntry, CompressionType, TimestampType};
    use crate::object_store::{ObjectStoreConfig, ObjectStoreFactory, StorageBackend};

    async fn local_store(dir: &std::path::Path) -> Arc<dyn ObjectStore> {
        let config = ObjectStoreConfig {
            backend: StorageBackend::Local { path: dir.to_string_lossy().to_string() },
            bucket: "test".to_string(),
            prefix: None,
            connection: Default::default(),
            performance: Default::default(),
            retry: Default::default(),
            auth: Default::default(),
            default_metadata: None,
            encryption: None,
        };
        Arc::from(ObjectStoreFactory::create(config).await.unwrap())
    }

    /// `n` archives of 10 records each (offsets 0..10n), registered, in either
    /// format. Record 37 holds the word "needleword".
    async fn archives(store: &Arc<dyn ObjectStore>, index: &Arc<SegmentIndex>, n: i64, gzip: bool) {
        for a in 0..n {
            let base = a * 10;
            let records = (base..base + 10)
                .map(|o| CanonicalRecordEntry {
                    offset: o,
                    timestamp: 1_700_000_000_000 + o,
                    key: None,
                    value: Some(format!("{{\"n\":{o},\"w\":\"{}\"}}", if o == 37 { "needleword" } else { "plain" }).into_bytes()),
                    headers: vec![],
                    attributes: 0,
                })
                .collect();
            let batch = CanonicalRecord {
                base_offset: base,
                partition_leader_epoch: 0,
                producer_id: -1,
                producer_epoch: -1,
                base_sequence: -1,
                is_transactional: false,
                is_control: false,
                compression: CompressionType::None,
                timestamp_type: TimestampType::CreateTime,
                base_timestamp: 1_700_000_000_000 + base,
                max_timestamp: 1_700_000_000_000 + base + 9,
                records,
                compressed_records_wire_bytes: None,
                original_v1_wire_format: None,
                original_v2_wire_format: None,
            };
            let mut w = TantivySegmentWriter::new("t".into(), 0, base).unwrap();
            w.write_batch(&batch).unwrap();
            let out = tempfile::tempdir().unwrap();
            let (path, _) = w.commit_and_serialize(out.path()).unwrap();
            let mut data = std::fs::read(&path).unwrap();
            let ext = if gzip {
                use std::io::Write;
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                gz.write_all(&data).unwrap();
                data = gz.finish().unwrap();
                "tar.gz"
            } else {
                "tar"
            };
            let key = format!("idx/t/partition-0/segment-{}-{}.{ext}", base, base + 9);
            let size = data.len() as u64;
            store.put(&key, bytes::Bytes::from(data)).await.unwrap();
            index
                .add_segment(SegmentMetadata { object_store_path: key, size_bytes: size, ..seg(base, base + 9, size) })
                .await
                .unwrap();
        }
    }

    fn policy() -> MergePolicy {
        MergePolicy { target_bytes: 64 * 1024 * 1024, min_run: 3, orphan_age: Duration::from_secs(3600), grace: Duration::ZERO }
    }

    #[tokio::test]
    async fn merges_a_run_into_one_archive_holding_every_record() {
        for gzip in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = local_store(dir.path()).await;
            let index = Arc::new(SegmentIndex::new());
            archives(&store, &index, 6, gzip).await;

            let outcome = merge_all(&store, &index, &policy()).await;
            assert_eq!(outcome, MergeOutcome { merges: 1, archives_merged: 6, abandoned: 0 }, "gzip={gzip}");
            let segs = index.get_segments_for_partition("t", 0).await.unwrap();
            assert_eq!(segs.len(), 1);
            let merged = &segs[0];
            assert_eq!((merged.min_offset, merged.max_offset, merged.record_count), (0, 59, 60));

            let reader = TantivySegmentReader::from_archive_bytes(store.get(&merged.object_store_path).await.unwrap()).unwrap();
            let rows = reader.query_by_offset_range(0, 60).unwrap();
            assert_eq!(rows.iter().map(|r| r.offset).collect::<Vec<_>>(), (0..60).collect::<Vec<_>>());
            let filter = TermFilter::from_bytes(&store.get(&term_filter_key(&merged.object_store_path)).await.unwrap()).unwrap();
            let value = reader.index().schema().get_field("value").unwrap();
            let mut a = crate::text_analysis::build_analyzer();
            let mut s = a.token_stream("needleword");
            s.advance();
            let term = tantivy::Term::from_field_text(value, &s.token().text);
            assert!(filter.may_contain(value.field_id(), term.serialized_value_bytes()));

            // The sources go after the grace period (zero here).
            for _ in 0..50 {
                if store.get("idx/t/partition-0/segment-0-9.tar").await.is_err()
                    && store.get("idx/t/partition-0/segment-0-9.tar.gz").await.is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let ext = if gzip { "tar.gz" } else { "tar" };
            assert!(store.get(&format!("idx/t/partition-0/segment-0-9.{ext}")).await.is_err(), "source deleted");
        }
    }

    /// A source removed while the merge ran (a truncation, a topic delete):
    /// the merge is abandoned and leaves nothing behind.
    #[tokio::test]
    async fn a_vanished_source_abandons_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        archives(&store, &index, 4, false).await;
        let run = plan(index.get_segments_for_partition("t", 0).await.unwrap(), &policy()).remove(0);
        index.remove_segment("t", 0, &run[2].segment_id).await.unwrap();

        assert_eq!(merge_run(&store, &index, &run, &policy()).await.unwrap(), (0, 0));
        assert_eq!(index.get_segments_for_partition("t", 0).await.unwrap().len(), 3, "untouched");
        assert!(store.get("idx/t/partition-0/segment-0-39.tar").await.is_err(), "no merged archive left");
        assert!(store.get("idx/t/partition-0/segment-0-9.tar").await.is_ok(), "sources kept");
    }

    /// An archive in an older schema (no JSON or text fields), as earlier
    /// versions wrote them: 10 records from `base`.
    async fn old_schema_archive(store: &Arc<dyn ObjectStore>, index: &Arc<SegmentIndex>, base: i64) {
        use tantivy::schema::{DateOptions, NumericOptions, Schema};
        let work = tempfile::tempdir().unwrap();
        let index_dir = work.path().join("index");
        std::fs::create_dir_all(&index_dir).unwrap();
        let mut b = Schema::builder();
        let o = b.add_i64_field("_offset", NumericOptions::default().set_indexed().set_stored().set_fast());
        let t = b.add_date_field("_ts", DateOptions::default().set_indexed().set_stored().set_fast());
        let idx = tantivy::Index::create_in_dir(&index_dir, b.build()).unwrap();
        let mut w: tantivy::IndexWriter = idx.writer(15_000_000).unwrap();
        for off in base..base + 10 {
            let mut d = tantivy::TantivyDocument::default();
            d.add_i64(o, off);
            d.add_date(t, tantivy::DateTime::from_timestamp_millis(off));
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
        w.wait_merging_threads().unwrap();
        let meta = ArchiveMetadata {
            topic: "t".into(),
            partition: 0,
            base_offset: base,
            last_offset: base + 9,
            record_count: 10,
            min_timestamp: base,
            max_timestamp: base + 9,
            created_at: 0,
            compression: CompressionType::None,
        };
        let meta_path = work.path().join("metadata.json");
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        let tar_path = work.path().join("a.tar");
        TantivySegmentWriter::write_archive(&tar_path, &index_dir, &meta_path).unwrap();
        let data = std::fs::read(&tar_path).unwrap();
        let key = format!("idx/t/partition-0/segment-{}-{}.tar", base, base + 9);
        let size = data.len() as u64;
        store.put(&key, bytes::Bytes::from(data)).await.unwrap();
        index
            .add_segment(SegmentMetadata { object_store_path: key, size_bytes: size, ..seg(base, base + 9, size) })
            .await
            .unwrap();
    }

    /// A run spanning archives of two versions' schemas is merged stretch by
    /// stretch, never across the boundary.
    #[tokio::test]
    async fn archives_of_different_versions_merge_separately() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        archives(&store, &index, 4, false).await; // 0-39, current schema
        for base in [40, 50, 60, 70] {
            old_schema_archive(&store, &index, base).await; // 40-79, older schema
        }
        let outcome = merge_all(&store, &index, &policy()).await;
        assert_eq!((outcome.merges, outcome.archives_merged), (2, 8));
        let spans: Vec<(i64, i64)> = index
            .get_segments_for_partition("t", 0)
            .await
            .unwrap()
            .iter()
            .map(|s| (s.min_offset, s.max_offset))
            .collect();
        assert_eq!(spans, vec![(0, 39), (40, 79)]);
    }

    #[tokio::test]
    async fn the_sweep_removes_only_old_unreferenced_archive_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        archives(&store, &index, 2, false).await;
        let referenced = "idx/t/partition-0/segment-0-9.tar";
        store.put(&term_filter_key(referenced), bytes::Bytes::from_static(b"f")).await.unwrap();
        let orphan = "idx/t/partition-0/segment-100-199.tar";
        store.put(orphan, bytes::Bytes::from_static(b"x")).await.unwrap();
        store.put(&term_filter_key(orphan), bytes::Bytes::from_static(b"f")).await.unwrap();
        store.put("idx/t/partition-0/notes.txt", bytes::Bytes::from_static(b"n")).await.unwrap();

        // Too young to be a leftover: kept (the indexer may be about to register it).
        assert_eq!(sweep_orphans(&store, &index, Duration::from_secs(3600)).await, 0);
        assert_eq!(sweep_orphans(&store, &index, Duration::ZERO).await, 2);
        assert!(store.get(orphan).await.is_err());
        assert!(store.get(&term_filter_key(orphan)).await.is_err());
        assert!(store.get(referenced).await.is_ok(), "a referenced archive stays");
        assert!(store.get(&term_filter_key(referenced)).await.is_ok(), "and its filter");
        assert!(store.get("idx/t/partition-0/notes.txt").await.is_ok(), "anything else stays");
    }

    #[test]
    fn the_merge_lands_beside_its_sources() {
        assert_eq!(merged_key("/data/ti/t/partition-2/segment-0-9.tar.gz", 0, 99), "/data/ti/t/partition-2/segment-0-99.tar");
    }
}
