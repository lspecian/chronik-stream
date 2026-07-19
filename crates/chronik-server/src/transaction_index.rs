//! Per-partition transaction index for `read_committed` fetch (EOS layer 6).
//!
//! To serve `read_committed` correctly the broker must tell the consumer two things
//! per partition:
//!   1. the **Last Stable Offset (LSO)** — the offset up to which all transactions
//!      have completed (committed or aborted). A `read_committed` consumer never
//!      reads past the LSO, so records of an in-flight transaction stay invisible
//!      until it ends.
//!   2. the **aborted transactions** list — `(producer_id, first_offset)` pairs the
//!      consumer uses (together with the ABORT control markers in the log) to drop
//!      the aborted producer's records.
//!
//! This index tracks, per `(topic, partition)`:
//!   - `ongoing`: producer_id → first offset of its still-open transaction, and
//!   - `aborted`: the aborted `(producer_id, first_offset)` entries.
//!
//! It is updated from the produce path (a transactional data batch opens a txn),
//! and from EndTxn (a COMMIT closes it, an ABORT closes it and records it). It is
//! in-memory; rebuilding it from the WAL on restart is a later layer — until then a
//! restart resets to "no open transactions" (LSO = high watermark), which is safe
//! (never hides committed data) though it forgets aborts across a restart.

use dashmap::DashMap;
use std::collections::HashMap;

/// An aborted transaction as reported in a fetch response.
#[derive(Debug, Clone, Copy)]
pub struct AbortedTxn {
    pub producer_id: i64,
    pub first_offset: i64,
}

#[derive(Default)]
struct PartitionTxnState {
    /// producer_id -> first offset of its open transaction on this partition.
    ongoing: HashMap<i64, i64>,
    /// Aborted transactions, kept so `read_committed` fetches can report them.
    aborted: Vec<AbortedTxn>,
}

/// Shared, lock-free-per-partition transaction index.
#[derive(Default)]
pub struct TransactionIndex {
    partitions: DashMap<(String, i32), PartitionTxnState>,
}

impl TransactionIndex {
    pub fn new() -> Self {
        Self { partitions: DashMap::new() }
    }

    /// A transactional (non-control) data batch landed at `base_offset`. Records the
    /// first offset of this producer's transaction on the partition (idempotent —
    /// only the first batch of a transaction sets the first offset).
    pub fn on_transactional_produce(&self, topic: &str, partition: i32, producer_id: i64, base_offset: i64) {
        let mut entry = self.partitions.entry((topic.to_string(), partition)).or_default();
        entry.ongoing.entry(producer_id).or_insert(base_offset);
    }

    /// A COMMIT marker was written: the transaction's records are now stable.
    pub fn on_commit(&self, topic: &str, partition: i32, producer_id: i64) {
        if let Some(mut entry) = self.partitions.get_mut(&(topic.to_string(), partition)) {
            entry.ongoing.remove(&producer_id);
        }
    }

    /// An ABORT marker was written: close the transaction and remember it so
    /// `read_committed` consumers can drop its records.
    pub fn on_abort(&self, topic: &str, partition: i32, producer_id: i64) {
        if let Some(mut entry) = self.partitions.get_mut(&(topic.to_string(), partition)) {
            if let Some(first_offset) = entry.ongoing.remove(&producer_id) {
                entry.aborted.push(AbortedTxn { producer_id, first_offset });
            }
        }
    }

