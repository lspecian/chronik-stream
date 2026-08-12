//! Applying fetched records to a follower's local log (RP-2.4).
//!
//! A Fetch response carries the leader's raw RecordBatch bytes — the producer's
//! original wire format, CRC and all. The follower must append exactly those
//! bytes so its log is byte-identical to the leader's; re-encoding would change
//! the CRC-32C on any compressed batch and Java clients would reject it.
//!
//! The decision of *what* to append is separated from the act of appending.
//! [`plan_batches`] is pure — it walks the blob, compares each batch against the
//! follower's current position, and classifies it — so every ordering hazard
//! (duplicate, gap, straddle) is unit-testable without a WAL, a socket or a
//! cluster. The three cluster-only bugs the push stack shipped were all of this
//! shape, and none of them could be reached from a unit test.

use std::sync::Arc;

use chronik_common::metadata::traits::MetadataStore;
use chronik_common::{Error, Result};
use chronik_storage::canonical_record::CanonicalRecord;
use tracing::{debug, warn};

/// Byte offsets within a v2 RecordBatch header.
const BATCH_LENGTH_OFFSET: usize = 8;
const MAGIC_OFFSET: usize = 16;
const LAST_OFFSET_DELTA_OFFSET: usize = 23;
const RECORD_COUNT_OFFSET: usize = 57;
/// Smallest possible v2 batch header: through the record-count field.
const MIN_BATCH_HEADER: usize = 61;

/// One record batch located within a fetched blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchFrame {
    /// Start of the batch within the blob.
    pub start: usize,
    /// One past the end of the batch within the blob.
    pub end: usize,
    pub base_offset: i64,
    /// Offset of the last record in the batch.
    pub last_offset: i64,
    pub record_count: i32,
}

impl BatchFrame {
    /// The follower's LEO after applying this batch.
    pub fn next_offset(&self) -> i64 {
        self.last_offset + 1
    }
}

/// What a follower should do with one batch, given where its log currently ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAction {
    /// Append it — it continues the log exactly.
    Apply(BatchFrame),
    /// Already present. The leader returns whole batches from the one
    /// containing the requested offset, so the first batch of a response can
    /// repeat data the follower already has.
    Skip(BatchFrame),
}

/// Why a fetched blob could not be applied as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyRefusal {
    /// The batch starts beyond where the follower's log ends. Appending would
    /// leave a hole. Usually means the leader's retention has moved past this
    /// replica; the follower must reset rather than invent continuity.
    Gap { expected: i64, found: i64 },
    /// The batch spans the follower's current end — part known, part new. The
    /// follower only ever advances by whole batches, so this cannot happen
    /// against a log that shares this leader's history. It means divergence,
    /// which needs epoch-based truncation (RP-3) rather than a blind append.
    Straddle { expected: i64, base_offset: i64, last_offset: i64 },
    /// The blob is not a well-formed sequence of v2 batches.
    Malformed(String),
}

impl std::fmt::Display for ApplyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyRefusal::Gap { expected, found } => write!(
                f,
                "fetched batch starts at {found} but the local log ends at {expected} — appending would leave a hole"
            ),
            ApplyRefusal::Straddle { expected, base_offset, last_offset } => write!(
                f,
                "fetched batch spans [{base_offset}, {last_offset}] across the local log end {expected} — logs have diverged"
            ),
            ApplyRefusal::Malformed(why) => write!(f, "malformed record blob: {why}"),
        }
    }
}

