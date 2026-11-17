//! Retire the indexes the removed RealtimeIndexer left in `{data_dir}/index`.
//!
//! Those indexes are frozen at the day that indexer stopped, searches read
//! them as an extra source, and they hold two kinds of records the archives do
//! not: records of topics or partitions that have since been deleted, emptied
//! or recreated — data that is gone everywhere else and must not be searchable
//! — and a few records the archiver of the time skipped (every archive of an
//! old topic starts at offset 1, missing offset 0).
//!
//! Once per boot, for each old index: the offsets no local archive covers are
//! read back from this node's own log (`FetchHandler::read_local_range`), and
//! whatever is still there is archived exactly as the WalIndexer archives the
//! WAL. Records the log no longer has are dropped with the old index. Only when
//! every re-archive succeeded is the old index deleted; otherwise it stays (and
//! stays searchable) and the next boot tries again.
//!
//! `CHRONIK_LEGACY_INDEX_RETIREMENT=false` leaves the old indexes alone.

use crate::fetch_handler::FetchHandler;
use chronik_search::legacy_index::{coverage, legacy_index_dirs, read_index};
use chronik_storage::canonical_record::CanonicalRecord;
use chronik_storage::WalIndexer;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// How long after boot the retirement runs: watermarks restored, the catalog
/// caught up, the indexer's first pass under way.
const START_DELAY_SECS: u64 = 120;
/// Bytes read from the log per step while re-archiving.
const READ_BYTES: i32 = 8 * 1024 * 1024;

pub fn enabled() -> bool {
    !matches!(
        std::env::var("CHRONIK_LEGACY_INDEX_RETIREMENT").ok().as_deref().map(str::trim),
        Some("false" | "0" | "off" | "no")
    )
}

