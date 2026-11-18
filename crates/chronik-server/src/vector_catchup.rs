//! Embed what the seal-time pass missed.
//!
//! The WalIndexer embeds a vector-enabled topic's records once, when it seals
//! the WAL segment that holds them, and then deletes that segment. Two things
//! left records unembedded forever:
//!
//! - **A failed embed counted as done.** The provider erroring (thunderbird:
//!   OpenAI credit exhausted, reported as a rate limit) logged a warning, the
//!   segment was deleted all the same, and nothing tried again.
//! - **Nothing embeds records a node did not seal itself.** A replica that
//!   receives a partition by re-replication or assignment repair archives it
//!   and embeds it once, at that moment, with whatever the provider says then.
//!
//! On thunderbird that left 8 vector topics (up to 42,793 records each) with no
//! index on any node, while every signal looked healthy.
//!
//! This task closes the gap from the node's own log. Each pass, for every
//! vector-enabled topic, it compares the offsets this node has archived (its
//! Tantivy archives and its raw segments) with
//! the offsets its vector index holds, reads the missing ones back
//! (`FetchHandler::read_local_range`, whatever tier holds them) and embeds
//! them. Only offsets the indexer has already archived are considered: above
//! that, the seal-time pass has not run yet, and racing it would pay for the
//! same embedding twice.
//!
//! A record that yields no text (an empty value, a missing JSON field) never
//! enters the index, so it would look missing on every pass. Progress is
//! therefore kept per partition as the highest offset through which every gap
//! has been processed successfully; a gap is advanced past only when its embed
//! reported no failures. That memory is per process: after a restart each
//! partition's gaps are read once more, and only offsets still missing are
//! embedded (the index skips offsets it holds).
//!
//! Each pass embeds at most `CHRONIK_VECTOR_CATCHUP_MAX_RECORDS` records, so a
//! node that comes back with a large backlog spends on the provider gradually.
//!
//! Env: `CHRONIK_VECTOR_CATCHUP` (default on), `_DELAY_SECS` (180),
//! `_INTERVAL_SECS` (600), `_MAX_RECORDS` (20000).

use crate::fetch_handler::{record_batches, FetchHandler};
use chronik_common::metadata::traits::MetadataStore;
use chronik_storage::canonical_record::CanonicalRecord;
use chronik_storage::WalIndexer;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Bytes read from the log per step.
const READ_BYTES: i32 = 4 * 1024 * 1024;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

pub fn enabled() -> bool {
    !matches!(
        std::env::var("CHRONIK_VECTOR_CATCHUP").ok().as_deref().map(str::trim),
        Some("false" | "0" | "off" | "no")
    )
}

/// Per-process state carried from pass to pass.
#[derive(Debug, Default)]
pub struct Progress {
    /// (topic, partition) → every gap at or below this offset has been
    /// processed without a failure.
    pub done_through: HashMap<(String, i32), i64>,
    /// Topics holding a vector index that the catalog did not list on the
    /// previous pass. Absent twice in a row, the index is deleted: once could
    /// be a catalog that has not caught up after a restart.
    pub absent_once: std::collections::HashSet<String>,
}

/// What one pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassOutcome {
    /// Records read back from the log and handed to the embedder.
    pub records: usize,
    /// Vectors added.
    pub embedded: usize,
    /// Texts the provider failed to embed (left for the next pass).
    pub failed: usize,
    /// Partitions with a gap still open at the end of the pass.
    pub partitions_behind: usize,
}

pub fn spawn(
    metadata_store: Arc<dyn MetadataStore>,
    wal_indexer: Arc<WalIndexer>,
    fetch: Arc<FetchHandler>,
) {
    if !enabled() {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(env_u64("CHRONIK_VECTOR_CATCHUP_DELAY_SECS", 180))).await;
        let interval = Duration::from_secs(env_u64("CHRONIK_VECTOR_CATCHUP_INTERVAL_SECS", 600).max(1));
        let budget = env_u64("CHRONIK_VECTOR_CATCHUP_MAX_RECORDS", 20_000) as usize;
        let mut progress = Progress::default();
        loop {
            run_pass(metadata_store.as_ref(), &wal_indexer, &fetch, budget, &mut progress).await;
            tokio::time::sleep(interval).await;
        }
    });
}

