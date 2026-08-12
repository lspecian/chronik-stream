//! RP-3.3: discarding a divergent tail from a partition's WAL.
//!
//! The planner in `truncate.rs` is unit-tested against byte buffers. These
//! tests exercise the other half — real segment files, the real writer, the
//! real sealed-segment registry — because that is where truncation can leave
//! the partition in a state that reads fine right up until the next append or
//! the next restart.
//!
//! The invariant under test throughout: **after truncating to N, no offset at
//! or above N is readable, and the partition still works.**

use std::path::{Path, PathBuf};

use chronik_wal::config::WalConfig;
use chronik_wal::{WalManager, WalRecord};

fn segment_path(dir: &Path, topic: &str, partition: i32, segment_id: u64) -> PathBuf {
    dir.join(topic)
        .join(partition.to_string())
        .join(format!("wal_{}_{}.log", partition, segment_id))
}

/// One single-record batch per offset, so offsets and batches line up and a
/// truncation target always lands on a boundary. Batch-straddling targets are
/// covered separately.
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

/// Every offset currently readable from the partition, in order.
async fn readable_offsets(manager: &WalManager, topic: &str, partition: i32) -> Vec<i64> {
    manager
        .read_from(topic, partition, 0, usize::MAX)
        .await
        .expect("read")
        .iter()
        .flat_map(|record| match record {
            WalRecord::V2 {
                base_offset,
                last_offset,
                ..
            } => (*base_offset..=*last_offset).collect::<Vec<_>>(),
            WalRecord::V1 { offset, .. } => vec![*offset],
        })
        .collect()
}

fn config_at(dir: &Path) -> WalConfig {
    let mut config = WalConfig::default();
    config.data_dir = dir.to_path_buf();
    config
}

#[tokio::test]
async fn truncating_removes_the_tail_and_keeps_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc", 0);

    append(&manager, topic, partition, 0..20).await;
    assert_eq!(readable_offsets(&manager, topic, partition).await.len(), 20);

    let outcome = manager.truncate_to(topic, partition, 10).await.unwrap();

    assert_eq!(outcome.new_log_end_offset, Some(10));
    assert_eq!(
        readable_offsets(&manager, topic, partition).await,
        (0..10).collect::<Vec<_>>()
    );
}

/// A truncation reaching back past segment boundaries must delete the whole
/// segments above the cut, not just shorten the last one.
#[tokio::test]
async fn truncating_across_segments_deletes_the_ones_above_the_cut() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-multi", 0);

    // Three segments: [0,10), [10,20), [20,30).
    append(&manager, topic, partition, 0..10).await;
    manager.seal_stale_segments(0).await;
    append(&manager, topic, partition, 10..20).await;
    manager.seal_stale_segments(0).await;
    append(&manager, topic, partition, 20..30).await;

    assert!(segment_path(dir.path(), topic, partition, 1).exists());
    assert!(segment_path(dir.path(), topic, partition, 2).exists());

    let outcome = manager.truncate_to(topic, partition, 5).await.unwrap();

    assert_eq!(outcome.new_log_end_offset, Some(5));
    assert_eq!(outcome.segments_deleted, 2, "segments 1 and 2 hold only discarded offsets");
    assert!(!segment_path(dir.path(), topic, partition, 1).exists());
    assert!(!segment_path(dir.path(), topic, partition, 2).exists());
    assert_eq!(
        readable_offsets(&manager, topic, partition).await,
        (0..5).collect::<Vec<_>>()
    );
}

/// Cutting exactly at a segment boundary must not shorten the segment below it.
#[tokio::test]
async fn truncating_at_a_segment_boundary_keeps_the_segment_below_intact() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-boundary", 0);

    append(&manager, topic, partition, 0..10).await;
    manager.seal_stale_segments(0).await;
    let segment0_size = std::fs::metadata(segment_path(dir.path(), topic, partition, 0))
        .unwrap()
        .len();
    append(&manager, topic, partition, 10..20).await;

    let outcome = manager.truncate_to(topic, partition, 10).await.unwrap();

    assert_eq!(outcome.new_log_end_offset, Some(10));
    assert_eq!(
        std::fs::metadata(segment_path(dir.path(), topic, partition, 0))
            .unwrap()
            .len(),
        segment0_size,
        "the segment entirely below the cut must be byte-identical"
    );
    assert_eq!(
        readable_offsets(&manager, topic, partition).await,
        (0..10).collect::<Vec<_>>()
    );
}