/// Spawn the retirement, once, `START_DELAY_SECS` after boot
/// (`CHRONIK_LEGACY_INDEX_RETIREMENT_DELAY_SECS` overrides it).
pub fn spawn(data_dir: PathBuf, wal_indexer: Arc<WalIndexer>, fetch: Arc<FetchHandler>) {
    if !enabled() || legacy_index_dirs(&data_dir).is_empty() {
        return;
    }
    tokio::spawn(async move {
        let delay = std::env::var("CHRONIK_LEGACY_INDEX_RETIREMENT_DELAY_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(START_DELAY_SECS);
        tokio::time::sleep(Duration::from_secs(delay)).await;
        retire_all(&data_dir, &wal_indexer, &fetch).await;
    });
}

/// Retire every old index under `data_dir/index`. Returns how many were
/// deleted.
pub async fn retire_all(data_dir: &Path, wal_indexer: &WalIndexer, fetch: &FetchHandler) -> usize {
    let dirs = legacy_index_dirs(data_dir);
    if dirs.is_empty() {
        return 0;
    }
    info!(count = dirs.len(), "retiring indexes left by the removed RealtimeIndexer");
    let mut retired = 0;
    for (dir_name, dir) in dirs {
        let contents = match read_index(&dir_name, &dir) {
            Ok(c) => c,
            Err(e) => {
                warn!(index = %dir_name, "leaving {} in place: {}", dir.display(), e);
                continue;
            }
        };
        let topic = contents.topic.clone();
        let archives = wal_indexer.segment_index().get_segments_for_topic(&topic).await.unwrap_or_default();
        let report = coverage(&contents, &archives);
        let mut rearchived = 0u64;
        let mut keep = false;
        for p in &report.partitions {
            // Gaps below the highest archived offset are records the indexer
            // has passed and will never archive: re-archive them from the
            // log. Above it the indexer may simply not have got there yet;
            // archiving those here would make it archive them twice.
            let archived_up_to = archives
                .iter()
                .filter(|s| s.partition == p.partition)
                .map(|s| s.max_offset)
                .max()
                .unwrap_or(-1);
            for &(lo, hi) in &p.uncovered {
                if lo <= archived_up_to {
                    let gap_end = hi.min(archived_up_to);
                    match rearchive(&topic, p.partition, lo, gap_end, wal_indexer, fetch).await {
                        Ok(n) => rearchived += n,
                        Err(e) => {
                            warn!(topic = %topic, partition = p.partition, lo, hi = gap_end, "re-archiving failed: {}", e);
                            keep = true;
                        }
                    }
                }
                if hi > archived_up_to {
                    let from = lo.max(archived_up_to + 1);
                    match holds_any(&topic, p.partition, from, hi, fetch).await {
                        Ok(false) => {} // gone from the log: nothing to keep
                        Ok(true) => {
                            info!(
                                topic = %topic,
                                partition = p.partition,
                                from,
                                "old index holds records the indexer has not archived yet; keeping it for now"
                            );
                            keep = true;
                        }
                        Err(e) => {
                            warn!(topic = %topic, partition = p.partition, "could not read the log: {}", e);
                            keep = true;
                        }
                    }
                }
            }
        }
        if keep {
            warn!(topic = %topic, "keeping {} for now; retried next boot", dir.display());
            continue;
        }
        if let Err(e) = remove(data_dir, &topic, &dir).await {
            warn!(topic = %topic, "could not delete {}: {}", dir.display(), e);
            continue;
        }
        retired += 1;
        info!(
            topic = %topic,
            records = report.docs(),
            uncovered = report.uncovered_docs(),
            rearchived,
            dropped = report.uncovered_docs().saturating_sub(rearchived),
            mb = report.bytes / (1024 * 1024),
            "retired old RealtimeIndexer index"
        );
    }
    retired
}

/// Archive the records of `[lo, hi]` this node's log still holds; the rest
/// are gone (deleted topic, emptied or recreated partition, retention).
/// Returns how many records were archived.
async fn rearchive(
    topic: &str,
    partition: i32,
    lo: i64,
    hi: i64,
    wal_indexer: &WalIndexer,
    fetch: &FetchHandler,
) -> chronik_common::Result<u64> {
    let mut next = lo;
    let mut batches: Vec<CanonicalRecord> = Vec::new();
    let mut archived = 0u64;
    while next <= hi {
        let bytes = fetch.read_local_range(topic, partition, next, READ_BYTES).await?;
        let mut progressed = false;
        for raw in record_batches(&bytes) {
            let Ok(mut batch) = CanonicalRecord::from_kafka_batch(raw) else { continue };
            let batch_end = batch.last_offset();
            batch.records.retain(|r| r.offset >= next && r.offset <= hi);
            if let (Some(first), Some(last)) = (batch.records.first(), batch.records.last()) {
                let (first, last) = (first.offset, last.offset);
                // A trimmed batch: its own offsets now, and no wire bytes that
                // describe records it no longer holds.
                batch.base_offset = first;
                batch.compressed_records_wire_bytes = None;
                batch.original_v1_wire_format = None;
                batch.original_v2_wire_format = None;
                archived += batch.records.len() as u64;
                batches.push(batch);
                next = last + 1;
                progressed = true;
            } else if batch_end >= next {
                next = batch_end + 1;
                progressed = true;
            }
        }
        if !progressed {
            break; // nothing more in the log at or after `next`
        }
    }
    if !batches.is_empty() {
        wal_indexer.archive_batches(topic, partition, batches).await?;
    }
    Ok(archived)
}

/// Whether this node's log still holds any record in `[lo, hi]`.
async fn holds_any(
    topic: &str,
    partition: i32,
    lo: i64,
    hi: i64,
    fetch: &FetchHandler,
) -> chronik_common::Result<bool> {
    let bytes = fetch.read_local_range(topic, partition, lo, READ_BYTES).await?;
    Ok(record_batches(&bytes).into_iter().any(|raw| {
        let base = i64::from_be_bytes(raw[..8].try_into().expect("8 bytes"));
        base <= hi
    }))
}

/// The v2 record batches in a fetch's record bytes (`baseOffset` i64,
/// `batchLength` i32, then `batchLength` bytes; magic at byte 16). Older
/// message sets are skipped.
fn record_batches(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 17 <= bytes.len() {
        let len = i32::from_be_bytes(bytes[at + 8..at + 12].try_into().expect("4 bytes"));
        if len <= 0 {
            break;
        }
        let end = at + 12 + len as usize;
        if end > bytes.len() {
            break; // a partial batch at the end of a size-bounded read
        }
        if bytes[at + 16] == 2 {
            out.push(&bytes[at..end]);
        }
        at = end;
    }
    out
}

/// Move the old index out of `index/` first, so a crash mid-delete never
/// leaves a half-deleted index that searches would read.
async fn remove(data_dir: &Path, topic: &str, dir: &Path) -> std::io::Result<()> {
    let trash = data_dir.join("index").join(format!(".retired-{}-{}", topic, std::process::id()));
    tokio::fs::rename(dir, &trash).await?;
    tokio::fs::remove_dir_all(&trash).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(base: i64, n: i32) -> Vec<u8> {
        // baseOffset, batchLength, partitionLeaderEpoch, magic=2, then `n`
        // filler bytes (enough for the splitter, which reads only the header).
        let body_len = 4 + 1 + n as usize;
        let mut b = Vec::new();
        b.extend_from_slice(&base.to_be_bytes());
        b.extend_from_slice(&(body_len as i32).to_be_bytes());
        b.extend_from_slice(&0i32.to_be_bytes());
        b.push(2);
        b.extend(std::iter::repeat(0u8).take(n as usize));
        b
    }

    #[test]
    fn splits_batches_and_drops_a_trailing_partial_one() {
        let mut bytes = batch(0, 10);
        bytes.extend(batch(5, 20));
        let mut partial = batch(9, 30);
        partial.truncate(20);
        bytes.extend(partial);
        let got = record_batches(&bytes);
        assert_eq!(got.len(), 2);
        assert_eq!(i64::from_be_bytes(got[1][..8].try_into().unwrap()), 5);
        assert!(record_batches(&[]).is_empty());
    }

    #[tokio::test]
    async fn removal_leaves_no_index_behind() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("index").join("t");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), b"{}").unwrap();
        remove(root.path(), "t", &dir).await.unwrap();
        assert!(!dir.exists());
        assert!(legacy_index_dirs(root.path()).is_empty());
        assert_eq!(std::fs::read_dir(root.path().join("index")).unwrap().count(), 0);
    }
}
