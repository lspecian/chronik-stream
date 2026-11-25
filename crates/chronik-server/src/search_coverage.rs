//! Coverage check: every acknowledged record of a searchable partition is
//! searchable, in the hot index or in an archive.
//!
//! The hot index may drop only records an archive covers, so the invariant is
//! that the offsets `[log start, high watermark)` of every searchable
//! partition lie in the union of the archives' offset ranges and the hot
//! index's. Every `CHRONIK_SEARCH_COVERAGE_INTERVAL_SECS` (default 60; 0 turns
//! it off) each node checks the partitions it leads (the ones it answers
//! searches for) and publishes `chronik_search_unsearchable_offsets` and
//! `chronik_search_unsearchable_partitions`, logging each partition with a gap.
//!
//! Only records at least one interval old count: a record just produced is
//! briefly in neither while the hot index ingests it. Ranges are compared,
//! not individual records, so offsets with no record (transaction markers,
//! compacted keys) inside a covered range count as covered.
//!
//! Also publishes the archive health gauges (`chronik_archives_damaged`,
//! `chronik_archives_repaired_total`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chronik_common::metadata::MetadataStore;
use chronik_monitoring::MetricsRecorder;
use chronik_storage::wal_indexer::WalIndexer;
use tracing::{debug, warn};

use crate::produce_handler::ProduceHandler;

/// Offsets of `[start, end)` outside every range of `ranges` (inclusive
/// `(lo, hi)` pairs, in any order, possibly overlapping).
pub fn uncovered(start: i64, end: i64, ranges: &mut [(i64, i64)]) -> i64 {
    if end <= start {
        return 0;
    }
    ranges.sort_unstable();
    let mut missing = 0;
    let mut next = start; // lowest offset not yet known covered
    for &(lo, hi) in ranges.iter() {
        if hi < next {
            continue;
        }
        if lo >= end {
            break;
        }
        if lo > next {
            missing += lo - next;
        }
        next = next.max(hi + 1);
        if next >= end {
            return missing;
        }
    }
    missing + (end - next).max(0)
}

pub struct Coverage {
    pub metadata_store: Arc<dyn MetadataStore>,
    pub wal_indexer: Arc<WalIndexer>,
    pub produce: Arc<ProduceHandler>,
    pub hot: Option<Arc<chronik_search::hot_text_index::HotTextIndex>>,
    pub node_id: i32,
}

impl Coverage {
    /// One check. `previous` holds each partition's high watermark at the
    /// last check: only offsets below it are counted (and it is updated).
    /// Returns (unsearchable offsets, partitions with any).
    pub async fn check(&self, previous: &mut HashMap<(String, i32), i64>) -> (u64, u64) {
        MetricsRecorder::set_archives_damaged(chronik_storage::archive_health::damaged_archives().len() as u64);
        let topics = match self.metadata_store.list_topics().await {
            Ok(t) => t,
            Err(e) => {
                debug!("coverage check: cannot list topics: {}", e);
                return (0, 0);
            }
        };
        let index = self.wal_indexer.segment_index();
        let mut seen = HashMap::new();
        let (mut offsets, mut partitions) = (0u64, 0u64);
        for topic in topics.iter().filter(|t| t.config.is_searchable() && !t.name.starts_with("__")) {
            let hot_on = self.hot.is_some() && topic.config.is_searchable_hot_enabled();
            for partition in 0..topic.config.partition_count as i32 {
                match self.metadata_store.get_partition_leader(&topic.name, partition as u32).await {
                    Ok(Some(leader)) if leader != self.node_id => continue,
                    _ => {}
                }
                let Ok(hwm) = self.produce.get_high_watermark(&topic.name, partition).await else { continue };
                let key = (topic.name.clone(), partition);
                seen.insert(key.clone(), hwm);
                let Some(&old) = previous.get(&key) else { continue };
                let start = self.produce.get_log_start_offset(&topic.name, partition).await;
                let end = old.min(hwm);
                let mut ranges: Vec<(i64, i64)> = index
                    .get_segments_for_partition(&topic.name, partition)
                    .await
                    .unwrap_or_default()
                    .iter()
                    .map(|s| (s.min_offset, s.max_offset))
                    .collect();
                if hot_on {
                    if let Some(range) = self.hot.as_ref().unwrap().offset_range(&topic.name, partition).await {
                        ranges.push(range);
                    }
                }
                let missing = uncovered(start, end, &mut ranges);
                if missing > 0 {
                    offsets += missing as u64;
                    partitions += 1;
                    warn!(
                        topic = %topic.name, partition, log_start = start, checked_below = end, missing,
                        "search coverage: acknowledged records neither the hot index nor an archive holds"
                    );
                }
            }
        }
        *previous = seen;
        MetricsRecorder::set_search_unsearchable(offsets, partitions);
        (offsets, partitions)
    }
}

pub fn spawn(coverage: Coverage) {
    let secs = std::env::var("CHRONIK_SEARCH_COVERAGE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(60);
    if secs == 0 {
        return;
    }
    let mut repaired_seen = 0;
    tokio::spawn(async move {
        let mut previous = HashMap::new();
        loop {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            let repaired = chronik_storage::archive_health::repaired_total();
            for _ in repaired_seen..repaired {
                MetricsRecorder::record_archive_repaired();
            }
            repaired_seen = repaired;
            coverage.check(&mut previous).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::uncovered;

    #[test]
    fn counts_offsets_outside_every_range() {
        assert_eq!(uncovered(0, 100, &mut []), 100);
        assert_eq!(uncovered(0, 100, &mut [(0, 99)]), 0);
        assert_eq!(uncovered(0, 100, &mut [(0, 49), (50, 99)]), 0);
        assert_eq!(uncovered(0, 100, &mut [(60, 99), (0, 39)]), 20, "a hole, ranges out of order");
        assert_eq!(uncovered(0, 100, &mut [(0, 80), (10, 20), (70, 120)]), 0, "overlaps and beyond the end");
        assert_eq!(uncovered(10, 100, &mut [(0, 49)]), 50, "below the log start does not count");
        assert_eq!(uncovered(0, 100, &mut [(0, 49)]), 50, "tail not covered");
        assert_eq!(uncovered(5, 5, &mut []), 0);
        assert_eq!(uncovered(0, 10, &mut [(20, 30)]), 10);
    }
}
