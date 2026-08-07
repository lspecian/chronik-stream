//! Regression test for issue #19: idle sealing must rotate the segment.
//!
//! `seal_stale_segments` hands a segment to the WalIndexer. Before the fix it
//! recorded the segment as sealed but kept writing into the same file, so the
//! "sealed" segment kept growing and every indexer run re-read it from offset 0
//! and published another, larger, overlapping Parquet segment — which made
//! `SELECT COUNT(*)` over-report by whole generations of duplicated rows.

use std::path::{Path, PathBuf};

use chronik_wal::config::WalConfig;
use chronik_wal::WalManager;

/// Base offset of a WAL record, whichever version it is.
fn base_offset_of(record: &chronik_wal::WalRecord) -> i64 {
    use chronik_wal::WalRecord;
    match record {
        WalRecord::V2 { base_offset, .. } => *base_offset,
        WalRecord::V1 { offset, .. } => *offset,
    }
}

fn segment_path(dir: &Path, topic: &str, partition: i32, segment_id: u64) -> PathBuf {
    dir.join(topic)
        .join(partition.to_string())
        .join(format!("wal_{}_{}.log", partition, segment_id))
}

/// Append one single-record V2 batch per offset. The payload is opaque here —
/// `read_from` returns batches without deserializing them.
async fn append(manager: &WalManager, topic: &str, partition: i32, offsets: std::ops::Range<i64>) {
    for offset in offsets {
        manager
            .append_canonical(
                topic.to_string(),
                partition,
                format!("value-{}", offset).into_bytes(),
                offset,
                offset,
                1,
            )
            .await
            .expect("append");
    }
}

#[tokio::test]
async fn idle_seal_rotates_so_sealed_segments_stop_growing() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = WalConfig::default();
    config.data_dir = dir.path().to_path_buf();

    let manager = WalManager::new(config).await.unwrap();
    let (topic, partition) = ("rotate-test", 0);

    append(&manager, topic, partition, 0..20).await;

    // Threshold 0: every segment with data counts as stale.
    let sealed = manager.seal_stale_segments(0).await;
    assert_eq!(sealed, 1, "the active segment should have been sealed");

    let segment0 = segment_path(dir.path(), topic, partition, 0);
    let size_at_seal = std::fs::metadata(&segment0).unwrap().len();
    assert!(size_at_seal > 0);

    let sealed_sizes = manager.get_sealed_segments_with_size();
    assert_eq!(sealed_sizes.len(), 1);
    assert_eq!(sealed_sizes[0].0, format!("{}:{}:0", topic, partition));

    // More traffic arrives after sealing.
    append(&manager, topic, partition, 20..40).await;

    assert_eq!(
        std::fs::metadata(&segment0).unwrap().len(),
        size_at_seal,
        "a sealed segment must be immutable — otherwise the indexer re-reads it \
         from offset 0 and republishes overlapping Parquet segments"
    );

    assert!(
        segment_path(dir.path(), topic, partition, 1).exists(),
        "writes after sealing must land in a new segment file"
    );

    // The reported size stays put, which is what lets the indexer skip a
    // segment it has already processed.
    let sealed_sizes = manager.get_sealed_segments_with_size();
    let segment0_entry = sealed_sizes
        .iter()
        .find(|(id, _)| id.ends_with(":0"))
        .expect("segment 0 still tracked");
    assert_eq!(segment0_entry.1, size_at_seal);
}

/// Sealing and committing contend for the same writer. If a commit can append
/// after the segment's size has been recorded, the indexer — which skips
/// segments it has already seen at that size — never comes back for those
/// records, so they silently never reach search / SQL / the cold tier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_commits_never_land_in_a_sealed_segment() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = WalConfig::default();
    config.data_dir = dir.path().to_path_buf();

    let manager = std::sync::Arc::new(WalManager::new(config).await.unwrap());
    let (topic, partition) = ("rotate-race", 0);

    append(&manager, topic, partition, 0..5).await;

    // Hammer the partition while sealing runs repeatedly.
    let writer = {
        let manager = manager.clone();
        tokio::spawn(async move {
            for offset in 5..400 {
                append(&manager, topic, partition, offset..offset + 1).await;
            }
        })
    };

    let sealer = {
        let manager = manager.clone();
        tokio::spawn(async move {
            for _ in 0..40 {
                manager.seal_stale_segments(0).await;
                tokio::task::yield_now().await;
            }
        })
    };

    writer.await.unwrap();
    sealer.await.unwrap();

    // Every segment reported as sealed must still be exactly the size that was
    // reported — anything larger means a commit appended after the handoff.
    for (segment_id, reported) in manager.get_sealed_segments_with_size() {
        let parts: Vec<&str> = segment_id.split(':').collect();
        let path = segment_path(dir.path(), parts[0], parts[1].parse().unwrap(), parts[2].parse().unwrap());
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            on_disk, reported,
            "segment {} grew after being sealed ({} on disk vs {} reported)",
            segment_id, on_disk, reported
        );
    }

    // And nothing was lost or duplicated along the way. Assert the offsets
    // themselves: a count alone would let a duplicate paper over a missing one.
    let records = manager
        .read_from(topic, partition, 0, usize::MAX)
        .await
        .expect("read across segments");
    let mut offsets: Vec<i64> = records.iter().map(base_offset_of).collect();
    offsets.sort_unstable();
    assert_eq!(
        offsets,
        (0..400).collect::<Vec<i64>>(),
        "rotation under load must preserve every offset exactly once"
    );
}

#[tokio::test]
async fn all_records_remain_readable_across_the_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = WalConfig::default();
    config.data_dir = dir.path().to_path_buf();

    let manager = WalManager::new(config).await.unwrap();
    let (topic, partition) = ("rotate-read", 0);

    append(&manager, topic, partition, 0..10).await;
    manager.seal_stale_segments(0).await;
    append(&manager, topic, partition, 10..20).await;
    manager.seal_stale_segments(0).await;
    append(&manager, topic, partition, 20..30).await;

    let records = manager
        .read_from(topic, partition, 0, usize::MAX)
        .await
        .expect("read across segments");

    let mut offsets: Vec<i64> = records.iter().map(base_offset_of).collect();
    offsets.sort_unstable();
    assert_eq!(
        offsets,
        (0..30).collect::<Vec<i64>>(),
        "rotation must not lose or duplicate records"
    );
}
