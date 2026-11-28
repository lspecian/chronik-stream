//! What this node's archive holds for a partition, and how to cut it.
//!
//! The WAL indexer moves sealed WAL into the object store as raw segments keyed
//! `segments/<topic>/<partition>/<min>-<max>.segment`, and — for searchable and
//! columnar topics — into Tantivy and Parquet segments tracked by the
//! [`SegmentIndex`]. Once the WAL copy is reclaimed, the archive is the only
//! local record of those offsets, so anything that asks "where does this log
//! end?" or "discard everything above X" must ask the archive too.
//!
//! The key is the index: listing the store answers for every partition this
//! node holds. The metadata catalog is written only on the metadata leader, so
//! on any other node it does not know the archive exists.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::object_store::storage::ObjectStore;
use crate::segment_index::SegmentIndex;

/// Leads a zstd-compressed raw segment. Read as the bincode length prefix of an
/// uncompressed segment it would announce ~10^18 batches, so the two formats
/// cannot be confused.
const RAW_SEGMENT_ZSTD_MAGIC: &[u8; 8] = b"CHRNKZ1\n";

/// zstd level for raw segments: written once per sealed WAL segment in the
/// background indexer, read on every archive fetch.
const RAW_SEGMENT_ZSTD_LEVEL: i32 = 3;

/// A raw segment's bytes: the WAL's batches (bincode `Vec<CanonicalRecord>`,
/// original wire bytes included), zstd-compressed behind
/// [`RAW_SEGMENT_ZSTD_MAGIC`]. Up to v2.19.2 they were stored uncompressed,
/// most of a node's archive footprint; [`decode_raw_segment`] reads both.
pub fn encode_raw_segment(records: &[crate::canonical_record::CanonicalRecord]) -> chronik_common::Result<Vec<u8>> {
    let plain = bincode::serialize(records)
        .map_err(|e| chronik_common::Error::Internal(format!("serialize raw segment: {e}")))?;
    let mut out = Vec::with_capacity(plain.len() / 3 + RAW_SEGMENT_ZSTD_MAGIC.len());
    out.extend_from_slice(RAW_SEGMENT_ZSTD_MAGIC);
    zstd::stream::copy_encode(plain.as_slice(), &mut out, RAW_SEGMENT_ZSTD_LEVEL)
        .map_err(|e| chronik_common::Error::Internal(format!("compress raw segment: {e}")))?;
    Ok(out)
}

/// The batches of a raw segment, compressed or (written before compression)
/// plain.
pub fn decode_raw_segment(data: &[u8]) -> chronik_common::Result<Vec<crate::canonical_record::CanonicalRecord>> {
    let decoded;
    let plain = match data.strip_prefix(RAW_SEGMENT_ZSTD_MAGIC.as_slice()) {
        Some(compressed) => {
            decoded = zstd::stream::decode_all(compressed)
                .map_err(|e| chronik_common::Error::Internal(format!("decompress raw segment: {e}")))?;
            decoded.as_slice()
        }
        None => data,
    };
    bincode::deserialize(plain).map_err(|e| chronik_common::Error::Internal(format!("deserialize raw segment: {e}")))
}

/// One raw segment in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedSegment {
    pub base_offset: i64,
    pub last_offset: i64,
    pub object_key: String,
    /// Stored size in bytes.
    pub size_bytes: u64,
    /// When it was written (ms since the epoch): when its records were
    /// archived, so never earlier than their own timestamps.
    pub last_modified_ms: u64,
}

/// Prefix under which a partition's raw segments live.
pub fn archived_segment_prefix(topic: &str, partition: i32) -> String {
    format!("segments/{}/{}/", topic, partition)
}

/// Key of the raw segment holding `[min_offset, max_offset]`.
pub fn archived_segment_key(topic: &str, partition: i32, min_offset: i64, max_offset: i64) -> String {
    format!("segments/{}/{}/{}-{}.segment", topic, partition, min_offset, max_offset)
}

/// `(base, last)` from a raw segment key, or `None` for anything else under the prefix.
pub fn parse_archived_segment_key(key: &str) -> Option<(i64, i64)> {
    let name = key.rsplit('/').next()?.strip_suffix(".segment")?;
    let (lo, hi) = name.split_once('-')?;
    let (base, last) = (lo.parse::<i64>().ok()?, hi.parse::<i64>().ok()?);
    (base >= 0 && last >= base).then_some((base, last))
}

