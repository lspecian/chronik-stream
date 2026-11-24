//! Topic retention (`retention.ms`, `retention.bytes`) over the archive.
//!
//! The WAL is reclaimed once a segment is archived, so the archive holds a
//! partition's history. Every `CHRONIK_RETENTION_CHECK_INTERVAL_SECS` (300,
//! Kafka's `log.retention.check.interval.ms`) each node, for each topic with a
//! limit set and `cleanup.policy` including `delete`, deletes whole archived
//! segments past it, oldest first ([`archive::retention_start`]): the
//! partition's log start moves up (fetch and ListOffsets answer from it, as
//! after a DeleteRecords), then the raw segments, Tantivy and Parquet files and
//! vectors below it go.
//!
//! Each pass also removes what the archive holds below every partition's log
//! start however it moved there: a DeleteRecords only reclaimed WAL, so the
//! records it deleted stayed in search, SQL and the vector index.
//!
//! `CHRONIK_RETENTION=false` turns it off.

use std::sync::Arc;
use std::time::Duration;

use chronik_common::metadata::MetadataStore;
use chronik_storage::archive;
use chronik_storage::wal_indexer::WalIndexer;
use tracing::{debug, info, warn};

use crate::produce_handler::ProduceHandler;

/// The in-memory indexes that may still hold records below a log start.
#[derive(Clone, Default)]
pub struct HotIndexes {
    pub text: Option<Arc<chronik_search::hot_text_index::HotTextIndex>>,
    pub vector: Option<Arc<chronik_columnar::hot_vector_index::HotVectorIndex>>,
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// What one pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassOutcome {
    /// Partitions whose log start retention moved.
    pub partitions_advanced: usize,
    /// Archive segments (raw, Tantivy, Parquet) removed.
    pub segments_removed: usize,
    /// Vectors removed.
    pub vectors_removed: usize,
}

/// The limits of a topic retention applies to, or `None`.
///
/// Internal topics (`__…`) and compact-only topics are left alone. `-1` (or
/// unset) means no limit, as in Kafka.
pub fn limits(
    name: &str,
    retention_ms: Option<i64>,
    config: &std::collections::HashMap<String, String>,
) -> Option<(Option<i64>, Option<i64>)> {
    if name.starts_with("__") {
        return None;
    }
    let policy = config.get("cleanup.policy").map(String::as_str).unwrap_or("delete");
    if !policy.split(',').any(|p| p.trim() == "delete") {
        return None;
    }
    let ms = retention_ms.filter(|ms| *ms >= 0);
    let bytes = config.get("retention.bytes").and_then(|v| v.trim().parse::<i64>().ok()).filter(|b| *b >= 0);
    (ms.is_some() || bytes.is_some()).then_some((ms, bytes))
}

pub fn spawn(metadata_store: Arc<dyn MetadataStore>, wal_indexer: Arc<WalIndexer>, produce: Arc<ProduceHandler>, hot: HotIndexes) {
    if matches!(
        std::env::var("CHRONIK_RETENTION").ok().as_deref().map(str::trim),
        Some("false" | "0" | "off" | "no")
    ) {
        info!("Retention disabled (CHRONIK_RETENTION=false)");
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(env_u64("CHRONIK_RETENTION_DELAY_SECS", 60))).await;
        let interval = Duration::from_secs(env_u64("CHRONIK_RETENTION_CHECK_INTERVAL_SECS", 300).max(1));
        loop {
            run_pass(metadata_store.as_ref(), &wal_indexer, &produce, &hot).await;
            tokio::time::sleep(interval).await;
        }
    });
}

pub async fn run_pass(
    metadata_store: &dyn MetadataStore,
    wal_indexer: &WalIndexer,
    produce: &ProduceHandler,
    hot: &HotIndexes,
) -> PassOutcome {
    let mut outcome = PassOutcome::default();
    let topics = match metadata_store.list_topics().await {
        Ok(t) => t,
        Err(e) => {
            warn!("retention: cannot list topics: {}", e);
            return outcome;
        }
    };
    let store = wal_indexer.object_store();
    let index = wal_indexer.segment_index();
    let vectors = wal_indexer.vector_index_manager();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    for topic in topics {
        let partitions = match archive::archived_partitions(store.as_ref(), Some(index), &topic.name).await {
            Ok(p) => p,
            Err(e) => {
                debug!(topic = %topic.name, "retention: cannot list archive: {}", e);
                continue;
            }
        };
        let limits = limits(&topic.name, topic.config.retention_ms, &topic.config.config);
        for partition in partitions {
            if let Some((ms, bytes)) = limits {
                let segments = match archive::list_archived_segments(store.as_ref(), &topic.name, partition).await {
                    Ok(s) => s,
                    Err(e) => {
                        debug!(topic = %topic.name, partition, "retention: cannot list segments: {}", e);
                        continue;
                    }
                };
                let ranges = archive::index_ranges(Some(index), &topic.name, partition).await;
                if let Some(start) = archive::retention_start(&segments, &ranges, ms, bytes, now_ms) {
                    let current = produce.get_log_start_offset(&topic.name, partition).await;
                    if start > current {
                        match produce.delete_records(&topic.name, partition, start).await {
                            Ok(lso) => {
                                outcome.partitions_advanced += 1;
                                info!(
                                    topic = %topic.name, partition, log_start = lso,
                                    retention_ms = ?ms, retention_bytes = ?bytes,
                                    "retention: deleted records below the new log start"
                                );
                            }
                            Err(code) => {
                                warn!(topic = %topic.name, partition, start, code, "retention: could not move the log start");
                                continue;
                            }
                        }
                    }
                }
            }
            // Whatever moved the log start (retention, DeleteRecords), what
            // lies wholly below it goes from the archive, the vector index and the
            // in-memory indexes.
            let log_start = produce.get_log_start_offset(&topic.name, partition).await;
            if log_start <= 0 {
                continue;
            }
            match archive::trim_archive_below(store.as_ref(), Some(index), &topic.name, partition, log_start).await {
                Ok(n) => outcome.segments_removed += n,
                Err(e) => warn!(topic = %topic.name, partition, log_start, "retention: archive trim failed: {}", e),
            }
            outcome.vectors_removed += vectors.drop_below(&topic.name, partition, log_start).await;
            if let Some(text) = &hot.text {
                let _ = text.evict_up_to(&topic.name, partition, log_start - 1).await;
            }
            if let Some(vector) = &hot.vector {
                let _ = vector.evict_up_to(&topic.name, partition, log_start - 1).await;
            }
        }
    }
    if outcome != PassOutcome::default() {
        info!(
            partitions = outcome.partitions_advanced,
            segments = outcome.segments_removed,
            vectors = outcome.vectors_removed,
            "retention pass"
        );
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn limits_follow_kafka_semantics() {
        let week = Some(604_800_000);
        assert_eq!(limits("orders", week, &cfg(&[])), Some((week, None)));
        assert_eq!(limits("orders", None, &cfg(&[])), None);
        assert_eq!(limits("orders", Some(-1), &cfg(&[("retention.bytes", "-1")])), None);
        assert_eq!(limits("orders", None, &cfg(&[("retention.bytes", "1000")])), Some((None, Some(1000))));
        assert_eq!(limits("orders", week, &cfg(&[("cleanup.policy", "compact")])), None);
        assert_eq!(limits("orders", week, &cfg(&[("cleanup.policy", "compact,delete")])), Some((week, None)));
        assert_eq!(limits("__consumer_offsets", week, &cfg(&[])), None);
    }
}