/// The whole point of the primitive: the partition must still accept writes,
/// and the log that results must be contiguous.
#[tokio::test]
async fn the_partition_keeps_working_after_a_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-resume", 0);

    append(&manager, topic, partition, 0..20).await;
    let outcome = manager.truncate_to(topic, partition, 12).await.unwrap();
    let resume = outcome.new_log_end_offset.unwrap();

    // A follower resumes from where the log actually ends.
    append(&manager, topic, partition, resume..resume + 8).await;

    assert_eq!(
        readable_offsets(&manager, topic, partition).await,
        (0..20).collect::<Vec<_>>(),
        "re-fetched records must land contiguously on the truncated log"
    );
}

/// Truncation writes to disk, so it has to survive the process that did it.
#[tokio::test]
async fn a_truncation_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (topic, partition) = ("trunc-restart", 0);

    {
        let manager = WalManager::new(config_at(dir.path())).await.unwrap();
        append(&manager, topic, partition, 0..20).await;
        manager.truncate_to(topic, partition, 7).await.unwrap();
        manager.shutdown().await;
    }

    let recovered = WalManager::recover(&config_at(dir.path())).await.unwrap();
    assert_eq!(
        readable_offsets(&recovered, topic, partition).await,
        (0..7).collect::<Vec<_>>(),
        "the discarded tail must not come back on recovery"
    );
}

#[tokio::test]
async fn truncating_to_zero_empties_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-all", 0);

    append(&manager, topic, partition, 0..10).await;
    let outcome = manager.truncate_to(topic, partition, 0).await.unwrap();

    assert_eq!(outcome.new_log_end_offset, None, "nothing survived");
    assert!(readable_offsets(&manager, topic, partition).await.is_empty());

    // And the partition is still usable from scratch.
    append(&manager, topic, partition, 0..3).await;
    assert_eq!(
        readable_offsets(&manager, topic, partition).await,
        vec![0, 1, 2]
    );
}

/// A target at or above the end is the common case once a follower is caught
/// up. It must not cost a rotation, let alone a byte.
#[tokio::test]
async fn a_target_at_or_above_the_end_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-noop", 0);

    append(&manager, topic, partition, 0..10).await;
    let before = std::fs::metadata(segment_path(dir.path(), topic, partition, 0))
        .unwrap()
        .len();

    for target in [10i64, 11, 1_000_000] {
        let outcome = manager.truncate_to(topic, partition, target).await.unwrap();
        assert_eq!(outcome.new_log_end_offset, Some(10));
        assert_eq!(outcome.segments_deleted, 0);
        assert_eq!(outcome.bytes_discarded, 0);
    }

    assert_eq!(
        std::fs::metadata(segment_path(dir.path(), topic, partition, 0))
            .unwrap()
            .len(),
        before
    );
    assert!(
        !segment_path(dir.path(), topic, partition, 1).exists(),
        "a no-op truncation must not rotate"
    );
    assert_eq!(
        readable_offsets(&manager, topic, partition).await,
        (0..10).collect::<Vec<_>>()
    );
}

/// A target inside a batch takes the whole batch. The caller must be told the
/// truth about where the log ends, because it is *below* what was asked for.
#[tokio::test]
async fn a_target_inside_a_batch_reports_the_lower_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-straddle", 0);

    // Two 10-record batches: [0,9] and [10,19].
    manager
        .append_canonical(topic.to_string(), partition, b"batch-a".to_vec(), 0, 9, 10)
        .await
        .unwrap();
    manager
        .append_canonical(topic.to_string(), partition, b"batch-b".to_vec(), 10, 19, 10)
        .await
        .unwrap();

    // 15 sits inside the second batch.
    let outcome = manager.truncate_to(topic, partition, 15).await.unwrap();

    assert_eq!(
        outcome.new_log_end_offset,
        Some(10),
        "the straddling batch goes whole, so the log ends below the target"
    );
    assert_eq!(
        readable_offsets(&manager, topic, partition).await,
        (0..10).collect::<Vec<_>>()
    );
}