/// Walk a fetched blob and decide what to do with each batch.
///
/// `expected_offset` is the follower's LEO — the offset its log will next
/// accept. Returns the actions in log order, or the first refusal encountered.
pub fn plan_batches(bytes: &[u8], expected_offset: i64) -> std::result::Result<Vec<BatchAction>, ApplyRefusal> {
    let mut actions = Vec::new();
    let mut cursor = expected_offset;
    let mut pos = 0usize;
    let len = bytes.len();

    while pos < len {
        // A trailing partial batch is normal: the leader truncates the blob at
        // its byte budget. Stop cleanly and let the next fetch pick it up.
        if len - pos < MIN_BATCH_HEADER {
            break;
        }

        let base_offset = read_i64(bytes, pos);
        let batch_length = read_i32(bytes, pos + BATCH_LENGTH_OFFSET);
        if batch_length < 9 {
            return Err(ApplyRefusal::Malformed(format!(
                "batch at byte {pos} declared length {batch_length}"
            )));
        }

        let end = pos + 12 + batch_length as usize;
        if end > len {
            break; // partial trailing batch
        }

        let magic = bytes[pos + MAGIC_OFFSET] as i8;
        if magic != 2 {
            return Err(ApplyRefusal::Malformed(format!(
                "batch at byte {pos} has magic {magic}, only v2 batches can be replicated"
            )));
        }

        let last_offset_delta = read_i32(bytes, pos + LAST_OFFSET_DELTA_OFFSET);
        if last_offset_delta < 0 {
            return Err(ApplyRefusal::Malformed(format!(
                "batch at byte {pos} declared a negative last_offset_delta {last_offset_delta}"
            )));
        }
        let record_count = read_i32(bytes, pos + RECORD_COUNT_OFFSET);

        let frame = BatchFrame {
            start: pos,
            end,
            base_offset,
            last_offset: base_offset + last_offset_delta as i64,
            record_count,
        };

        if frame.last_offset < cursor {
            // Entirely below the log end — a duplicate, drop it.
            actions.push(BatchAction::Skip(frame));
        } else if frame.base_offset == cursor {
            cursor = frame.next_offset();
            actions.push(BatchAction::Apply(frame));
        } else if frame.base_offset < cursor {
            return Err(ApplyRefusal::Straddle {
                expected: cursor,
                base_offset: frame.base_offset,
                last_offset: frame.last_offset,
            });
        } else {
            return Err(ApplyRefusal::Gap {
                expected: cursor,
                found: frame.base_offset,
            });
        }

        pos = end;
    }

    Ok(actions)
}

/// Append one batch to the follower's local log, with the same side effects the
/// leader's produce path applies.
///
/// Shared with the push receive path deliberately: while both mechanisms exist,
/// a record must land identically whichever way it arrived, and when RP-4
/// deletes push this stays put.
pub async fn apply_canonical_batch(
    wal_manager: &Arc<chronik_wal::WalManager>,
    topic: &str,
    partition: i32,
    canonical: &CanonicalRecord,
    produce_handler: Option<&Arc<crate::produce_handler::ProduceHandler>>,
    metadata_store: Option<&Arc<dyn MetadataStore>>,
) -> Result<i64> {
    let record_count = canonical.records.len() as i32;
    if record_count == 0 {
        return Ok(canonical.base_offset);
    }
    let base_offset = canonical.base_offset;
    let last_offset = base_offset + record_count as i64 - 1;
    let next_offset = last_offset + 1;

    let serialized = bincode::serialize(canonical)
        .map_err(|e| Error::Internal(format!("Failed to serialize replicated batch: {e}")))?;

    wal_manager
        .append_canonical_with_acks(
            topic.to_string(),
            partition,
            serialized,
            base_offset,
            last_offset,
            record_count,
            1, // fsync on the follower, matching the push receive path
        )
        .await
        .map_err(|e| {
            Error::Internal(format!(
                "Failed to append replicated batch {topic}-{partition} [{base_offset}, {last_offset}]: {e}"
            ))
        })?;

    // A follower must apply the same transaction-index updates as the leader,
    // or read_committed served from this node computes a different LSO and
    // aborted list than the leader would.
    if let Some(handler) = produce_handler {
        // RP-3: record the epoch this batch was written under, exactly as the
        // leader did when it produced it.
        //
        // Without this a follower builds no epoch history at all while it
        // replicates — `observe_append` was called only from the leader's
        // produce path and from the startup WAL scan. A replica promoted by
        // failover could then answer `OffsetForLeaderEpoch` only for epochs it
        // had produced under itself, and returned "I cannot say" for the very
        // history it had just finished replicating.
        //
        // Measured on a cluster: after a failover the new leader answered -1 and
        // the returning replica logged "not truncating — the leader cannot say
        // where our epoch ended". Truncation after a leader change was therefore
        // impossible, which is the one case RP-3.3 exists for.
        //
        // The epoch travels in the batch itself, so a follower learns the same
        // history from the same bytes. Batches predating RP-3 carry -1 and
        // `observe_append` ignores those.
        handler.leader_epochs().observe_append(
            topic,
            partition,
            canonical.partition_leader_epoch,
            base_offset,
        );

        let first_key = canonical.records.first().and_then(|r| r.key.as_deref());
        handler.transaction_index().apply_log_batch(
            topic,
            partition,
            canonical.producer_id,
            canonical.is_transactional,
            canonical.is_control,
            first_key,
            canonical.base_offset,
        );

        if let Err(e) = handler.update_high_watermark(topic, partition, next_offset).await {
            warn!("Failed to update watermark for {}-{} to {}: {}", topic, partition, next_offset, e);
        }
    }

    // ListOffsets reads the metadata store, not the ProduceHandler, so both
    // have to move or a follower serves stale end offsets.
    if let Some(store) = metadata_store {
        if let Err(e) = store
            .update_partition_offset(topic, partition as u32, next_offset, 0)
            .await
        {
            warn!(
                "Failed to update metadata offset for {}-{} to {}: {}",
                topic, partition, next_offset, e
            );
        }
    }

    debug!(
        "Replicated {}-{} [{}, {}] ({} records)",
        topic, partition, base_offset, last_offset, record_count
    );

    Ok(next_offset)
}

