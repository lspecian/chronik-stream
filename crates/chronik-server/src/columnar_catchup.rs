//! Write to Parquet what the seal-time pass missed.
//!
//! The WalIndexer writes a columnar topic's records to Parquet once, when it
//! seals the WAL segment that holds them. Until v2.18.2 its idempotency guard
//! skipped every offset at or below the highest one already in Parquet, and
//! sealed segments were taken in no particular order, so whenever a higher
//! segment of a partition went first, every lower one was written as an empty
//! file and its records never reached `{topic}_cold` (10M produced, 1.06M
//! queryable). The guard is fixed, but the WAL segments those records lived in
//! are gone; the records survive in this node's archives.
//!
//! Each pass, for every columnar topic, this compares the offsets this node
//! has archived with the ranges its Parquet segments cover, reads the missing
//! ones back from the log (`FetchHandler::read_local_range`) and writes them
//! (`WalIndexer::write_parquet_records`, which skips anything a concurrent
//! seal-time write covered first). The same pass also gives Parquet to records
//! archived before `columnar.enabled` was set on the topic.
//!
//! A range that yields no rows would look missing on every pass, so progress
//! is kept per partition (per process) as the offset through which every gap
//! has been written; after a restart each gap is read once more and only what
//! is still uncovered is written.
//!
//! Env: `CHRONIK_COLUMNAR_CATCHUP` (default on), `_DELAY_SECS` (120),
//! `_INTERVAL_SECS` (300), `_MAX_RECORDS` (1,000,000 per pass).

use crate::fetch_handler::FetchHandler;
use crate::vector_catchup::{archived_ranges, read_log, LogRead};
use chronik_common::metadata::traits::MetadataStore;
use chronik_storage::canonical_record::CanonicalRecord;
use chronik_storage::WalIndexer;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Records per Parquet file written by the catch-up (about one sealed WAL
/// segment's worth at the default rotation size).
const FILE_RECORDS: usize = 200_000;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

pub fn enabled() -> bool {
    !matches!(
        std::env::var("CHRONIK_COLUMNAR_CATCHUP").ok().as_deref().map(str::trim),
        Some("false" | "0" | "off" | "no")
    )
}

/// What one pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassOutcome {
    /// Records read back from the log and written to Parquet.
    pub records: usize,
    /// Parquet files written.
    pub files: usize,
    /// Partitions with a gap still open at the end of the pass.
    pub partitions_behind: usize,
}

/// `CHRONIK_PARQUET_DEDUP`: remove Parquet segments every offset of which
/// another segment also holds (`WalIndexer::dedup_parquet`). Default on;
/// `dry-run` only logs what it would remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dedup {
    Off,
    DryRun,
    On,
}

fn dedup_mode() -> Dedup {
    match std::env::var("CHRONIK_PARQUET_DEDUP").ok().as_deref().map(str::trim) {
        Some("false" | "0" | "off" | "no") => Dedup::Off,
        Some("dry-run" | "dry_run" | "dryrun") => Dedup::DryRun,
        _ => Dedup::On,
    }
}