/// The registry drives the indexer. If it still advertises a deleted segment
/// the indexer reads a path that is gone; if it advertises a stale size it
/// reads past the new end of a shortened file.
#[tokio::test]
async fn the_sealed_segment_registry_reflects_the_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-registry", 0);

    append(&manager, topic, partition, 0..10).await;
    manager.seal_stale_segments(0).await;
    append(&manager, topic, partition, 10..20).await;
    manager.seal_stale_segments(0).await;

    assert_eq!(manager.get_sealed_segments_with_size().len(), 2);

    manager.truncate_to(topic, partition, 5).await.unwrap();

    let sealed = manager.get_sealed_segments_with_size();
    assert!(
        !sealed.iter().any(|(id, _)| id.ends_with(":1")),
        "a deleted segment must not stay advertised: {sealed:?}"
    );
    for (id, size) in &sealed {
        let seg_id: u64 = id.rsplit(':').next().unwrap().parse().unwrap();
        let path = segment_path(dir.path(), topic, partition, seg_id);
        assert!(path.exists(), "{id} is advertised but the file is gone");
        assert_eq!(
            *size,
            std::fs::metadata(&path).unwrap().len(),
            "{id} is advertised at a size the file does not have"
        );
    }
}

/// Segment ids must keep climbing across a truncation. Reusing the id of a
/// deleted segment would let the indexer's "seen this id at this size" memory
/// mistake fresh records for work it has already done.
#[tokio::test]
async fn segment_ids_are_never_reused_after_a_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-ids", 0);

    append(&manager, topic, partition, 0..10).await;
    manager.seal_stale_segments(0).await;
    append(&manager, topic, partition, 10..20).await;
    manager.seal_stale_segments(0).await;
    append(&manager, topic, partition, 20..30).await;

    // Deletes segments 1 and 2, cuts nothing — 0 keeps [0,10).
    manager.truncate_to(topic, partition, 3).await.unwrap();
    append(&manager, topic, partition, 3..6).await;

    let dir_entries: Vec<u64> = std::fs::read_dir(dir.path().join(topic).join(partition.to_string()))
        .unwrap()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix(&format!("wal_{}_", partition))?
                .strip_suffix(".log")?
                .parse()
                .ok()
        })
        .collect();

    assert!(
        dir_entries.iter().all(|id| *id == 0 || *id >= 3),
        "ids 1 and 2 were deleted and must not be handed out again: {dir_entries:?}"
    );
    assert_eq!(
        readable_offsets(&manager, topic, partition).await,
        (0..6).collect::<Vec<_>>()
    );
}

/// Truncating a partition this process has never written must not invent one.
#[tokio::test]
async fn truncating_an_unknown_partition_is_harmless() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();

    let outcome = manager.truncate_to("never-written", 7, 100).await.unwrap();

    assert_eq!(outcome.new_log_end_offset, None);
    assert_eq!(outcome.segments_deleted, 0);
    assert_eq!(outcome.bytes_discarded, 0);
}

/// Truncation is idempotent — a follower that retries after a crash mid-way
/// must converge on the same log, not shave more off each attempt.
#[tokio::test]
async fn truncating_twice_to_the_same_offset_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let manager = WalManager::new(config_at(dir.path())).await.unwrap();
    let (topic, partition) = ("trunc-idem", 0);

    append(&manager, topic, partition, 0..20).await;

    let first = manager.truncate_to(topic, partition, 8).await.unwrap();
    let after_first = readable_offsets(&manager, topic, partition).await;

    let second = manager.truncate_to(topic, partition, 8).await.unwrap();
    let after_second = readable_offsets(&manager, topic, partition).await;

    assert_eq!(first.new_log_end_offset, second.new_log_end_offset);
    assert_eq!(after_first, after_second);
    assert_eq!(second.bytes_discarded, 0, "the second pass has nothing to do");
    assert_eq!(second.segments_deleted, 0);
}