/// The offset ranges in `archived` (inclusive, any order) that `embedded`
/// (ascending) does not hold, above `done_through`.
pub fn missing_ranges(archived: &[(i64, i64)], embedded: &[u64], done_through: i64) -> Vec<(i64, i64)> {
    let mut ranges: Vec<(i64, i64)> = archived.to_vec();
    ranges.sort_unstable();
    let mut out = Vec::new();
    for (lo, hi) in ranges {
        let lo = lo.max(done_through + 1);
        if lo > hi {
            continue;
        }
        let mut next = lo;
        let start = embedded.partition_point(|&o| (o as i64) < lo);
        for &o in &embedded[start..] {
            let o = o as i64;
            if o > hi {
                break;
            }
            if o > next {
                out.push((next, o - 1));
            }
            next = o + 1;
        }
        if next <= hi {
            out.push((next, hi));
        }
    }
    // Archives may overlap (a re-archived range); merge what they produced.
    out.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (lo, hi) in out {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

pub async fn run_pass(
    metadata_store: &dyn MetadataStore,
    wal_indexer: &WalIndexer,
    fetch: &FetchHandler,
    budget: usize,
    progress: &mut Progress,
) -> PassOutcome {
    let mut outcome = PassOutcome::default();
    let topics = match metadata_store.list_topics().await {
        Ok(t) => t,
        Err(e) => {
            debug!("vector catch-up: could not list topics: {}", e);
            return outcome;
        }
    };
    let manager = wal_indexer.vector_index_manager().clone();
    drop_indexes_of_deleted_topics(&manager, &topics, progress).await;
    debug!(
        topics = topics.len(),
        vector_topics = topics.iter().filter(|t| t.config.is_vector_enabled()).count(),
        "vector catch-up pass"
    );
    for topic in topics.into_iter().filter(|t| t.config.is_vector_enabled()) {
        for (partition, archived) in archived_ranges(metadata_store, wal_indexer, &topic.name).await {
            let key = (topic.name.clone(), partition);
            let done = progress.done_through.get(&key).copied().unwrap_or(-1);
            let embedded = manager.embedded_offsets(&topic.name, partition).await;
            let gaps = missing_ranges(&archived, &embedded, done);
            debug!(topic = %topic.name, partition, embedded = embedded.len(), done, gaps = ?gaps, "vector catch-up: coverage");
            if gaps.is_empty() {
                continue;
            }
            let mut done_through = done;
            let mut open = false;
            let embedded_before = outcome.embedded;
            for (lo, hi) in gaps {
                if outcome.records >= budget {
                    open = true;
                    break;
                }
                match catch_up_range(wal_indexer, fetch, &topic.name, partition, lo, hi, budget - outcome.records).await {
                    Ok(step) => {
                        outcome.records += step.records;
                        outcome.embedded += step.embedded;
                        outcome.failed += step.failed;
                        if step.failed > 0 || step.through < hi {
                            open = true;
                            break;
                        }
                        done_through = hi;
                    }
                    Err(e) => {
                        warn!(topic = %topic.name, partition, lo, hi, "vector catch-up could not read the log: {}", e);
                        open = true;
                        break;
                    }
                }
            }
            if done_through > done {
                progress.done_through.insert(key, done_through);
            }
            if open {
                outcome.partitions_behind += 1;
            }
            if outcome.embedded > embedded_before {
                if let Err(e) = manager.save_partition_index(&topic.name, partition).await {
                    debug!(topic = %topic.name, partition, "vector catch-up: index snapshot failed: {}", e);
                }
            }
        }
    }
    if outcome.records > 0 || outcome.partitions_behind > 0 {
        if outcome.failed > 0 {
            warn!(
                records = outcome.records,
                embedded = outcome.embedded,
                failed = outcome.failed,
                partitions_behind = outcome.partitions_behind,
                "vector catch-up pass: the embedding provider failed (see its error above); the records stay \
                 unembedded and are retried next pass"
            );
        } else {
            info!(
                records = outcome.records,
                embedded = outcome.embedded,
                partitions_behind = outcome.partitions_behind,
                "vector catch-up pass: embedded archived records the index was missing"
            );
        }
    }
    outcome
}

/// Vector indexes of topics the catalog no longer has. Before v2.18.2 a
/// topic delete left its index on disk; it was loaded at every boot and listed
/// by `/_vector/topics` (thunderbird: 7 such topics).
async fn drop_indexes_of_deleted_topics(
    manager: &chronik_columnar::VectorIndexManager,
    topics: &[chronik_common::metadata::traits::TopicMetadata],
    progress: &mut Progress,
) {
    let live: std::collections::HashSet<&str> = topics.iter().map(|t| t.name.as_str()).collect();
    let indexed = manager.list_topics().await;
    let absent: std::collections::HashSet<String> =
        indexed.into_iter().filter(|t| !live.contains(t.as_str())).collect();
    for topic in absent.intersection(&progress.absent_once) {
        match manager.clear_topic_with_snapshots(topic).await {
            Ok(n) => info!(topic = %topic, vectors = n, "removed the vector index of a deleted topic"),
            Err(e) => warn!(topic = %topic, "could not remove the vector index of a deleted topic: {}", e),
        }
    }
    progress.absent_once = absent;
}

/// The offset ranges this node has archived, per partition (ascending): its
/// Tantivy archives and its raw segments. A topic that is not searchable is
/// archived as raw segments only, recorded in the metadata store rather than
/// the Tantivy catalogue; both tell which offsets the indexer has passed.
pub(crate) async fn archived_ranges(
    metadata_store: &dyn MetadataStore,
    wal_indexer: &WalIndexer,
    topic: &str,
) -> Vec<(i32, Vec<(i64, i64)>)> {
    let mut by_partition: HashMap<i32, Vec<(i64, i64)>> = HashMap::new();
    for s in wal_indexer.segment_index().get_segments_for_topic(topic).await.unwrap_or_default() {
        by_partition.entry(s.partition).or_default().push((s.min_offset, s.max_offset));
    }
    for s in metadata_store.list_segments(topic, None).await.unwrap_or_default() {
        if s.end_offset >= s.start_offset {
            by_partition.entry(s.partition as i32).or_default().push((s.start_offset, s.end_offset));
        }
    }
    let mut partitions: Vec<_> = by_partition.into_iter().collect();
    partitions.sort_unstable_by_key(|(p, _)| *p);
    partitions
}

/// One read from the log for a catch-up.
pub(crate) struct LogRead {
    /// The batches read, trimmed to `[next, hi]`.
    pub batches: Vec<CanonicalRecord>,
    /// Records in `batches`.
    pub count: usize,
    /// Every offset up to here was read (`next - 1` when the log holds
    /// nothing at or after `next`: compacted or retained away).
    pub through: i64,
}

/// Read `[next, hi]` from the log, whatever tier holds it, up to about
/// `READ_BYTES` and at most `room` records (always at least one batch).
pub(crate) async fn read_log(
    fetch: &FetchHandler,
    topic: &str,
    partition: i32,
    next: i64,
    hi: i64,
    room: usize,
) -> chronik_common::Result<LogRead> {
    let bytes = fetch.read_local_range(topic, partition, next, READ_BYTES).await?;
    let raws = record_batches(&bytes);
    debug!(topic, partition, next, hi, bytes = bytes.len(), batches = raws.len(), "catch-up: read");
    let mut read = LogRead { batches: Vec::new(), count: 0, through: next - 1 };
    for raw in raws {
        let Ok(mut batch) = CanonicalRecord::from_kafka_batch(raw) else { continue };
        let batch_end = batch.last_offset();
        batch.records.retain(|r| r.offset >= next && r.offset <= hi);
        if read.count + batch.records.len() > room && read.count > 0 {
            break;
        }
        read.count += batch.records.len();
        read.through = read.through.max(batch_end.min(hi));
        if !batch.records.is_empty() {
            read.batches.push(batch);
        }
    }
    Ok(read)
}

struct Step {
    records: usize,
    embedded: usize,
    failed: usize,
    /// Every offset up to here was read and embedded (or has no text).
    through: i64,
}

/// Read `[lo, hi]` back from the log, at most `budget` records, and embed it.
async fn catch_up_range(
    wal_indexer: &WalIndexer,
    fetch: &FetchHandler,
    topic: &str,
    partition: i32,
    lo: i64,
    hi: i64,
    budget: usize,
) -> chronik_common::Result<Step> {
    let mut next = lo;
    let mut step = Step { records: 0, embedded: 0, failed: 0, through: lo - 1 };
    while next <= hi && step.records < budget {
        let LogRead { batches, count, through: read_through } =
            read_log(fetch, topic, partition, next, hi, budget - step.records).await?;
        if read_through < next {
            // The log holds nothing at or after `next` (compacted, retained
            // away): the rest of the gap has nothing to embed.
            step.through = hi;
            return Ok(step);
        }
        if !batches.is_empty() {
            let embedded = wal_indexer.embed_records(topic, partition, batches).await?;
            step.records += count;
            step.embedded += embedded.embedded;
            step.failed += embedded.failed;
            if embedded.failed > 0 {
                return Ok(step);
            }
        }
        step.through = read_through;
        next = read_through + 1;
    }
    Ok(step)
}

#[cfg(test)]
mod tests {
    use super::missing_ranges;

    #[test]
    fn gaps_are_the_archived_offsets_the_index_lacks() {
        // Archived 0-9 and 20-29; embedded 0-4 and 7, 20-29 complete.
        let embedded: Vec<u64> = (0..5).chain([7]).chain(20..30).collect();
        assert_eq!(missing_ranges(&[(20, 29), (0, 9)], &embedded, -1), vec![(5, 6), (8, 9)]);
    }

    #[test]
    fn nothing_embedded_means_every_archived_offset() {
        assert_eq!(missing_ranges(&[(0, 9), (10, 19)], &[], -1), vec![(0, 19)]);
    }

    #[test]
    fn progress_hides_gaps_already_processed() {
        // Offsets 3 and 15 have no text: once processed, not re-read.
        let embedded: Vec<u64> = (0..20).filter(|o| *o != 3 && *o != 15).collect();
        assert_eq!(missing_ranges(&[(0, 19)], &embedded, -1), vec![(3, 3), (15, 15)]);
        assert_eq!(missing_ranges(&[(0, 19)], &embedded, 9), vec![(15, 15)]);
        assert!(missing_ranges(&[(0, 19)], &embedded, 19).is_empty());
    }

    #[test]
    fn overlapping_archives_do_not_double_a_gap() {
        assert_eq!(missing_ranges(&[(0, 9), (5, 14)], &[0, 1], -1), vec![(2, 14)]);
    }

    #[test]
    fn a_fully_embedded_partition_has_no_gap() {
        let embedded: Vec<u64> = (100..200).collect();
        assert!(missing_ranges(&[(100, 199)], &embedded, -1).is_empty());
    }
}