/// Every raw segment this node's store holds for the partition, in base-offset order.
pub async fn list_archived_segments(
    store: &dyn ObjectStore,
    topic: &str,
    partition: i32,
) -> crate::object_store::errors::ObjectStoreResult<Vec<ArchivedSegment>> {
    let mut segments: Vec<ArchivedSegment> = store
        .list(&archived_segment_prefix(topic, partition))
        .await?
        .into_iter()
        .filter_map(|o| {
            let (base_offset, last_offset) = parse_archived_segment_key(&o.key)?;
            Some(ArchivedSegment { base_offset, last_offset, object_key: o.key, size_bytes: o.size, last_modified_ms: o.last_modified })
        })
        .collect();
    segments.sort_by_key(|s| (s.base_offset, s.last_offset));
    Ok(segments)
}

/// One past the highest offset the archive holds for the partition; 0 when it
/// holds nothing or cannot be listed.
///
/// A store that cannot be listed answers 0 rather than an error because every
/// caller takes the maximum of this and what it already knows — an unknown
/// archive must not lower anything.
pub async fn archived_high_watermark(store: &dyn ObjectStore, topic: &str, partition: i32) -> i64 {
    match list_archived_segments(store, topic, partition).await {
        Ok(segments) => segments.iter().map(|s| s.last_offset + 1).max().unwrap_or(0),
        Err(e) => {
            debug!("archive: cannot list {}-{}: {}", topic, partition, e);
            0
        }
    }
}

/// The result of [`truncate_archive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveCut {
    /// Where the log is actually cut. At most the requested offset; lower when
    /// an archived segment straddled it, because a segment is removed whole.
    pub cut: i64,
    /// One past the highest offset still archived below the cut, if any is.
    pub archived_end: Option<i64>,
    /// Raw, Tantivy and Parquet segments removed.
    pub removed: usize,
}

/// Where a cut at `target` actually lands, given archived ranges `[base, last]`.
///
/// A segment that straddles the cut holds records on both sides and is removed
/// whole, so the cut moves down to its base — and that new cut may straddle
/// another (overlapping) segment, so repeat until nothing straddles. The
/// records between the final cut and the target are re-fetched from the leader.
pub fn settle_cut(target: i64, ranges: &[(i64, i64)]) -> i64 {
    let mut cut = target.max(0);
    loop {
        let lowered = ranges
            .iter()
            .filter(|(base, last)| *base < cut && *last >= cut)
            .map(|(base, _)| *base)
            .min();
        match lowered {
            Some(base) => cut = base,
            None => return cut,
        }
    }
}

/// Where a cut at `target` would land, without removing anything.
pub async fn plan_archive_cut(
    store: &dyn ObjectStore,
    segment_index: Option<&Arc<SegmentIndex>>,
    topic: &str,
    partition: i32,
    target: i64,
) -> crate::object_store::errors::ObjectStoreResult<i64> {
    let ranges = archived_ranges(store, segment_index, topic, partition).await?;
    Ok(settle_cut(target, &ranges))
}

/// Remove everything this node archived at or above `cut` for the partition.
///
/// `cut` must already be settled (see [`plan_archive_cut`]); any segment still
/// straddling it is lowered here too, so the returned [`ArchiveCut::cut`] is
/// authoritative. A follower that truncates a divergent log has to do this:
/// otherwise its archive keeps the records it just discarded, the fetch tier
/// serves them, and a restart reads its log end out of them.
pub async fn truncate_archive(
    store: &dyn ObjectStore,
    segment_index: Option<&Arc<SegmentIndex>>,
    topic: &str,
    partition: i32,
    cut: i64,
) -> crate::object_store::errors::ObjectStoreResult<ArchiveCut> {
    let ranges = archived_ranges(store, segment_index, topic, partition).await?;
    let cut = settle_cut(cut, &ranges);
    let mut removed = 0usize;

    for segment in list_archived_segments(store, topic, partition).await? {
        if segment.last_offset < cut {
            continue;
        }
        match store.delete(&segment.object_key).await {
            Ok(()) => removed += 1,
            Err(e) => {
                // A segment left behind keeps the discarded records servable.
                // Fail the cut: the caller refuses to resume replication on a
                // log it could not repair.
                warn!(
                    "archive: could not remove {} while cutting {}-{} at {}: {}",
                    segment.object_key, topic, partition, cut, e
                );
                return Err(e);
            }
        }
    }

    if let Some(index) = segment_index {
        removed += remove_indexed_from(store, index, topic, partition, cut).await;
    }

    let archived_end = list_archived_segments(store, topic, partition)
        .await?
        .iter()
        .map(|s| s.last_offset + 1)
        .max();

    if removed > 0 {
        info!(
            "archive: cut {}-{} at {} ({} segment(s) removed, archive now ends at {:?})",
            topic, partition, cut, removed, archived_end
        );
    }
    Ok(ArchiveCut { cut, archived_end, removed })
}

