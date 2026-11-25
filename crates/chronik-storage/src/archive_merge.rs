//! Merge runs of archived index segments into larger ones, by size tier.
//!
//! The WalIndexer archives each sealed WAL segment separately, so archives
//! pile up: small ones from topics written in bursts (thunderbird's nodes held
//! 6,000–13,000, median 26–263 KB) and, on busy partitions, one ~50 MB archive
//! per 250 MB segment — six per million small records, without end. Every one
//! is a separate index to open, filter and search. This merges, per partition,
//! runs of consecutive archives of the same size tier into one, with Tantivy's
//! own segment merge (no re-tokenizing; stored documents copied as they are).
//!
//! Tiers (see [`tier`]) grow by `min_run`: archives under `tier_base_bytes`
//! are tier 0, then each tier spans a factor of `min_run`. Merging `min_run`
//! archives of one tier yields one of the next, so a record is rewritten about
//! once per tier (log of the archive size), and a partition holds at most a
//! few archives per tier: their number grows with the logarithm of its data,
//! up to archives of `target_bytes`. (Merging any small archive into a growing
//! neighbour instead rewrites that neighbour on every pass.)
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
    /// Fewest archives worth merging in one run, and the factor between size
    /// tiers.
    pub min_run: usize,
    /// Archives under this size are all tier 0 (`CHRONIK_ARCHIVE_MERGE_TIER_BASE_BYTES`).
    pub tier_base_bytes: u64,
    /// Unreferenced archive files older than this are swept (crash leftovers;
    /// `CHRONIK_ARCHIVE_ORPHAN_AGE_SECS`).
    pub orphan_age: Duration,
    /// How long merged-away archives stay readable before deletion.
    pub grace: Duration,
}