/// Decode and append every applicable batch in a fetched blob.
///
/// Returns the follower's LEO afterwards. A refusal aborts before any append,
/// so a blob is either applied in full or not at all.
pub async fn apply_fetched_records(
    wal_manager: &Arc<chronik_wal::WalManager>,
    topic: &str,
    partition: i32,
    records: &[u8],
    expected_offset: i64,
    produce_handler: Option<&Arc<crate::produce_handler::ProduceHandler>>,
    metadata_store: Option<&Arc<dyn MetadataStore>>,
) -> std::result::Result<i64, ApplyRefusal> {
    let actions = plan_batches(records, expected_offset)?;

    let mut leo = expected_offset;
    for action in actions {
        let frame = match action {
            BatchAction::Skip(frame) => {
                debug!(
                    "Skipping already-replicated {}-{} [{}, {}]",
                    topic, partition, frame.base_offset, frame.last_offset
                );
                continue;
            }
            BatchAction::Apply(frame) => frame,
        };

        let wire = &records[frame.start..frame.end];
        let canonical = CanonicalRecord::from_kafka_batch(wire).map_err(|e| {
            ApplyRefusal::Malformed(format!(
                "batch {topic}-{partition} at offset {} did not decode: {e}",
                frame.base_offset
            ))
        })?;

        match apply_canonical_batch(
            wal_manager,
            topic,
            partition,
            &canonical,
            produce_handler,
            metadata_store,
        )
        .await
        {
            Ok(next) => leo = next,
            Err(e) => {
                // A failed append must not advance the follower's position:
                // the next fetch has to re-request this offset, not skip it.
                return Err(ApplyRefusal::Malformed(format!(
                    "append failed at offset {}: {e}",
                    frame.base_offset
                )));
            }
        }
    }

    Ok(leo)
}

fn read_i32(bytes: &[u8], pos: usize) -> i32 {
    i32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
}