pub fn spawn(metadata_store: Arc<dyn MetadataStore>, wal_indexer: Arc<WalIndexer>, fetch: Arc<FetchHandler>) {
    let catch_up = enabled();
    let dedup = dedup_mode();
    if !catch_up && dedup == Dedup::Off {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(env_u64("CHRONIK_COLUMNAR_CATCHUP_DELAY_SECS", 120))).await;
        let interval = Duration::from_secs(env_u64("CHRONIK_COLUMNAR_CATCHUP_INTERVAL_SECS", 300).max(1));
        let budget = env_u64("CHRONIK_COLUMNAR_CATCHUP_MAX_RECORDS", 1_000_000) as usize;
        let grace = Duration::from_secs(env_u64("CHRONIK_PARQUET_DEDUP_GRACE_SECS", 60));
        let mut done_through: HashMap<(String, i32), i64> = HashMap::new();
        let mut settled: HashMap<(String, i32), u64> = HashMap::new();
        loop {
            // Dedup first: the catch-up then measures coverage over what is
            // left, and never mistakes a copy about to go for the only one
            // (coverage is by offset, so it would not anyway).
            if dedup != Dedup::Off {
                match wal_indexer.dedup_parquet(dedup == Dedup::DryRun, grace, &mut settled).await {
                    Ok(o) if o.segments_removed > 0 => info!(
                        partitions = o.partitions_checked,
                        segments = o.segments_removed,
                        rows = o.rows_removed,
                        bytes = o.bytes_removed,
                        unreadable = o.unreadable,
                        dry_run = dedup == Dedup::DryRun,
                        "Parquet dedup pass: removed segments whose every offset another segment holds"
                    ),
                    Ok(o) => debug!(partitions = o.partitions_checked, unreadable = o.unreadable, "Parquet dedup pass: nothing redundant"),
                    Err(e) => warn!("Parquet dedup pass failed: {}", e),
                }
            }
            if catch_up {
                run_pass(metadata_store.as_ref(), &wal_indexer, &fetch, budget, &mut done_through).await;
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// The parts of `archived` (inclusive ranges, any order) that `covered`
/// (merged, ascending) does not hold, above `done_through`; merged, ascending.
pub fn uncovered(archived: &[(i64, i64)], covered: &[(i64, i64)], done_through: i64) -> Vec<(i64, i64)> {
    let mut archived: Vec<(i64, i64)> = archived
        .iter()
        .map(|&(lo, hi)| (lo.max(done_through.saturating_add(1)), hi))
        .filter(|(lo, hi)| lo <= hi)
        .collect();
    archived.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (lo, hi) in archived {
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    let mut gaps = Vec::new();
    for (lo, hi) in merged {
        let mut next = lo;
        for &(clo, chi) in covered {
            if chi < next || clo > hi {
                continue;
            }
            if clo > next {
                gaps.push((next, clo - 1));
            }
            next = next.max(chi.saturating_add(1));
            if next > hi {
                break;
            }
        }
        if next <= hi {
            gaps.push((next, hi));
        }
    }
    gaps
}

pub async fn run_pass(
    metadata_store: &dyn MetadataStore,
    wal_indexer: &WalIndexer,
    fetch: &FetchHandler,
    budget: usize,
    done_through: &mut HashMap<(String, i32), i64>,
) -> PassOutcome {
    let mut outcome = PassOutcome::default();
    let topics = match metadata_store.list_topics().await {
        Ok(t) => t,
        Err(e) => {
            debug!("columnar catch-up: could not list topics: {}", e);
            return outcome;
        }
    };
    let live: std::collections::HashSet<&str> = topics.iter().map(|t| t.name.as_str()).collect();
    done_through.retain(|(topic, _), _| live.contains(topic.as_str()));
    for topic in topics.iter().filter(|t| t.config.is_columnar_enabled()) {
        for (partition, archived) in archived_ranges(metadata_store, wal_indexer, &topic.name).await {
            let key = (topic.name.clone(), partition);
            let done = done_through.get(&key).copied().unwrap_or(-1);
            let covered = match wal_indexer.parquet_coverage(&topic.name, partition).await {
                Ok(c) => c,
                Err(e) => {
                    debug!(topic = %topic.name, partition, "columnar catch-up: coverage unavailable: {}", e);
                    continue;
                }
            };
            let gaps = uncovered(&archived, &covered, done);
            debug!(topic = %topic.name, partition, done, gaps = ?gaps, "columnar catch-up: coverage");
            if gaps.is_empty() {
                continue;
            }
            let mut through = done;
            let mut open = false;
            for (lo, hi) in gaps {
                if outcome.records >= budget {
                    open = true;
                    break;
                }
                match catch_up_range(wal_indexer, fetch, &topic.name, partition, lo, hi, budget - outcome.records)
                    .await
                {
                    Ok(step) => {
                        outcome.records += step.records;
                        outcome.files += step.files;
                        if step.through < hi {
                            open = true;
                            break;
                        }
                        through = hi;
                    }
                    Err(e) => {
                        warn!(topic = %topic.name, partition, lo, hi, "columnar catch-up could not complete a range: {}", e);
                        open = true;
                        break;
                    }
                }
            }
            if through > done {
                done_through.insert(key, through);
            }
            if open {
                outcome.partitions_behind += 1;
            }
        }
    }
    if outcome.records > 0 || outcome.partitions_behind > 0 {
        info!(
            records = outcome.records,
            files = outcome.files,
            partitions_behind = outcome.partitions_behind,
            "columnar catch-up pass: wrote archived records Parquet was missing"
        );
    }
    outcome
}

struct Step {
    records: usize,
    files: usize,
    /// Every offset up to here was read and written (or has no rows).
    through: i64,
}

/// Read `[lo, hi]` back from the log, at most `budget` records, and write it
/// to Parquet in files of about `FILE_RECORDS`.
async fn catch_up_range(
    wal_indexer: &WalIndexer,
    fetch: &FetchHandler,
    topic: &str,
    partition: i32,
    lo: i64,
    hi: i64,
    budget: usize,
) -> chronik_common::Result<Step> {
    let mut step = Step { records: 0, files: 0, through: lo - 1 };
    let mut next = lo;
    let mut pending: Vec<CanonicalRecord> = Vec::new();
    let mut pending_count = 0usize;
    let mut pending_through = lo - 1;
    loop {
        let finished = next > hi || step.records + pending_count >= budget;
        if pending_count > 0 && (finished || pending_count >= FILE_RECORDS) {
            let (bytes, _) = wal_indexer
                .write_parquet_records(topic, partition, std::mem::take(&mut pending))
                .await
                .map_err(|e| chronik_common::Error::Internal(format!("Parquet write failed: {}", e)))?;
            step.records += pending_count;
            if bytes > 0 {
                step.files += 1;
            }
            pending_count = 0;
            step.through = pending_through;
        }
        if finished {
            if next > hi {
                step.through = hi;
            }
            return Ok(step);
        }
        let room = (budget - step.records - pending_count).min(FILE_RECORDS - pending_count);
        let LogRead { batches, count, through } = read_log(fetch, topic, partition, next, hi, room).await?;
        if through < next {
            // The log holds nothing at or after `next` (compacted, retained
            // away): nothing more to write in this gap.
            next = hi + 1;
            if pending_count == 0 {
                step.through = hi;
                return Ok(step);
            }
            pending_through = hi;
            continue;
        }
        pending.extend(batches);
        pending_count += count;
        pending_through = through;
        if pending_count == 0 {
            step.through = through;
        }
        next = through + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::uncovered;

    #[test]
    fn gaps_are_archived_offsets_parquet_does_not_cover() {
        // The 10M case: only one high segment made it to Parquet.
        assert_eq!(uncovered(&[(0, 99), (100, 199), (200, 299)], &[(200, 249)], -1), vec![(0, 199), (250, 299)]);
    }

    #[test]
    fn fully_covered_has_no_gap() {
        assert!(uncovered(&[(0, 99)], &[(0, 49), (50, 99)], -1).is_empty());
        assert!(uncovered(&[(0, 99)], &[(0, 99)], -1).is_empty());
    }

    #[test]
    fn interior_holes_are_found() {
        assert_eq!(uncovered(&[(0, 99)], &[(0, 9), (20, 29), (90, 99)], -1), vec![(10, 19), (30, 89)]);
    }

    #[test]
    fn progress_hides_processed_gaps() {
        assert_eq!(uncovered(&[(0, 99)], &[(40, 49)], 59), vec![(60, 99)]);
        assert!(uncovered(&[(0, 99)], &[], 99).is_empty());
    }

    #[test]
    fn nothing_covered_means_every_archived_offset_merged() {
        assert_eq!(uncovered(&[(10, 19), (0, 9), (30, 39)], &[], -1), vec![(0, 19), (30, 39)]);
    }
}