impl Default for MergePolicy {
    fn default() -> Self {
        Self {
            // Archives are unpacked into the search cache (4 GB by default)
            // to be searched; a few of the largest must fit at once.
            target_bytes: 1024 * 1024 * 1024,
            min_run: 4,
            tier_base_bytes: 16 * 1024 * 1024,
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
    /// Runs given up because a source left the catalogue while they were being
    /// merged (a truncation, a topic delete). The next pass plans afresh.
    pub raced: usize,
    /// Runs that can never merge as planned: their archives were written in
    /// different index schemas and no single-schema stretch is long enough.
    /// Remembered, so later passes do not reopen them; any change to the
    /// partition's archives yields a different run, which is planned anew.
    pub ineligible: usize,
    /// Runs that failed (download, merge or upload error).
    pub failed: usize,
}

/// What became of one run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunOutcome {
    /// `merges` merged archives replaced `archives` sources.
    Merged { merges: usize, archives: usize },
    /// A source left the catalogue during the merge; nothing was changed.
    Raced,
    /// Archives of different schemas, no stretch of one schema long enough.
    Ineligible,
}

/// The identity of a planned run: its sources' segment ids.
fn run_key(run: &[SegmentMetadata]) -> String {
    run.iter().map(|s| s.segment_id.as_str()).collect::<Vec<_>>().join("|")
}

/// The size tier of an archive of `size` bytes: 0 under `tier_base_bytes`,
/// then one more per factor of `min_run`.
pub fn tier(size: u64, policy: &MergePolicy) -> u32 {
    let base = policy.tier_base_bytes.max(1);
    if size < base {
        return 0;
    }
    let factor = policy.min_run.max(2) as u64;
    let mut t = 1;
    let mut bound = base.saturating_mul(factor);
    while size >= bound && bound < u64::MAX {
        t += 1;
        bound = bound.saturating_mul(factor);
    }
    t
}

/// The runs worth merging among a partition's archives: consecutive,
/// contiguous, of one size tier, together under the target, at least
/// `min_run` long.
pub fn plan(mut archives: Vec<SegmentMetadata>, policy: &MergePolicy) -> Vec<Vec<SegmentMetadata>> {
    archives.sort_by_key(|s| s.min_offset);
    let mut runs = Vec::new();
    let mut run: Vec<SegmentMetadata> = Vec::new();
    let mut run_bytes = 0u64;
    let mut run_tier = 0u32;
    for s in archives {
        let s_tier = tier(s.size_bytes, policy);
        let continues = run.last().is_some_and(|last: &SegmentMetadata| s.min_offset == last.max_offset + 1)
            && s_tier == run_tier
            && run_bytes + s.size_bytes <= policy.target_bytes;
        if !continues {
            if run.len() >= policy.min_run {
                runs.push(std::mem::take(&mut run));
            }
            run.clear();
            run_bytes = 0;
            run_tier = s_tier;
            // An archive that could never join `min_run` of its kind under
            // the target starts nothing.
            if s.size_bytes.saturating_mul(policy.min_run as u64) > policy.target_bytes {
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
    // Per archive directory (one partition): the referenced keys and the
    // offset ranges they cover.
    let mut dirs: BTreeMap<String, (HashSet<String>, Vec<(i64, i64)>)> = BTreeMap::new();
    for s in index.get_all_segments().await.unwrap_or_default() {
        if let Some((dir, _)) = s.object_store_path.rsplit_once('/') {
            let entry = dirs.entry(format!("{dir}/")).or_default();
            entry.0.insert(norm(&s.object_store_path));
            entry.1.push((s.min_offset, s.max_offset));
        }
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let (mut swept, mut kept) = (0usize, 0usize);
    for (dir, (referenced, ranges)) in dirs {
        let covered = merge_ranges(ranges);
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
            // Only a file whose every offset a registered archive holds: an
            // unreferenced archive with offsets nothing else covers may be the
            // only copy of those records, and is kept.
            let redundant = file_range(file).is_some_and(|(lo, hi)| {
                covered.iter().any(|&(clo, chi)| clo <= lo && hi <= chi)
            });
            if !redundant {
                kept += 1;
                continue;
            }
            if store.delete(&o.key).await.is_ok() {
                swept += 1;
            }
        }
    }
    if swept > 0 || kept > 0 {
        info!(
            swept,
            kept_uncovered = kept,
            "swept archive files no catalogue entry references (kept those holding offsets no registered archive covers)"
        );
    }
    swept
}

/// The offsets an archive's file name says it holds: `segment-<min>-<max>.tar[.gz]`.
fn file_range(file: &str) -> Option<(i64, i64)> {
    let stem = file.strip_prefix("segment-")?;
    let stem = stem.strip_suffix(".tar.gz").or_else(|| stem.strip_suffix(".tar"))?;
    let (lo, rest) = stem.split_once('-')?;
    // A reindexed archive carries a suffix: `segment-<lo>-<hi>-r<id>.tar`.
    let hi = rest.split('-').next()?;
    let (lo, hi) = (lo.parse().ok()?, hi.parse().ok()?);
    (lo <= hi).then_some((lo, hi))
}

fn merge_ranges(mut ranges: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

/// Bring every registered archive to the current format: a `.tar.gz` (written
/// before v2.18.0) is rewritten as a plain `.tar`, so it is read in place
/// instead of unpacked; any archive without a term filter gets one, so a word
/// search can skip it without opening it. Without this, a cluster whose
/// archives predate v2.18.0 kept unpacking them on every search — merging
/// rewrites only runs of small archives, and most partitions have one or two.
/// Each rewrite is swapped in like a merge (`replace_segments`, then the old
/// file deleted after the grace period). Returns (converted, filters built).
pub async fn upgrade_archives(store: &Arc<dyn ObjectStore>, index: &Arc<SegmentIndex>, policy: &MergePolicy) -> (usize, usize) {
    let (mut converted, mut filtered) = (0usize, 0usize);
    for seg in index.get_all_segments().await.unwrap_or_default() {
        let key = seg.object_store_path.clone();
        let result = if let Some(stem) = key.strip_suffix(".tar.gz") {
            convert_one(store, index, &seg, &format!("{stem}.tar"), policy).await.map(|done| {
                if done {
                    converted += 1;
                }
            })
        } else if key.ends_with(".tar") && store.head(&term_filter_key(&key)).await.is_err() {
            build_filter(store, &key).await.map(|_| filtered += 1)
        } else {
            Ok(())
        };
        if let Err(e) = result {
            debug!(archive = %key, "archive upgrade skipped: {}", e);
        }
    }
    if converted > 0 || filtered > 0 {
        info!(converted, filters_built = filtered, "archives brought to the current format");
    }
    (converted, filtered)
}

async fn convert_one(
    store: &Arc<dyn ObjectStore>,
    index: &Arc<SegmentIndex>,
    seg: &SegmentMetadata,
    new_key: &str,
    policy: &MergePolicy,
) -> Result<bool> {
    let old_key = seg.object_store_path.clone();
    let gz = store.get(&old_key).await.map_err(|e| Error::Internal(format!("download {old_key}: {e}")))?;
    let (tar, filter) = tokio::task::spawn_blocking(move || -> Result<(bytes::Bytes, Option<TermFilter>)> {
        use std::io::Read;
        let mut tar = Vec::with_capacity(gz.len() * 3 / 2);
        flate2::read::GzDecoder::new(gz.as_ref())
            .read_to_end(&mut tar)
            .map_err(|e| Error::Internal(format!("gunzip: {e}")))?;
        let tar = bytes::Bytes::from(tar);
        // It must open as an archive before it replaces one.
        let reader = TantivySegmentReader::open_tar(crate::tar_directory::owned(tar.clone()))?;
        let filter = TermFilter::build(reader.index()).ok();
        Ok((tar, filter))
    })
    .await
    .map_err(|e| Error::Internal(format!("convert task: {e}")))??;

    let size_bytes = tar.len() as u64;
    store.put(new_key, tar).await.map_err(|e| Error::Internal(format!("upload {new_key}: {e}")))?;
    if let Some(f) = &filter {
        let _ = store.put(&term_filter_key(new_key), bytes::Bytes::from(f.to_bytes())).await;
    }
    let replacement = SegmentMetadata { object_store_path: new_key.to_string(), size_bytes, ..seg.clone() };
    if !index.replace_segments(&[seg.segment_id.clone()], replacement).await? {
        let _ = store.delete(new_key).await;
        let _ = store.delete(&term_filter_key(new_key)).await;
        return Ok(false);
    }
    let store = store.clone();
    let grace = policy.grace;
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        let _ = store.delete(&old_key).await;
        let _ = store.delete(&term_filter_key(&old_key)).await;
    });
    Ok(true)
}

async fn build_filter(store: &Arc<dyn ObjectStore>, key: &str) -> Result<()> {
    let scratch = tempfile::tempdir().map_err(|e| Error::Internal(format!("{e}")))?;
    let reader = open_source(store, key, scratch.path()).await?;
    let filter = tokio::task::spawn_blocking(move || TermFilter::build(reader.index()))
        .await
        .map_err(|e| Error::Internal(format!("filter task: {e}")))??;
    store
        .put(&term_filter_key(key), bytes::Bytes::from(filter.to_bytes()))
        .await
        .map_err(|e| Error::Internal(format!("upload filter: {e}")))
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
///
/// `ineligible` carries, from pass to pass, the runs found unmergeable: they
/// are skipped instead of being reopened every pass. Thunderbird node 3 reopened
/// one mixed-schema run of four archives every 15 minutes, reported only as
/// `abandoned=1`, with no partition and no reason.
pub async fn merge_all(
    store: &Arc<dyn ObjectStore>,
    index: &Arc<SegmentIndex>,
    policy: &MergePolicy,
    ineligible: &mut std::collections::HashSet<String>,
) -> MergeOutcome {
    sweep_orphans(store, index, policy.orphan_age).await;
    let mut outcome = MergeOutcome::default();
    let all = index.get_all_segments().await.unwrap_or_default();
    let mut by_partition: std::collections::BTreeMap<(String, i32), Vec<SegmentMetadata>> = Default::default();
    for s in all {
        by_partition.entry((s.topic.clone(), s.partition)).or_default().push(s);
    }
    for ((topic, partition), archives) in by_partition {
        for run in plan(archives, policy) {
            let key = run_key(&run);
            if ineligible.contains(&key) {
                continue;
            }
            let (lo, hi) = (run[0].min_offset, run[run.len() - 1].max_offset);
            match merge_run(store, index, &run, policy).await {
                Ok(RunOutcome::Merged { merges, archives }) => {
                    outcome.merges += merges;
                    outcome.archives_merged += archives;
                }
                Ok(RunOutcome::Raced) => {
                    outcome.raced += 1;
                    info!(
                        topic = %topic, partition, min_offset = lo, max_offset = hi,
                        "archive merge given up: a source left the catalogue during the merge"
                    );
                }
                Ok(RunOutcome::Ineligible) => {
                    outcome.ineligible += 1;
                    info!(
                        topic = %topic, partition, min_offset = lo, max_offset = hi, archives = run.len(),
                        "archives not merged: written in different index schemas and no stretch of one \
                         schema is long enough; not retried until the partition's archives change"
                    );
                    ineligible.insert(key);
                }
                Err(e) => {
                    outcome.failed += 1;
                    warn!(topic = %topic, partition, min_offset = lo, max_offset = hi, "archive merge failed: {}", e);
                }
            }
        }
    }
    // Forget runs whose archives are gone, so the set holds only runs the
    // catalogue still has.
    if !ineligible.is_empty() {
        let live: std::collections::HashSet<String> =
            index.get_all_segments().await.unwrap_or_default().into_iter().map(|s| s.segment_id).collect();
        ineligible.retain(|k| k.split('|').all(|id| live.contains(id)));
    }
    // What was not merged is brought to the current format (plain tar, with
    // a term filter).
    upgrade_archives(store, index, policy).await;
    if outcome.merges > 0 || outcome.raced > 0 || outcome.ineligible > 0 || outcome.failed > 0 {
        info!(
            merges = outcome.merges,
            archives_merged = outcome.archives_merged,
            raced = outcome.raced,
            ineligible = outcome.ineligible,
            failed = outcome.failed,
            "archive merge pass"
        );
    }
    outcome
}

/// Merge one run. See [`RunOutcome`].
pub async fn merge_run(
    store: &Arc<dyn ObjectStore>,
    index: &Arc<SegmentIndex>,
    run: &[SegmentMetadata],
    policy: &MergePolicy,
) -> Result<RunOutcome> {
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
    // Archives written by different versions cannot be merged together:
    // fields were added over time, and archives written since v2.20.2 keep
    // their records newest first (index sorting, `archive_settings`) while
    // older ones are in offset order. Merge each stretch of one kind.
    let kind = |r: &TantivySegmentReader| (r.index().schema(), r.index().settings().clone());
    let first_kind = kind(&readers[0]);
    if readers.iter().any(|r| kind(r) != first_kind) {
        let schemas: Vec<_> = readers.iter().map(kind).collect();
        drop(readers);
        let (mut merges, mut archives, mut raced) = (0, 0, false);
        let mut start = 0;
        for i in 1..=run.len() {
            if i == run.len() || schemas[i] != schemas[start] {
                if i - start >= policy.min_run {
                    match Box::pin(merge_run(store, index, &run[start..i], policy)).await? {
                        RunOutcome::Merged { merges: m, archives: a } => {
                            merges += m;
                            archives += a;
                        }
                        RunOutcome::Raced => raced = true,
                        RunOutcome::Ineligible => {}
                    }
                }
                start = i;
            }
        }
        debug!(topic = %topic, partition, merges, "archives of different schemas merged by stretch");
        return Ok(if merges > 0 {
            RunOutcome::Merged { merges, archives }
        } else if raced {
            RunOutcome::Raced
        } else {
            RunOutcome::Ineligible
        });
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
        return Ok(RunOutcome::Raced);
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
    Ok(RunOutcome::Merged { merges: 1, archives: run.len() })
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
/// Open a source archive, checked against its checksums: merging a damaged
/// archive would carry the damage into the merged one (and delete the
/// original). A damaged source is recorded (`archive_health`) and rebuilt.
pub(crate) async fn open_source(store: &Arc<dyn ObjectStore>, key: &str, scratch: &std::path::Path) -> Result<TantivySegmentReader> {
    if crate::archive_health::is_damaged(key) {
        return Err(Error::Storage(format!("{key} is damaged (awaiting rebuild)")));
    }
    let reader = open_source_unchecked(store, key, scratch).await?;
    let key = key.to_string();
    tokio::task::spawn_blocking(move || crate::archive_health::check(&key, &reader).map(|()| reader))
        .await
        .map_err(|e| Error::Internal(format!("verify task: {e}")))?
}

async fn open_source_unchecked(store: &Arc<dyn ObjectStore>, key: &str, scratch: &std::path::Path) -> Result<TantivySegmentReader> {
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
    if let Some(t) = env("CHRONIK_ARCHIVE_MERGE_TIER_BASE_BYTES").and_then(|v| v.parse().ok()) {
        policy.tier_base_bytes = t;
    }
    policy.grace = Duration::from_secs(secs("CHRONIK_ARCHIVE_MERGE_GRACE_SECS", 60));
    policy.orphan_age = Duration::from_secs(secs("CHRONIK_ARCHIVE_ORPHAN_AGE_SECS", 3600));
    let delay = Duration::from_secs(secs("CHRONIK_ARCHIVE_MERGE_DELAY_SECS", 300));
    let interval = Duration::from_secs(secs("CHRONIK_ARCHIVE_MERGE_INTERVAL_SECS", 900));
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let mut ineligible = std::collections::HashSet::new();
        loop {
            merge_all(&store, &index, &policy, &mut ineligible).await;
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

    fn tiered(target: u64, min_run: usize, base: u64) -> MergePolicy {
        MergePolicy { target_bytes: target, min_run, tier_base_bytes: base, orphan_age: Duration::ZERO, grace: Duration::ZERO }
    }

    #[test]
    fn tiers_grow_by_the_run_length() {
        let p = tiered(1 << 40, 4, 16);
        let tiers: Vec<u32> = [0, 15, 16, 63, 64, 255, 256, 1023, 1024].iter().map(|s| tier(*s, &p)).collect();
        assert_eq!(tiers, vec![0, 0, 1, 1, 2, 2, 3, 3, 4]);
        assert_eq!(tier(u64::MAX, &p), tier(u64::MAX, &p)); // terminates
    }

    #[test]
    fn plans_runs_of_one_tier_only() {
        let p = tiered(10_000, 4, 16);
        // Four tier-2 archives (64..255) then four tier-1 (16..63), contiguous.
        let mut archives: Vec<_> = (0..4).map(|i| seg(i * 10, i * 10 + 9, 100)).collect();
        archives.extend((4..8).map(|i| seg(i * 10, i * 10 + 9, 40)));
        let spans: Vec<(i64, i64)> =
            plan(archives, &p).iter().map(|r| (r[0].min_offset, r.last().unwrap().max_offset)).collect();
        assert_eq!(spans, vec![(0, 39), (40, 79)]);
        // Alternating tiers: nothing has three of a kind in a row.
        let mixed: Vec<_> = (0..8).map(|i| seg(i * 10, i * 10 + 9, if i % 2 == 0 { 100 } else { 40 })).collect();
        assert!(plan(mixed, &p).is_empty());
    }

    /// Steady ingest of ~46 MB archives (one per sealed 250 MB WAL segment),
    /// a merge pass after every new archive: the archive count stays roughly
    /// logarithmic and each byte is rewritten a few times, not once per pass.
    #[test]
    fn tiered_merging_keeps_archive_count_and_rewrites_low() {
        const MB: u64 = 1024 * 1024;
        let p = MergePolicy::default();
        let mut archives: Vec<SegmentMetadata> = Vec::new();
        let mut written = 0u64; // bytes written by merges
        let incoming = 600u64;
        for i in 0..incoming as i64 {
            archives.push(seg(i * 1000, i * 1000 + 999, 46 * MB));
            loop {
                let runs = plan(archives.clone(), &p);
                if runs.is_empty() {
                    break;
                }
                for run in runs {
                    let ids: std::collections::HashSet<_> = run.iter().map(|s| s.segment_id.clone()).collect();
                    let size: u64 = run.iter().map(|s| s.size_bytes).sum();
                    written += size;
                    archives.retain(|s| !ids.contains(&s.segment_id));
                    archives.push(seg(run[0].min_offset, run.last().unwrap().max_offset, size));
                }
            }
        }
        // Every offset still covered exactly once.
        archives.sort_by_key(|s| s.min_offset);
        assert_eq!(archives[0].min_offset, 0);
        assert!(archives.windows(2).all(|w| w[1].min_offset == w[0].max_offset + 1));
        assert_eq!(archives.last().unwrap().max_offset, incoming as i64 * 1000 - 1);
        // 600 archives (27.6 GB) end as ~736 MB archives plus a short tail of
        // smaller tiers, instead of 600.
        assert!(archives.len() <= 50, "{} archives", archives.len());
        assert!(archives.iter().all(|s| s.size_bytes <= p.target_bytes));
        let rewrites = written as f64 / (incoming * 46 * MB) as f64;
        assert!(rewrites <= 3.0, "each byte rewritten {rewrites:.1} times");
    }

    #[test]
    fn plans_only_contiguous_runs_of_small_archives() {
        let policy = tiered(1000, 3, 1000);
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

    /// An archive of 10 records from `base` whose timestamps are seconds apart
    /// and out of offset order (as producers' own timestamps can be), written
    /// with `settings`, registered.
    async fn spread_archive(store: &Arc<dyn ObjectStore>, index: &Arc<SegmentIndex>, base: i64, settings: tantivy::IndexSettings) {
        let ts = |o: i64| 1_700_000_000_000 + ((o * 7919) % 50) * 1000 + o;
        let records = (base..base + 10)
            .map(|o| CanonicalRecordEntry {
                offset: o,
                timestamp: ts(o),
                key: None,
                value: Some(format!("{{\"n\":{o}}}").into_bytes()),
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
            base_timestamp: ts(base),
            max_timestamp: (base..base + 10).map(ts).max().unwrap(),
            records,
            compressed_records_wire_bytes: None,
            original_v1_wire_format: None,
            original_v2_wire_format: None,
        };
        let mut w = TantivySegmentWriter::with_settings("t".into(), 0, base, settings).unwrap();
        w.write_batch(&batch).unwrap();
        let out = tempfile::tempdir().unwrap();
        let (path, _) = w.commit_and_serialize(out.path()).unwrap();
        let data = std::fs::read(&path).unwrap();
        let key = format!("idx/t/partition-0/segment-{}-{}.tar", base, base + 9);
        let size = data.len() as u64;
        store.put(&key, bytes::Bytes::from(data)).await.unwrap();
        index
            .add_segment(SegmentMetadata { object_store_path: key, size_bytes: size, ..seg(base, base + 9, size) })
            .await
            .unwrap();
    }

    /// Whether every segment of the archive at `key` holds its records newest
    /// (highest offset) first, and the archive says so (its index settings).
    async fn newest_first(store: &Arc<dyn ObjectStore>, key: &str) -> (bool, bool) {
        let reader = TantivySegmentReader::from_archive_bytes(store.get(key).await.unwrap()).unwrap();
        let declared = reader.index().settings() == &crate::tantivy_segment::archive_settings();
        let searcher = reader.searcher();
        let ordered = searcher.segment_readers().iter().all(|s| {
            let off = s.fast_fields().i64("_offset").unwrap();
            let offsets: Vec<i64> = (0..s.max_doc()).map(|d| off.first(d).unwrap()).collect();
            offsets.windows(2).all(|w| w[0] > w[1])
        });
        (declared, ordered)
    }

    #[tokio::test]
    async fn archives_keep_their_records_newest_first_through_merges() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        for base in [0, 10, 20, 30] {
            spread_archive(&store, &index, base, crate::tantivy_segment::archive_settings()).await;
            let key = format!("idx/t/partition-0/segment-{}-{}.tar", base, base + 9);
            assert_eq!(newest_first(&store, &key).await, (true, true), "written archive {key}");
        }
        let outcome = merge_all(&store, &index, &policy(), &mut Default::default()).await;
        assert_eq!((outcome.merges, outcome.archives_merged), (1, 4));
        let segs = index.get_segments_for_partition("t", 0).await.unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(newest_first(&store, &segs[0].object_store_path).await, (true, true), "merged archive");
        let reader = TantivySegmentReader::from_archive_bytes(store.get(&segs[0].object_store_path).await.unwrap()).unwrap();
        let offsets: Vec<i64> = reader.query_by_offset_range(0, 40).unwrap().iter().map(|r| r.offset).collect();
        assert_eq!(offsets, (0..40).collect::<Vec<_>>(), "every record, read back in offset order");
    }

    /// Archives of earlier versions (offset order) and newest-first ones are
    /// merged stretch by stretch, never together.
    #[tokio::test]
    async fn offset_order_and_newest_first_archives_merge_separately() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        for base in [0, 10, 20, 30] {
            spread_archive(&store, &index, base, tantivy::IndexSettings::default()).await;
        }
        for base in [40, 50, 60, 70] {
            spread_archive(&store, &index, base, crate::tantivy_segment::archive_settings()).await;
        }
        let outcome = merge_all(&store, &index, &policy(), &mut Default::default()).await;
        assert_eq!((outcome.merges, outcome.archives_merged), (2, 8));
        let segs = index.get_segments_for_partition("t", 0).await.unwrap();
        let spans: Vec<(i64, i64)> = segs.iter().map(|s| (s.min_offset, s.max_offset)).collect();
        assert_eq!(spans, vec![(0, 39), (40, 79)]);
        // Also shows an archive written without the setting is oldest first,
        // so the order above is the sorting's doing.
        assert_eq!(newest_first(&store, &segs[0].object_store_path).await, (false, false), "older stretch stays as it was");
        assert_eq!(newest_first(&store, &segs[1].object_store_path).await, (true, true));
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
        MergePolicy { target_bytes: 64 * 1024 * 1024, min_run: 3, tier_base_bytes: 16 * 1024 * 1024, orphan_age: Duration::from_secs(3600), grace: Duration::ZERO }
    }

    #[tokio::test]
    async fn merges_a_run_into_one_archive_holding_every_record() {
        for gzip in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = local_store(dir.path()).await;
            let index = Arc::new(SegmentIndex::new());
            archives(&store, &index, 6, gzip).await;

            let outcome = merge_all(&store, &index, &policy(), &mut Default::default()).await;
            assert_eq!(outcome, MergeOutcome { merges: 1, archives_merged: 6, ..Default::default() }, "gzip={gzip}");
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

        assert_eq!(merge_run(&store, &index, &run, &policy()).await.unwrap(), RunOutcome::Raced);
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
        let outcome = merge_all(&store, &index, &policy(), &mut Default::default()).await;
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

    /// Thunderbird node 3: three archives in an older schema and one in the
    /// current one. No stretch is long enough, so the run is reported as
    /// ineligible (not as a failure or a race), and the next pass skips it
    /// without reopening it.
    #[tokio::test]
    async fn a_mixed_schema_run_too_short_to_split_is_ineligible_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        for base in [0, 10, 20] {
            old_schema_archive(&store, &index, base).await;
        }
        let current = tempfile::tempdir().unwrap();
        let current_store = local_store(current.path()).await;
        let scratch = Arc::new(SegmentIndex::new());
        archives(&current_store, &scratch, 4, false).await; // 0-39 in the current schema
        let src = scratch.get_segments_for_partition("t", 0).await.unwrap().pop().unwrap(); // 30-39
        store.put(&src.object_store_path, current_store.get(&src.object_store_path).await.unwrap()).await.unwrap();
        index.add_segment(src.clone()).await.unwrap();
        let p = MergePolicy { min_run: 4, ..policy() };

        let mut ineligible = std::collections::HashSet::new();
        let first = merge_all(&store, &index, &p, &mut ineligible).await;
        assert_eq!(first, MergeOutcome { ineligible: 1, ..Default::default() });
        assert_eq!(ineligible.len(), 1);
        assert_eq!(index.get_segments_for_partition("t", 0).await.unwrap().len(), 4, "untouched");

        // The second pass must not reopen it: delete a source file, and a pass
        // that tried would fail on it.
        store.delete(&src.object_store_path).await.unwrap();
        let second = merge_all(&store, &index, &p, &mut ineligible).await;
        assert_eq!(second, MergeOutcome::default());

        // Once the partition's archives change, the memory lets go of the run.
        index.remove_segment("t", 0, &src.segment_id).await.unwrap();
        merge_all(&store, &index, &p, &mut ineligible).await;
        assert!(ineligible.is_empty());
    }

    #[tokio::test]
    async fn the_sweep_removes_only_old_unreferenced_archive_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        archives(&store, &index, 2, false).await; // registered: 0-9, 10-19
        let referenced = "idx/t/partition-0/segment-0-9.tar";
        store.put(&term_filter_key(referenced), bytes::Bytes::from_static(b"f")).await.unwrap();
        // Superseded: an earlier archive of a range a registered one now holds
        // (thunderbird: segment-1003901-1041137 beside segment-1003901-1105876).
        let superseded = "idx/t/partition-0/segment-10-15.tar.gz";
        store.put(superseded, bytes::Bytes::from_static(b"x")).await.unwrap();
        store.put(&term_filter_key(superseded), bytes::Bytes::from_static(b"f")).await.unwrap();
        // Holds offsets no registered archive covers: may be their only copy.
        let uncovered = "idx/t/partition-0/segment-100-199.tar";
        store.put(uncovered, bytes::Bytes::from_static(b"x")).await.unwrap();
        store.put("idx/t/partition-0/notes.txt", bytes::Bytes::from_static(b"n")).await.unwrap();

        // Too young to be a leftover: kept (the indexer may be about to register it).
        assert_eq!(sweep_orphans(&store, &index, Duration::from_secs(3600)).await, 0);
        assert_eq!(sweep_orphans(&store, &index, Duration::ZERO).await, 2);
        assert!(store.get(superseded).await.is_err());
        assert!(store.get(&term_filter_key(superseded)).await.is_err());
        assert!(store.get(uncovered).await.is_ok(), "offsets nothing else covers: kept");
        assert!(store.get(referenced).await.is_ok(), "a referenced archive stays");
        assert!(store.get(&term_filter_key(referenced)).await.is_ok(), "and its filter");
        assert!(store.get("idx/t/partition-0/notes.txt").await.is_ok(), "anything else stays");
        // Absolute keys, as thunderbird's catalogue holds them, sweep the same.
        assert_eq!(file_range("segment-1003901-1041137.tar.gz"), Some((1003901, 1041137)));
        assert_eq!(file_range("segment-5-9-r1795507862393.tar"), Some((5, 9)));
    }

    /// Thunderbird's catalogue keys are absolute ("/data/tantivy_indexes/…")
    /// while listed keys are not: v2.18.0's sweep listed nothing there and
    /// left 4,000-12,000 superseded archives a node in place.
    #[tokio::test]
    async fn the_sweep_works_with_absolute_catalogue_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        store.put("idx/t/partition-0/segment-0-9.tar", bytes::Bytes::from_static(b"r")).await.unwrap();
        index
            .add_segment(SegmentMetadata { object_store_path: "/idx/t/partition-0/segment-0-9.tar".into(), ..seg(0, 9, 1) })
            .await
            .unwrap();
        store.put("idx/t/partition-0/segment-0-4.tar.gz", bytes::Bytes::from_static(b"old")).await.unwrap();
        assert_eq!(sweep_orphans(&store, &index, Duration::ZERO).await, 1);
        assert!(store.get("idx/t/partition-0/segment-0-4.tar.gz").await.is_err());
        assert!(store.get("/idx/t/partition-0/segment-0-9.tar").await.is_ok(), "the registered one stays");
    }

    /// Archives from before v2.18.0 (`.tar.gz`, no term filter) that no merge
    /// touches are rewritten as plain tars with filters; a plain tar without a
    /// filter gets one.
    #[tokio::test]
    async fn old_archives_are_brought_to_the_current_format() {
        let dir = tempfile::tempdir().unwrap();
        let store = local_store(dir.path()).await;
        let index = Arc::new(SegmentIndex::new());
        archives(&store, &index, 2, true).await; // 0-19 as two .tar.gz: too few to merge
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = local_store(dir2.path()).await;
        let plain = Arc::new(SegmentIndex::new());
        archives(&store2, &plain, 1, false).await; // a .tar without a filter
        let p = MergePolicy { min_run: 4, ..policy() };

        let (converted, _) = upgrade_archives(&store, &index, &p).await;
        assert_eq!(converted, 2);
        let segs = index.get_segments_for_partition("t", 0).await.unwrap();
        assert_eq!(segs.len(), 2);
        for s in &segs {
            assert!(s.object_store_path.ends_with(".tar"), "{}", s.object_store_path);
            let bytes = store.get(&s.object_store_path).await.unwrap();
            assert!(!crate::tar_directory::is_gzip(&bytes));
            let reader = TantivySegmentReader::from_archive_bytes(bytes).unwrap();
            assert_eq!(reader.query_by_offset_range(s.min_offset, s.max_offset + 1).unwrap().len(), 10);
            assert!(store.get(&term_filter_key(&s.object_store_path)).await.is_ok(), "with its filter");
        }
        for _ in 0..50 {
            if store.get("idx/t/partition-0/segment-0-9.tar.gz").await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(store.get("idx/t/partition-0/segment-0-9.tar.gz").await.is_err(), "old file deleted");

        let key = &plain.get_segments_for_partition("t", 0).await.unwrap()[0].object_store_path;
        assert!(store2.get(&term_filter_key(key)).await.is_err());
        assert_eq!(upgrade_archives(&store2, &plain, &p).await, (0, 1));
        assert!(TermFilter::from_bytes(&store2.get(&term_filter_key(key)).await.unwrap()).is_some());
        assert_eq!(upgrade_archives(&store2, &plain, &p).await, (0, 0), "nothing left to do");
    }

    #[test]
    fn the_merge_lands_beside_its_sources() {
        assert_eq!(merged_key("/data/ti/t/partition-2/segment-0-9.tar.gz", 0, 99), "/data/ti/t/partition-2/segment-0-99.tar");
    }
}
