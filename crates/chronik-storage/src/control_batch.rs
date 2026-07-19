//! Transaction control batches (COMMIT / ABORT end-transaction markers).
//!
//! When a transaction ends, the coordinator appends a *control batch* to every
//! partition that was part of the transaction. A control batch is a normal Kafka
//! RecordBatch with the control + transactional attribute bits set, carrying a
//! single control record whose key encodes the marker type (0 = ABORT, 1 = COMMIT).
//!
//! `read_committed` consumers use these markers to know where a transaction
//! commits or aborts in the log: the broker returns records only up to the Last
//! Stable Offset and an `aborted_transactions` list, and the consumer drops the
//! aborted producer's records when it reaches the ABORT marker.
//!
//! The batch is built with the same [`KafkaRecordBatch::encode`] path used for
//! ordinary produce data, so the CRC-32C is computed identically and correctly —
//! we do not hand-roll the wire format or the checksum.

use bytes::{BufMut, Bytes, BytesMut};
use crate::kafka_records::{KafkaRecordBatch, CompressionType, attributes};

/// Control marker type. The numeric values are the on-wire control-record key
/// `type` field defined by the Kafka protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMarker {
    Abort = 0,
    Commit = 1,
}

/// Build the wire bytes of an end-transaction control batch for one partition.
///
/// - `base_offset` — the offset this control record occupies (the partition's next
///   offset). The marker consumes exactly one offset.
/// - `producer_id` / `producer_epoch` — identify the transaction being closed.
/// - `marker` — COMMIT or ABORT.
/// - `timestamp` — log-append timestamp for the marker.
///
/// The returned bytes are a complete v2 RecordBatch (control + transactional),
/// ready to be wrapped in a `CanonicalRecord` and appended to the WAL exactly like
/// produced data.
pub fn build_end_txn_marker(
    base_offset: i64,
    producer_id: i64,
    producer_epoch: i16,
    marker: ControlMarker,
    timestamp: i64,
) -> Vec<u8> {
    // Control-record KEY: version (i16 = 0) + type (i16: 0 abort, 1 commit).
    let mut key = BytesMut::with_capacity(4);
    key.put_i16(0);
    key.put_i16(marker as i16);

    // Control-record VALUE: version (i16 = 0) + coordinator_epoch (i32).
    // We do not run a separate coordinator-epoch state machine, so 0 is used;
    // consumers only need the marker type + producer id to resolve a transaction.
    let mut value = BytesMut::with_capacity(6);
    value.put_i16(0);
    value.put_i32(0);

    let mut batch = KafkaRecordBatch::new(
        base_offset,
        timestamp,
        producer_id,
        producer_epoch,
        0,     // base_sequence: control records are not part of the data sequence space
        CompressionType::None,
        true,  // is_transactional
    );
    // Mark this batch as a control batch (attribute bit 5). KafkaRecordBatch::new
    // only sets the transactional bit; OR in the control bit here.
    batch.header.attributes |= attributes::CONTROL_FLAG;

    batch.add_record(Some(key.freeze()), Some(value.freeze()), Vec::new(), timestamp);

    // encode() computes batch_length + CRC-32C over the correct byte range.
    batch.encode().map(|b| b.to_vec()).unwrap_or_default()
}

/// Convenience: build a marker as `Bytes`.
pub fn build_end_txn_marker_bytes(
    base_offset: i64,
    producer_id: i64,
    producer_epoch: i16,
    marker: ControlMarker,
    timestamp: i64,
) -> Bytes {
    Bytes::from(build_end_txn_marker(base_offset, producer_id, producer_epoch, marker, timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kafka_records::KafkaRecordBatch;

    #[test]
    fn marker_round_trips_and_is_flagged_control_and_transactional() {
        let bytes = build_end_txn_marker(42, 1000, 3, ControlMarker::Commit, 1_700_000_000_000);
        // Decode via the same production decoder — proves the wire format + CRC are valid.
        let (decoded, _n) = KafkaRecordBatch::decode(&bytes)
            .expect("control batch must decode with a valid CRC");
        assert_eq!(decoded.header.base_offset, 42);
        assert_eq!(decoded.header.producer_id, 1000);
        assert_eq!(decoded.header.producer_epoch, 3);
        assert_ne!(decoded.header.attributes & attributes::CONTROL_FLAG, 0, "control bit set");
        assert_ne!(decoded.header.attributes & attributes::TRANSACTIONAL_FLAG, 0, "transactional bit set");
        assert_eq!(decoded.header.records_count, 1, "exactly one control record");
    }

    #[test]
    fn commit_and_abort_markers_differ_in_key_type() {
        let commit = build_end_txn_marker(0, 1, 0, ControlMarker::Commit, 1);
        let abort = build_end_txn_marker(0, 1, 0, ControlMarker::Abort, 1);
        let (dc, _) = KafkaRecordBatch::decode(&commit).unwrap();
        let (da, _) = KafkaRecordBatch::decode(&abort).unwrap();
        // The control-record key's type field (2nd i16) distinguishes commit(1)/abort(0).
        let commit_key = dc.records[0].key.as_ref().unwrap();
        let abort_key = da.records[0].key.as_ref().unwrap();
        assert_eq!(&commit_key[2..4], &[0x00, 0x01], "commit type=1");
        assert_eq!(&abort_key[2..4], &[0x00, 0x00], "abort type=0");
    }
}