fn read_i64(bytes: &[u8], pos: usize) -> i64 {
    i64::from_be_bytes([
        bytes[pos],
        bytes[pos + 1],
        bytes[pos + 2],
        bytes[pos + 3],
        bytes[pos + 4],
        bytes[pos + 5],
        bytes[pos + 6],
        bytes[pos + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but structurally valid v2 batch header. The record body
    /// is filler — `plan_batches` reads only the header, which is the point:
    /// it can classify batches without paying to decode them.
    fn batch(base_offset: i64, record_count: i32, body_padding: usize) -> Vec<u8> {
        let mut b = Vec::new();
        let payload_len = MIN_BATCH_HEADER - 12 + body_padding;
        b.extend_from_slice(&base_offset.to_be_bytes());
        b.extend_from_slice(&(payload_len as i32).to_be_bytes());
        b.extend_from_slice(&(-1i32).to_be_bytes()); // partition_leader_epoch
        b.push(2); // magic
        b.extend_from_slice(&0u32.to_be_bytes()); // crc
        b.extend_from_slice(&0i16.to_be_bytes()); // attributes
        b.extend_from_slice(&(record_count - 1).to_be_bytes()); // last_offset_delta
        b.extend_from_slice(&0i64.to_be_bytes()); // base_timestamp
        b.extend_from_slice(&0i64.to_be_bytes()); // max_timestamp
        b.extend_from_slice(&(-1i64).to_be_bytes()); // producer_id
        b.extend_from_slice(&(-1i16).to_be_bytes()); // producer_epoch
        b.extend_from_slice(&(-1i32).to_be_bytes()); // base_sequence
        b.extend_from_slice(&record_count.to_be_bytes());
        b.extend(std::iter::repeat(0u8).take(body_padding));
        assert_eq!(b.len(), MIN_BATCH_HEADER + body_padding);
        b
    }

    fn blob(batches: &[Vec<u8>]) -> Vec<u8> {
        batches.iter().flat_map(|b| b.iter().copied()).collect()
    }

    #[test]
    fn contiguous_batches_are_all_applied() {
        let bytes = blob(&[batch(0, 5, 0), batch(5, 3, 8), batch(8, 1, 0)]);

        let actions = plan_batches(&bytes, 0).expect("contiguous blob plans cleanly");

        assert_eq!(actions.len(), 3);
        let applied: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                BatchAction::Apply(f) => Some((f.base_offset, f.last_offset)),
                _ => None,
            })
            .collect();
        assert_eq!(applied, vec![(0, 4), (5, 7), (8, 8)]);
    }

    /// The leader answers a fetch with whole batches starting from the one that
    /// *contains* the requested offset, so a follower routinely re-receives a
    /// batch it already has. Treating that as an error would wedge replication
    /// permanently; treating it as new would duplicate records.
    #[test]
    fn already_replicated_batches_are_skipped_not_reapplied() {
        let bytes = blob(&[batch(0, 5, 0), batch(5, 5, 0), batch(10, 5, 0)]);

        let actions = plan_batches(&bytes, 10).expect("plans cleanly");

        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[0], BatchAction::Skip(_)));
        assert!(matches!(actions[1], BatchAction::Skip(_)));
        match actions[2] {
            BatchAction::Apply(f) => assert_eq!((f.base_offset, f.last_offset), (10, 14)),
            _ => panic!("the batch at the log end must be applied"),
        }
    }

    /// A hole is the failure mode that silently loses data: the follower's log
    /// would report an end offset it cannot actually serve. It must refuse.
    #[test]
    fn a_gap_is_refused_rather_than_papered_over() {
        let bytes = blob(&[batch(100, 5, 0)]);

        let err = plan_batches(&bytes, 40).expect_err("a hole must not be appended");

        assert_eq!(err, ApplyRefusal::Gap { expected: 40, found: 100 });
    }

    /// Nothing is applied before a refusal — a blob lands whole or not at all,
    /// so a later bad batch cannot leave the log half-advanced.
    #[test]
    fn a_gap_later_in_the_blob_discards_the_whole_plan() {
        let bytes = blob(&[batch(0, 5, 0), batch(5, 5, 0), batch(50, 5, 0)]);

        let err = plan_batches(&bytes, 0).expect_err("the trailing hole must refuse the blob");

        assert_eq!(err, ApplyRefusal::Gap { expected: 10, found: 50 });
    }

    /// A batch straddling the log end means the two logs share offsets but not
    /// content. Appending would interleave two histories. This is precisely
    /// what RP-3's epoch-based truncation exists to resolve, and until then the
    /// only safe answer is to stop.
    #[test]
    fn a_straddling_batch_is_reported_as_divergence() {
        let bytes = blob(&[batch(5, 10, 0)]);

        let err = plan_batches(&bytes, 8).expect_err("a straddle must be refused");

        assert_eq!(
            err,
            ApplyRefusal::Straddle { expected: 8, base_offset: 5, last_offset: 14 }
        );
    }

    /// The leader truncates its response at a byte budget, so the last batch is
    /// routinely cut in half. That is normal flow control, not corruption: keep
    /// the whole batches and let the next fetch collect the rest.
    #[test]
    fn a_partial_trailing_batch_is_left_for_the_next_fetch() {
        let full = blob(&[batch(0, 5, 0), batch(5, 5, 0)]);
        let cut = &full[..full.len() - 20];

        let actions = plan_batches(cut, 0).expect("a truncated tail is not an error");

        assert_eq!(actions.len(), 1, "only the complete batch is planned");
        match actions[0] {
            BatchAction::Apply(f) => assert_eq!(f.next_offset(), 5),
            _ => panic!("the first batch should apply"),
        }
    }

    /// A tail too short to even hold a header is the same situation.
    #[test]
    fn a_stub_tail_is_ignored() {
        let mut bytes = blob(&[batch(0, 5, 0)]);
        bytes.extend_from_slice(&[0u8; 7]);

        let actions = plan_batches(&bytes, 0).expect("a stub tail is not an error");
        assert_eq!(actions.len(), 1);
    }

    /// An empty response is the steady state of a caught-up follower.
    #[test]
    fn an_empty_blob_plans_nothing() {
        let actions = plan_batches(&[], 42).expect("empty is fine");
        assert!(actions.is_empty());
    }

    #[test]
    fn a_non_v2_batch_is_rejected() {
        let mut bytes = batch(0, 5, 0);
        bytes[MAGIC_OFFSET] = 1; // magic v1

        let err = plan_batches(&bytes, 0).expect_err("only v2 batches replicate");
        assert!(matches!(err, ApplyRefusal::Malformed(_)));
    }

    #[test]
    fn a_nonsense_batch_length_is_rejected() {
        let mut bytes = batch(0, 5, 0);
        bytes[BATCH_LENGTH_OFFSET..BATCH_LENGTH_OFFSET + 4].copy_from_slice(&1i32.to_be_bytes());

        let err = plan_batches(&bytes, 0).expect_err("a tiny batch length is malformed");
        assert!(matches!(err, ApplyRefusal::Malformed(_)));
    }

    /// A single-record batch has `last_offset_delta == 0`, so the arithmetic
    /// has to be inclusive-of-base or every batch would look one record short.
    #[test]
    fn a_single_record_batch_advances_by_exactly_one() {
        let bytes = batch(77, 1, 0);

        let actions = plan_batches(&bytes, 77).unwrap();
        match actions[0] {
            BatchAction::Apply(f) => {
                assert_eq!(f.last_offset, 77);
                assert_eq!(f.next_offset(), 78);
            }
            _ => panic!("expected an apply"),
        }
    }

    /// Byte ranges must be exact — an off-by-one here would hand
    /// `from_kafka_batch` a clipped batch and corrupt the follower's log.
    #[test]
    fn frames_carry_exact_byte_ranges() {
        let first = batch(0, 2, 4);
        let second = batch(2, 2, 0);
        let bytes = blob(&[first.clone(), second.clone()]);

        let actions = plan_batches(&bytes, 0).unwrap();
        let frames: Vec<BatchFrame> = actions
            .iter()
            .map(|a| match a {
                BatchAction::Apply(f) | BatchAction::Skip(f) => *f,
            })
            .collect();

        assert_eq!(frames[0].start, 0);
        assert_eq!(frames[0].end, first.len());
        assert_eq!(&bytes[frames[0].start..frames[0].end], first.as_slice());
        assert_eq!(&bytes[frames[1].start..frames[1].end], second.as_slice());
    }
}
