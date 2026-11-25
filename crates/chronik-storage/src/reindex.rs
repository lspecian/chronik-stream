//! Rebuild a node's archived search indexes from its raw tier.
//!
//! Every archived record is also kept as raw batches (`segments/…`, see
//! `archive`). A reindex re-reads them and writes each archive anew as the
//! current version writes it: archives written before the JSON field had
//! columns answer neither range queries nor aggregations, and archives written
//! before v2.20.2 keep their records in offset order, which makes ranked
//! searches read every tie; a rebuilt one has both. Also a repair: an archive
//! is rebuilt whole from the records it covers.
//!
//! Safety, as for merges (`archive_merge`):
//! - An archive is rebuilt only when the raw tier holds every record it
//!   covers (its catalogue `record_count`); otherwise it is left alone, since
//!   it may be the only searchable copy of some (legacy re-archiving, raw
//!   segments removed by retention).
//! - The rebuilt archive gets a new file name and is swapped into the
//!   catalogue in one locked step, only if the old entry is still registered
//!   (a merge, truncation or topic delete that ran meanwhile wins, and the new
//!   file is deleted). The old file is deleted after a grace period, so a
//!   search that just looked it up still finds it; a crash in between leaves
//!   an unreferenced file the merge pass's orphan sweep removes.
//! - One archive at a time, on the caller's task.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tracing::{info, warn};

use crate::archive::{decode_raw_segment, list_archived_segments};
use crate::segment_index::SegmentMetadata;
use crate::wal_indexer::WalIndexer;
use chronik_common::{Error, Result};

/// What became of one archive.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Rebuilt and swapped in.
    Rebuilt,
    /// Already has the current schema (only when not forced).
    Current,
    /// The raw tier holds fewer of its records than it covers: left alone.
    Incomplete { found: usize, expected: usize },
    /// It left the catalogue while being rebuilt; nothing changed.
    Raced,
}

/// Progress of a reindex on this node.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Progress {
    pub running: bool,
    /// The topic, or all topics.
    pub topic: Option<String>,
    /// Rebuild archives that already have the current schema too.
    pub force: bool,
    pub archives: usize,
    pub checked: usize,
    pub rebuilt: usize,
    pub current: usize,
    pub incomplete: usize,
    pub raced: usize,
    pub failed: usize,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub last_error: Option<String>,
}

/// Whether an archive is written as current versions write it: its JSON
/// field has columns (ranges and aggregations need them), and it keeps its
/// records newest first (`tantivy_segment::archive_settings`; ranked searches
/// stop reading ties early, and merges keep the order).
pub fn is_current(index: &tantivy::Index) -> bool {
    let schema = index.schema();
    let json_columns = schema
        .get_field(crate::json_index::JSON_FIELD)
        .map(|f| schema.get_field_entry(f).field_type().is_fast())
        .unwrap_or(false);
    json_columns && index.settings() == &crate::tantivy_segment::archive_settings()
}

/// Rebuild one archive from the raw tier (see the module docs).
pub async fn rebuild(indexer: &WalIndexer, entry: &SegmentMetadata, force: bool, grace: Duration) -> Result<Outcome> {
    let store = indexer.object_store();
    if !force {
        let scratch = tempfile::tempdir().map_err(|e| Error::Internal(format!("reindex temp dir: {e}")))?;
        let reader = crate::archive_merge::open_source(&store, &entry.object_store_path, scratch.path()).await?;
        if is_current(reader.index()) {
            return Ok(Outcome::Current);
        }
    }

    // The records the archive covers, once each, from the raw segments.
    let (lo, hi) = (entry.min_offset, entry.max_offset);
    let mut seen: HashSet<i64> = HashSet::new();
    let mut batches = Vec::new();
    let raw = list_archived_segments(store.as_ref(), &entry.topic, entry.partition)
        .await
        .map_err(|e| Error::Internal(format!("listing raw segments: {e}")))?;
    for segment in raw.into_iter().filter(|s| s.last_offset >= lo && s.base_offset <= hi) {
        let data = store
            .get(&segment.object_key)
            .await
            .map_err(|e| Error::Internal(format!("reading {}: {e}", segment.object_key)))?;
        for mut batch in decode_raw_segment(&data)? {
            batch.records.retain(|r| r.offset >= lo && r.offset <= hi && seen.insert(r.offset));
            if let Some(first) = batch.records.iter().map(|r| r.offset).min() {
                batch.records.sort_by_key(|r| r.offset);
                batch.base_offset = first;
                batches.push(batch);
            }
        }
    }
    if seen.len() < entry.record_count {
        return Ok(Outcome::Incomplete { found: seen.len(), expected: entry.record_count });
    }
    batches.sort_by_key(|b| b.base_offset);

    let suffix = format!("r{}", chrono::Utc::now().timestamp_millis());
    let rebuilt = indexer.build_unregistered_archive(&entry.topic, entry.partition, batches, &suffix).await?;
    let new_path = rebuilt.object_store_path.clone();
    if !indexer.segment_index().replace_segments(&[entry.segment_id.clone()], rebuilt).await? {
        let _ = store.delete(&new_path).await;
        let _ = store.delete(&crate::term_filter::term_filter_key(&new_path)).await;
        return Ok(Outcome::Raced);
    }
    let old_path = entry.object_store_path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        let _ = store.delete(&old_path).await;
        let _ = store.delete(&crate::term_filter::term_filter_key(&old_path)).await;
    });
    Ok(Outcome::Rebuilt)
}