/// Where retention moves a partition's log start: one past the last of the
/// oldest raw segments it deletes, or `None` when it deletes none.
///
/// As in Kafka, whole segments go, oldest first, and the first one kept stops
/// the scan: a segment goes when it is older than `retention_ms` (by when it
/// was written: never earlier than its records' own timestamps), or when the
/// partition's archive would still hold at least `retention_bytes` without
/// it. The newest segment always stays: it carries the partition's end, which
/// a restart reads back once the WAL is reclaimed. The start then moves down
/// to the start of any index archive (a merged Tantivy or Parquet file) that
/// straddles it, so fetch, search and SQL agree on what is gone.
pub fn retention_start(
    segments: &[ArchivedSegment],
    index_ranges: &[(i64, i64)],
    retention_ms: Option<i64>,
    retention_bytes: Option<i64>,
    now_ms: u64,
) -> Option<i64> {
    let retention_ms = retention_ms.filter(|ms| *ms >= 0);
    let retention_bytes = retention_bytes.filter(|b| *b >= 0);
    if retention_ms.is_none() && retention_bytes.is_none() {
        return None;
    }
    let mut ordered: Vec<&ArchivedSegment> = segments.iter().collect();
    ordered.sort_by_key(|s| (s.base_offset, s.last_offset));
    let newest = ordered.iter().map(|s| s.last_offset).max()?;
    let mut total: u64 = ordered.iter().map(|s| s.size_bytes).sum();
    let mut start: Option<i64> = None;
    for s in ordered {
        if s.last_offset == newest {
            break;
        }
        let expired = retention_ms.is_some_and(|ms| s.last_modified_ms.saturating_add(ms as u64) < now_ms);
        let oversize = retention_bytes.is_some_and(|b| total.saturating_sub(s.size_bytes) >= b as u64);
        if !(expired || oversize) {
            break;
        }
        total = total.saturating_sub(s.size_bytes);
        start = Some(start.unwrap_or(0).max(s.last_offset + 1));
    }
    let start = start?;
    let mut ranges: Vec<(i64, i64)> = segments.iter().map(|s| (s.base_offset, s.last_offset)).collect();
    ranges.extend_from_slice(index_ranges);
    let settled = settle_cut(start, &ranges);
    (settled > 0).then_some(settled)
}

/// Remove what the archive holds entirely below `start` (the partition's log
/// start): raw segments, Tantivy and Parquet entries with their files and term
/// filters. Anything that reaches `start` or beyond stays. Returns how many
/// were removed.
pub async fn trim_archive_below(
    store: &dyn ObjectStore,
    segment_index: Option<&Arc<SegmentIndex>>,
    topic: &str,
    partition: i32,
    start: i64,
) -> crate::object_store::errors::ObjectStoreResult<usize> {
    let mut removed = 0usize;
    for segment in list_archived_segments(store, topic, partition).await? {
        if segment.last_offset >= start {
            continue;
        }
        store.delete(&segment.object_key).await?;
        removed += 1;
    }
    if let Some(index) = segment_index {
        let mut paths: HashSet<String> = HashSet::new();
        if let Ok(segments) = index.get_segments_for_partition(topic, partition).await {
            for s in segments.into_iter().filter(|s| s.max_offset < start) {
                if matches!(index.remove_segment(topic, partition, &s.segment_id).await, Ok(true)) {
                    removed += 1;
                    paths.insert(s.object_store_path);
                }
            }
        }
        if let Ok(segments) = index.get_parquet_segments_for_partition(topic, partition).await {
            for s in segments.into_iter().filter(|s| s.max_offset < start) {
                if matches!(index.remove_parquet_segment(topic, partition, &s.segment_id).await, Ok(true)) {
                    removed += 1;
                    paths.insert(s.object_store_path);
                }
            }
        }
        remove_files(store, paths).await;
    }
    if removed > 0 {
        info!("archive: {}-{} trimmed below {} ({} segment(s) removed)", topic, partition, start, removed);
    }
    Ok(removed)
}