    /// Apply one batch's effect on the index as it lands in *this node's* copy of
    /// the partition log, from the batch's CanonicalRecord-level attributes.
    ///
    /// This is the log-derived update. It is called on the leader (produce path) and
    /// on followers (replication apply) so the index is a deterministic function of
    /// the replicated log — NOT of which node happened to run the EndTxn RPC. In a
    /// cluster the transaction coordinator and a partition's leader are often
    /// different nodes; updating the index from an RPC handler on the coordinator
    /// left the leader's (and followers') index stale, so `read_committed` on the
    /// leader saw a stuck LSO / leaked aborts. Driving it from the log fixes that.
    ///
    /// `first_record_key` is the key of the batch's first record — for a control
    /// batch it encodes the marker type (i16 version, i16 type: 0 = ABORT,
    /// 1 = COMMIT). Non-transactional / non-producer batches are ignored.
    pub fn apply_log_batch(
        &self,
        topic: &str,
        partition: i32,
        producer_id: i64,
        is_transactional: bool,
        is_control: bool,
        first_record_key: Option<&[u8]>,
        base_offset: i64,
    ) {
        if producer_id < 0 {
            return;
        }
        if is_control {
            match first_record_key
                .filter(|k| k.len() >= 4)
                .map(|k| i16::from_be_bytes([k[2], k[3]]))
            {
                Some(1) => self.on_commit(topic, partition, producer_id),
                Some(0) => self.on_abort(topic, partition, producer_id),
                _ => {}
            }
        } else if is_transactional {
            self.on_transactional_produce(topic, partition, producer_id, base_offset);
        }
    }

    /// The Last Stable Offset for a partition given its current high watermark.
    /// If any transaction is open, the LSO is the lowest open first-offset; otherwise
    /// it is the high watermark (everything is stable).
    pub fn last_stable_offset(&self, topic: &str, partition: i32, high_watermark: i64) -> i64 {
        match self.partitions.get(&(topic.to_string(), partition)) {
            Some(entry) => entry.ongoing.values().min().copied().unwrap_or(high_watermark),
            None => high_watermark,
        }
    }

    /// Aborted transactions whose first offset is below `upper` (the fetch's upper
    /// bound), for the fetch response's aborted list.
    pub fn aborted_below(&self, topic: &str, partition: i32, upper: i64) -> Vec<AbortedTxn> {
        match self.partitions.get(&(topic.to_string(), partition)) {
            Some(entry) => entry.aborted.iter().copied().filter(|a| a.first_offset < upper).collect(),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lso_reflects_open_transactions() {
        let idx = TransactionIndex::new();
        // No transactions: LSO = HWM.
        assert_eq!(idx.last_stable_offset("t", 0, 100), 100);

        // Producer 5 opens a txn at offset 10.
        idx.on_transactional_produce("t", 0, 5, 10);
        assert_eq!(idx.last_stable_offset("t", 0, 100), 10, "LSO capped at open txn's first offset");

        // Another producer opens later; LSO stays at the lowest.
        idx.on_transactional_produce("t", 0, 6, 40);
        assert_eq!(idx.last_stable_offset("t", 0, 100), 10);

        // Commit 5: LSO advances to the next open txn (40).
        idx.on_commit("t", 0, 5);
        assert_eq!(idx.last_stable_offset("t", 0, 100), 40);

        // Commit 6: nothing open, LSO = HWM.
        idx.on_commit("t", 0, 6);
        assert_eq!(idx.last_stable_offset("t", 0, 100), 100);
    }

    #[test]
    fn aborted_transactions_are_recorded_and_filtered_by_offset() {
        let idx = TransactionIndex::new();
        idx.on_transactional_produce("t", 0, 7, 20);
        idx.on_abort("t", 0, 7);
        // After abort, the txn is closed (LSO = HWM) but recorded as aborted.
        assert_eq!(idx.last_stable_offset("t", 0, 100), 100);
        let ab = idx.aborted_below("t", 0, 100);
        assert_eq!(ab.len(), 1);
        assert_eq!(ab[0].producer_id, 7);
        assert_eq!(ab[0].first_offset, 20);
        // A fetch that ends before the aborted txn's first offset must not list it.
        assert!(idx.aborted_below("t", 0, 15).is_empty());
    }

    #[test]
    fn commit_does_not_record_as_aborted() {
        let idx = TransactionIndex::new();
        idx.on_transactional_produce("t", 0, 8, 5);
        idx.on_commit("t", 0, 8);
        assert!(idx.aborted_below("t", 0, 1000).is_empty());
    }
}