/// Whether the catalogue still lists this exact archive (same file).
async fn still_registered(indexer: &WalIndexer, entry: &SegmentMetadata) -> bool {
    indexer
        .segment_index()
        .get_segments_for_partition(&entry.topic, entry.partition)
        .await
        .map(|segments| segments.iter().any(|s| s.object_store_path == entry.object_store_path))
        .unwrap_or(true)
}

/// Rebuild every archive of `topic` (all topics when `None`), updating
/// `progress` as it goes. Archives already current are skipped unless
/// `force`.
pub async fn run(
    indexer: Arc<WalIndexer>,
    topic: Option<String>,
    force: bool,
    grace: Duration,
    progress: Arc<std::sync::Mutex<Progress>>,
) {
    let mut archives = match &topic {
        Some(t) => indexer.segment_index().get_segments_for_topic(t).await.unwrap_or_default(),
        None => indexer.segment_index().get_all_segments().await.unwrap_or_default(),
    };
    archives.sort_by(|a, b| (&a.topic, a.partition, a.min_offset).cmp(&(&b.topic, b.partition, b.min_offset)));
    {
        let mut p = progress.lock().unwrap();
        *p = Progress {
            running: true,
            topic: topic.clone(),
            force,
            archives: archives.len(),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            ..Default::default()
        };
    }
    info!(topic = ?topic, archives = archives.len(), force, "reindex started");
    for entry in &archives {
        // The list was taken at the start; a merge (or a repair) may have
        // replaced this archive since, and deleted its file after the grace
        // period. That is a race, not a failure: its replacement is checked
        // on the next pass.
        let outcome = if !still_registered(&indexer, entry).await {
            Ok(Outcome::Raced)
        } else {
            match rebuild(&indexer, entry, force, grace).await {
                Err(_) if !still_registered(&indexer, entry).await => Ok(Outcome::Raced),
                other => other,
            }
        };
        let mut p = progress.lock().unwrap();
        p.checked += 1;
        match outcome {
            Ok(Outcome::Rebuilt) => p.rebuilt += 1,
            Ok(Outcome::Current) => p.current += 1,
            Ok(Outcome::Raced) => p.raced += 1,
            Ok(Outcome::Incomplete { found, expected }) => {
                p.incomplete += 1;
                warn!(
                    topic = %entry.topic, partition = entry.partition, min_offset = entry.min_offset,
                    max_offset = entry.max_offset, found, expected,
                    "reindex: raw tier does not hold every record of this archive; left as it is"
                );
            }
            Err(e) => {
                p.failed += 1;
                warn!(topic = %entry.topic, partition = entry.partition, min_offset = entry.min_offset, "reindex failed: {}", e);
                p.last_error = Some(format!("{}-{} {}: {}", entry.topic, entry.partition, entry.min_offset, e));
            }
        }
    }
    let mut p = progress.lock().unwrap();
    p.running = false;
    p.finished_at_ms = Some(chrono::Utc::now().timestamp_millis());
    info!(
        topic = ?topic, rebuilt = p.rebuilt, current = p.current, incomplete = p.incomplete,
        raced = p.raced, failed = p.failed, "reindex finished"
    );
}