/// Index ranges `(min, max)` the catalogue holds for a partition (Tantivy and
/// Parquet), for [`retention_start`].
pub async fn index_ranges(segment_index: Option<&Arc<SegmentIndex>>, topic: &str, partition: i32) -> Vec<(i64, i64)> {
    let mut ranges = Vec::new();
    if let Some(index) = segment_index {
        if let Ok(segments) = index.get_segments_for_partition(topic, partition).await {
            ranges.extend(segments.iter().map(|s| (s.min_offset, s.max_offset)));
        }
        if let Ok(segments) = index.get_parquet_segments_for_partition(topic, partition).await {
            ranges.extend(segments.iter().map(|s| (s.min_offset, s.max_offset)));
        }
    }
    ranges
}

/// Every partition of `topic` this node holds anything archived for — raw
/// segments or Tantivy/Parquet index entries. A deleted topic is purged by
/// cutting each of these at 0.
pub async fn archived_partitions(
    store: &dyn ObjectStore,
    segment_index: Option<&Arc<SegmentIndex>>,
    topic: &str,
) -> crate::object_store::errors::ObjectStoreResult<Vec<i32>> {
    // The trailing slash keeps "orders" from matching "orders.eu".
    let prefix = format!("segments/{}/", topic);
    let mut partitions: HashSet<i32> = store
        .list(&prefix)
        .await?
        .into_iter()
        .filter_map(|o| o.key.strip_prefix(&prefix)?.split('/').next()?.parse::<i32>().ok())
        .collect();
    if let Some(index) = segment_index {
        if let Ok(segments) = index.get_segments_for_topic(topic).await {
            partitions.extend(segments.iter().map(|s| s.partition));
        }
        if let Ok(segments) = index.get_all_parquet_segments().await {
            partitions.extend(segments.iter().filter(|s| s.topic == topic).map(|s| s.partition));
        }
    }
    let mut partitions: Vec<i32> = partitions.into_iter().collect();
    partitions.sort_unstable();
    Ok(partitions)
}

/// The lowest offset a fetch can be served from the archive for the
/// partition: its raw segments and, where the raw tier lacks them, its Tantivy
/// archives (fetch falls back to those). `None` when neither holds anything.
/// Parquet is not counted: fetch never reads it.
pub async fn archived_low_watermark(
    store: &dyn ObjectStore,
    segment_index: Option<&Arc<SegmentIndex>>,
    topic: &str,
    partition: i32,
) -> Option<i64> {
    let raw = list_archived_segments(store, topic, partition)
        .await
        .ok()
        .and_then(|segments| segments.first().map(|s| s.base_offset));
    let indexed = match segment_index {
        Some(index) => index
            .get_segments_for_partition(topic, partition)
            .await
            .ok()
            .and_then(|segments| segments.iter().map(|s| s.min_offset).min()),
        None => None,
    };
    match (raw, indexed) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

async fn archived_ranges(
    store: &dyn ObjectStore,
    segment_index: Option<&Arc<SegmentIndex>>,
    topic: &str,
    partition: i32,
) -> crate::object_store::errors::ObjectStoreResult<Vec<(i64, i64)>> {
    let mut ranges: Vec<(i64, i64)> = list_archived_segments(store, topic, partition)
        .await?
        .into_iter()
        .map(|s| (s.base_offset, s.last_offset))
        .collect();
    if let Some(index) = segment_index {
        if let Ok(segments) = index.get_segments_for_partition(topic, partition).await {
            ranges.extend(segments.iter().map(|s| (s.min_offset, s.max_offset)));
        }
        if let Ok(segments) = index.get_parquet_segments_for_partition(topic, partition).await {
            ranges.extend(segments.iter().map(|s| (s.min_offset, s.max_offset)));
        }
    }
    Ok(ranges)
}

/// Drop Tantivy and Parquet segments ending at or above `cut` from the index,
/// and their files. Returns how many index entries were removed.
async fn remove_indexed_from(
    store: &dyn ObjectStore,
    index: &Arc<SegmentIndex>,
    topic: &str,
    partition: i32,
    cut: i64,
) -> usize {
    let mut removed = 0usize;
    let mut paths: HashSet<String> = HashSet::new();

    if let Ok(segments) = index.get_segments_for_partition(topic, partition).await {
        for s in segments.into_iter().filter(|s| s.max_offset >= cut) {
            if matches!(index.remove_segment(topic, partition, &s.segment_id).await, Ok(true)) {
                removed += 1;
                paths.insert(s.object_store_path);
            }
        }
    }
    if let Ok(segments) = index.get_parquet_segments_for_partition(topic, partition).await {
        for s in segments.into_iter().filter(|s| s.max_offset >= cut) {
            if matches!(index.remove_parquet_segment(topic, partition, &s.segment_id).await, Ok(true)) {
                removed += 1;
                paths.insert(s.object_store_path);
            }
        }
    }

    remove_files(store, paths).await;
    removed
}

/// The files of removed index entries, from the store or local disk, with
/// their term filters.
async fn remove_files(store: &dyn ObjectStore, paths: HashSet<String>) {
    // The index entry is what makes a segment servable; the file is space.
    // Remove it from wherever it lives, and only warn if neither place had it.
    for path in paths.into_iter().filter(|p| !p.is_empty()) {
        let in_store = store.delete(&path).await.is_ok();
        let on_disk = std::path::Path::new(&path).is_absolute()
            && tokio::fs::remove_file(&path).await.is_ok();
        if !in_store && !on_disk {
            debug!("archive: no file removed for {} (already gone or elsewhere)", path);
        }
        // A Tantivy archive's term filter goes with it (absent for Parquet).
        let _ = store.delete(&crate::term_filter::term_filter_key(&path)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical_record::{CanonicalRecord, CanonicalRecordEntry, CompressionType, TimestampType};

    fn batches(n: i64) -> Vec<CanonicalRecord> {
        (0..n)
            .map(|b| CanonicalRecord {
                base_offset: b * 100,
                partition_leader_epoch: 0,
                producer_id: -1,
                producer_epoch: -1,
                base_sequence: -1,
                is_transactional: false,
                is_control: false,
                compression: CompressionType::None,
                timestamp_type: TimestampType::CreateTime,
                base_timestamp: 1_700_000_000_000,
                max_timestamp: 1_700_000_000_099,
                records: (b * 100..b * 100 + 100)
                    .map(|o| CanonicalRecordEntry {
                        offset: o,
                        timestamp: 1_700_000_000_000 + o,
                        key: None,
                        value: Some(format!("{{\"level\":\"INFO\",\"service\":\"svc-{}\",\"message\":\"request handled\"}}", o % 7).into_bytes()),
                        headers: vec![],
                        attributes: 0,
                    })
                    .collect(),
                compressed_records_wire_bytes: Some(vec![7u8; 300]),
                original_v1_wire_format: None,
                original_v2_wire_format: None,
            })
            .collect()
    }

    fn archived(base: i64, last: i64, size: u64, written_ms: u64) -> ArchivedSegment {
        ArchivedSegment {
            base_offset: base,
            last_offset: last,
            object_key: archived_segment_key("t", 0, base, last),
            size_bytes: size,
            last_modified_ms: written_ms,
        }
    }

    #[test]
    fn retention_deletes_whole_old_segments_oldest_first_and_keeps_the_newest() {
        const DAY: u64 = 86_400_000;
        let now = 100 * DAY;
        // Written on days 1, 2, 3, 99 and 100.
        let segs = vec![
            archived(0, 99, 10, DAY),
            archived(100, 199, 10, 2 * DAY),
            archived(200, 299, 10, 3 * DAY),
            archived(300, 399, 10, 99 * DAY),
            archived(400, 499, 10, now),
        ];
        let week = Some(7 * DAY as i64);
        assert_eq!(retention_start(&segs, &[], week, None, now), Some(300));
        // Nothing set (or -1): nothing goes.
        assert_eq!(retention_start(&segs, &[], None, None, now), None);
        assert_eq!(retention_start(&segs, &[], Some(-1), Some(-1), now), None);
        // Everything expired: the newest still stays.
        assert_eq!(retention_start(&segs, &[], Some(0), None, now + DAY), Some(400));
        // The first segment kept stops the scan (Kafka: oldest first).
        let mut gap = segs.clone();
        gap[1].last_modified_ms = now;
        assert_eq!(retention_start(&gap, &[], week, None, now), Some(100));
        // retention.bytes: keep at least 25 bytes -> drop the oldest two
        // (50 -> 30; dropping a third would leave 20 < 25).
        assert_eq!(retention_start(&segs, &[], None, Some(25), now), Some(200));
        // Either limit deletes.
        assert_eq!(retention_start(&segs, &[], week, Some(1_000), now), Some(300));
        // A merged index archive straddling the cut holds it back to its start.
        assert_eq!(retention_start(&segs, &[(100, 349)], week, None, now), Some(100));
        assert_eq!(retention_start(&segs, &[(0, 349)], week, None, now), None);
        // A single segment is never deleted.
        assert_eq!(retention_start(&segs[..1], &[], Some(0), Some(0), now), None);
    }

    /// Written compressed, read back identical; a segment written before
    /// compression (plain bincode) still reads.
    #[test]
    fn raw_segments_round_trip_compressed_and_plain_ones_still_read() {
        let records = batches(20);
        let plain = bincode::serialize(&records).unwrap();
        let encoded = encode_raw_segment(&records).unwrap();
        assert!(encoded.starts_with(RAW_SEGMENT_ZSTD_MAGIC));
        assert!(encoded.len() * 4 < plain.len(), "{} compressed vs {} plain", encoded.len(), plain.len());
        assert_eq!(bincode::serialize(&decode_raw_segment(&encoded).unwrap()).unwrap(), plain);
        assert_eq!(bincode::serialize(&decode_raw_segment(&plain).unwrap()).unwrap(), plain);
        // An empty segment, and garbage, behave.
        assert!(decode_raw_segment(&encode_raw_segment(&[]).unwrap()).unwrap().is_empty());
        assert!(decode_raw_segment(b"CHRNKZ1\nnot zstd").is_err());
    }

    #[test]
    fn keys_round_trip_and_reject_anything_else() {
        let key = archived_segment_key("orders", 3, 100, 199);
        assert_eq!(key, "segments/orders/3/100-199.segment");
        assert_eq!(parse_archived_segment_key(&key), Some((100, 199)));
        assert_eq!(parse_archived_segment_key("segments/orders/3/100-199.json"), None);
        assert_eq!(parse_archived_segment_key("segments/orders/3/abc-199.segment"), None);
        assert_eq!(parse_archived_segment_key("segments/orders/3/200-100.segment"), None);
    }

    #[test]
    fn a_cut_between_segments_stays_where_it_was_asked() {
        assert_eq!(settle_cut(200, &[(0, 99), (100, 199), (200, 299)]), 200);
    }

    #[test]
    fn a_straddling_segment_pulls_the_cut_down_to_its_base() {
        assert_eq!(settle_cut(150, &[(0, 99), (100, 199), (200, 299)]), 100);
    }

    #[test]
    fn overlapping_segments_settle_at_the_lowest_base_they_chain_to() {
        // The indexer re-archives a growing tail: 0-72, 0-105, 107-139, 107-168.
        // A cut at 120 straddles 107-139 and 107-168 → 107; nothing straddles 107.
        assert_eq!(settle_cut(120, &[(0, 72), (0, 105), (107, 139), (107, 168)]), 107);
        // A cut at 50 straddles both 0-72 and 0-105 → 0.
        assert_eq!(settle_cut(50, &[(0, 72), (0, 105), (107, 139)]), 0);
        // Chains: cut 150 → 120 (120-160) → 90 (90-130) → stop.
        assert_eq!(settle_cut(150, &[(0, 89), (90, 130), (120, 160)]), 90);
    }

    #[test]
    fn nothing_archived_leaves_the_cut_alone() {
        assert_eq!(settle_cut(42, &[]), 42);
    }
}
