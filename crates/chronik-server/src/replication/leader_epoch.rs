//! Leader epoch history (RP-3).
//!
//! A leader epoch is a counter that increments every time a partition changes
//! leader. Each record batch carries the epoch of the leader that wrote it, so
//! the log itself records its own leadership history — and that is what lets two
//! replicas discover *where* they diverged rather than guessing.
//!
//! ## Why offsets alone are not enough
//!
//! Two replicas can hold different records at the same offset. Leader A accepts
//! offsets 100..110 and dies before anyone replicates them; B is elected and
//! accepts different records at 100..105. A returns, and its log and B's log
//! agree on offset numbering and disagree on content. Comparing log end offsets
//! cannot see this: A is *ahead*, and truncating to the shorter log would be
//! right here and wrong in other orderings.
//!
//! With epochs the question becomes answerable. A asks the new leader "where did
//! epoch 4 end?", B replies "at 100", and A truncates to 100 — discarding
//! exactly the records that were never replicated, and nothing else.
//!
//! Kafka needed KIP-101, then KIP-279 and KIP-320 to get this right; the
//! roadmap says not to treat it as a detail, so the semantics below follow
//! Kafka's `LeaderEpochFileCache` deliberately rather than being reinvented.
//!
//! ## Where the history comes from
//!
//! It is **derived from the log**, not stored separately: every replica builds
//! the same history by watching the epoch stamped on each batch it appends. A
//! follower therefore knows its own leadership history without being told, which
//! is what makes the truncation exchange a single request.

use std::collections::HashMap;

use dashmap::DashMap;

/// No epoch / no offset. Kafka's `UNDEFINED_EPOCH` and `UNDEFINED_EPOCH_OFFSET`.
pub const UNDEFINED_EPOCH: i32 = -1;
pub const UNDEFINED_OFFSET: i64 = -1;

/// The offset at which a leader epoch began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochEntry {
    pub epoch: i32,
    /// First offset written by this epoch's leader.
    pub start_offset: i64,
}

/// One partition's leadership history.
///
/// Entries are kept strictly increasing in both epoch and start offset.
#[derive(Debug, Default, Clone)]
pub struct LeaderEpochCache {
    entries: Vec<EpochEntry>,
}

impl LeaderEpochCache {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[EpochEntry] {
        &self.entries
    }

    /// The most recent epoch this replica has seen in its log.
    ///
    /// This is what a follower sends when asking a new leader where to truncate.
    pub fn latest_epoch(&self) -> Option<i32> {
        self.entries.last().map(|e| e.epoch)
    }

    /// Record that `epoch` begins at `start_offset`.
    ///
    /// Called on every append with the batch's stamped epoch; cheap and
    /// idempotent for the overwhelmingly common case where the epoch has not
    /// changed since the last batch.
    pub fn assign(&mut self, epoch: i32, start_offset: i64) {
        if epoch < 0 {
            return; // unstamped batch (pre-RP-3 data) — nothing to learn
        }

        if let Some(last) = self.entries.last() {
            if epoch < last.epoch {
                // An older epoch appearing after a newer one means the log was
                // rewritten beneath us. Refusing is safer than recording a
                // history that is not monotonic; the caller truncates first.
                return;
            }
            if epoch == last.epoch {
                return; // same leader, still writing — no new entry
            }
        }

        // A new epoch starting at or before an existing entry supersedes it:
        // that entry's records are being replaced by this leader's.
        self.entries.retain(|e| e.start_offset < start_offset);
        self.entries.push(EpochEntry { epoch, start_offset });
    }

    /// The epoch that was in force at `offset`, if known.
    pub fn epoch_for_offset(&self, offset: i64) -> Option<i32> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.start_offset <= offset)
            .map(|e| e.epoch)
    }

    /// Answer `OffsetForLeaderEpoch`: where did `requested` end?
    ///
    /// Returns `(epoch, end_offset)`, following Kafka's semantics exactly:
    ///
    /// - the **current** epoch ends at the log end offset — it has not ended yet,
    ///   so everything written so far belongs to it;
    /// - an **earlier** epoch ends where the next epoch began;
    /// - an epoch **newer than anything here**, or **older than anything still
    ///   retained**, is answered `(UNDEFINED, UNDEFINED)`. The asker holds
    ///   history this replica cannot speak to, and must fall back rather than
    ///   act on a number that means something else.
    ///
    /// Getting that last case wrong is how epoch truncation causes the data loss
    /// it exists to prevent: returning 0, or the log end offset, for an unknown
    /// epoch tells a follower to discard a correct log or keep a divergent one.
    pub fn end_offset_for_epoch(&self, requested: i32, log_end_offset: i64) -> (i32, i64) {
        if requested < 0 || self.entries.is_empty() {
            return (UNDEFINED_EPOCH, UNDEFINED_OFFSET);
        }

        let latest = self.entries.last().expect("non-empty").epoch;
        if requested == latest {
            return (requested, log_end_offset);
        }

        let previous = self.entries.iter().rev().find(|e| e.epoch <= requested);
        let subsequent = self.entries.iter().find(|e| e.epoch > requested);

        match (previous, subsequent) {
            (Some(p), Some(s)) => (p.epoch, s.start_offset),
            // requested is beyond the newest, or below the oldest we still hold
            _ => (UNDEFINED_EPOCH, UNDEFINED_OFFSET),
        }
    }

    /// Drop history at or above `end_offset`, after the log was truncated there.
    pub fn truncate_from_end(&mut self, end_offset: i64) {
        if end_offset < 0 {
            return;
        }
        self.entries.retain(|e| e.start_offset < end_offset);
    }

    /// Drop history below `start_offset` after retention advanced the log start.
    ///
    /// The entry covering `start_offset` is kept and clamped, so the epoch of
    /// the oldest surviving record is still answerable.
    pub fn truncate_from_start(&mut self, start_offset: i64) {
        if start_offset <= 0 || self.entries.is_empty() {
            return;
        }

        let covering = self
            .entries
            .iter()
            .rev()
            .find(|e| e.start_offset <= start_offset)
            .copied();

        self.entries.retain(|e| e.start_offset > start_offset);

        if let Some(mut entry) = covering {
            entry.start_offset = start_offset;
            self.entries.insert(0, entry);
        }
    }
}

/// Per-partition leader epoch histories for this node.
#[derive(Debug, Default)]
pub struct LeaderEpochStore {
    caches: DashMap<(String, i32), LeaderEpochCache>,
}

impl LeaderEpochStore {
    pub fn new() -> Self {
        Self { caches: DashMap::new() }
    }

    /// Record the epoch stamped on an appended batch. Hot path.
    pub fn observe_append(&self, topic: &str, partition: i32, epoch: i32, base_offset: i64) {
        if epoch < 0 {
            return;
        }
        self.caches
            .entry((topic.to_string(), partition))
            .or_default()
            .assign(epoch, base_offset);
    }

    pub fn latest_epoch(&self, topic: &str, partition: i32) -> Option<i32> {
        self.caches
            .get(&(topic.to_string(), partition))
            .and_then(|c| c.latest_epoch())
    }

    pub fn end_offset_for_epoch(
        &self,
        topic: &str,
        partition: i32,
        requested: i32,
        log_end_offset: i64,
    ) -> (i32, i64) {
        match self.caches.get(&(topic.to_string(), partition)) {
            Some(cache) => cache.end_offset_for_epoch(requested, log_end_offset),
            None => (UNDEFINED_EPOCH, UNDEFINED_OFFSET),
        }
    }

    pub fn truncate_from_end(&self, topic: &str, partition: i32, end_offset: i64) {
        if let Some(mut cache) = self.caches.get_mut(&(topic.to_string(), partition)) {
            cache.truncate_from_end(end_offset);
        }
    }

    pub fn snapshot(&self, topic: &str, partition: i32) -> Vec<EpochEntry> {
        self.caches
            .get(&(topic.to_string(), partition))
            .map(|c| c.entries().to_vec())
            .unwrap_or_default()
    }

    /// Every partition with recorded history, for diagnostics.
    pub fn tracked(&self) -> HashMap<(String, i32), i32> {
        self.caches
            .iter()
            .filter_map(|e| e.value().latest_epoch().map(|epoch| (e.key().clone(), epoch)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(entries: &[(i32, i64)]) -> LeaderEpochCache {
        let mut c = LeaderEpochCache::new();
        for &(epoch, start) in entries {
            c.assign(epoch, start);
        }
        c
    }

    /// A leader writing many batches under one epoch produces one entry, not one
    /// per batch. `assign` is called on every append, so anything else would grow
    /// without bound on a busy partition.
    #[test]
    fn a_steady_leader_records_one_entry() {
        let mut c = LeaderEpochCache::new();
        for offset in [0, 10, 20, 30, 40] {
            c.assign(5, offset);
        }

        assert_eq!(c.entries(), &[EpochEntry { epoch: 5, start_offset: 0 }]);
        assert_eq!(c.latest_epoch(), Some(5));
    }

    #[test]
    fn each_leadership_change_starts_a_new_entry() {
        let c = cache(&[(1, 0), (2, 50), (5, 90)]);

        assert_eq!(
            c.entries(),
            &[
                EpochEntry { epoch: 1, start_offset: 0 },
                EpochEntry { epoch: 2, start_offset: 50 },
                EpochEntry { epoch: 5, start_offset: 90 },
            ],
            "epochs need not be consecutive — elections can be lost without writing"
        );
    }

    /// The current epoch has not ended, so everything written so far belongs to
    /// it. A follower asking about the epoch it shares with the leader is told
    /// "keep everything", which is the common, no-op case.
    #[test]
    fn the_current_epoch_ends_at_the_log_end() {
        let c = cache(&[(1, 0), (4, 100)]);
        assert_eq!(c.end_offset_for_epoch(4, 250), (4, 250));
    }

    /// The whole point: an earlier epoch ends exactly where the next began, and
    /// that offset is where a diverged follower must truncate to.
    #[test]
    fn an_earlier_epoch_ends_where_the_next_began() {
        let c = cache(&[(1, 0), (2, 100), (3, 175)]);

        assert_eq!(c.end_offset_for_epoch(1, 300), (1, 100));
        assert_eq!(c.end_offset_for_epoch(2, 300), (2, 175));
    }

    /// The scenario epochs exist for. Leader A accepted 100..110 under epoch 4
    /// and died before replicating them. B took over at 100 under epoch 5. A
    /// comes back holding records B never had, at offsets B has since reused.
    ///
    /// Offsets alone say A is ahead. Epochs say epoch 4 ended at 100, so A
    /// discards exactly its unreplicated tail.
    #[test]
    fn a_returning_leader_is_told_where_its_epoch_ended() {
        let new_leader = cache(&[(3, 0), (4, 60), (5, 100)]);

        let (epoch, truncate_to) = new_leader.end_offset_for_epoch(4, 140);

        assert_eq!(epoch, 4);
        assert_eq!(truncate_to, 100, "everything the old leader wrote past 100 is gone");
    }

    /// An epoch newer than anything the leader knows means the asker holds
    /// history this replica cannot speak to. Answering with a real offset would
    /// be a guess, and a guess here either discards a correct log or keeps a
    /// divergent one — the exact damage epochs exist to prevent.
    #[test]
    fn an_unknown_future_epoch_is_undefined_not_guessed() {
        let c = cache(&[(1, 0), (2, 100)]);

        assert_eq!(c.end_offset_for_epoch(9, 500), (UNDEFINED_EPOCH, UNDEFINED_OFFSET));
    }

    /// Likewise for an epoch that retention has aged out entirely.
    #[test]
    fn an_epoch_older_than_retained_history_is_undefined() {
        let mut c = cache(&[(1, 0), (5, 100), (6, 200)]);
        c.truncate_from_start(150);

        assert_eq!(c.end_offset_for_epoch(1, 400), (UNDEFINED_EPOCH, UNDEFINED_OFFSET));
    }

    #[test]
    fn an_empty_history_answers_undefined() {
        let c = LeaderEpochCache::new();
        assert_eq!(c.end_offset_for_epoch(3, 100), (UNDEFINED_EPOCH, UNDEFINED_OFFSET));
        assert_eq!(c.latest_epoch(), None);
    }

    /// Pre-RP-3 batches carry -1. They must be ignored rather than recorded as
    /// an epoch, or every old log would claim a leadership change at offset 0.
    #[test]
    fn unstamped_batches_are_ignored() {
        let mut c = LeaderEpochCache::new();
        c.assign(-1, 0);
        c.assign(-1, 50);

        assert!(c.is_empty());
        assert_eq!(c.end_offset_for_epoch(-1, 100), (UNDEFINED_EPOCH, UNDEFINED_OFFSET));
    }

    /// A new epoch beginning at or before an existing entry replaces it: those
    /// offsets are being rewritten by the new leader, so claiming the old epoch
    /// still covers them would answer later queries with a superseded offset.
    #[test]
    fn a_new_epoch_supersedes_entries_it_overwrites() {
        let mut c = cache(&[(1, 0), (2, 100), (3, 150)]);

        // New leader takes over and starts writing at 100 again.
        c.assign(4, 100);

        assert_eq!(
            c.entries(),
            &[
                EpochEntry { epoch: 1, start_offset: 0 },
                EpochEntry { epoch: 4, start_offset: 100 },
            ]
        );
        assert_eq!(c.end_offset_for_epoch(1, 200), (1, 100));
    }

    /// An older epoch arriving after a newer one means the log moved beneath us.
    /// Recording it would make the history non-monotonic and every later answer
    /// unreliable; the caller must truncate first.
    #[test]
    fn a_stale_epoch_is_refused() {
        let mut c = cache(&[(5, 0)]);
        c.assign(3, 50);

        assert_eq!(c.entries(), &[EpochEntry { epoch: 5, start_offset: 0 }]);
    }

    #[test]
    fn truncating_the_log_end_drops_the_history_above_it() {
        let mut c = cache(&[(1, 0), (2, 100), (3, 200)]);
        c.truncate_from_end(150);

        assert_eq!(
            c.entries(),
            &[
                EpochEntry { epoch: 1, start_offset: 0 },
                EpochEntry { epoch: 2, start_offset: 100 },
            ]
        );
        assert_eq!(c.latest_epoch(), Some(2));
    }

    /// Retention must not erase the epoch of the oldest surviving record — the
    /// covering entry is kept and clamped instead of dropped.
    #[test]
    fn retention_keeps_the_epoch_covering_the_new_log_start() {
        let mut c = cache(&[(1, 0), (2, 100), (3, 200)]);
        c.truncate_from_start(150);

        assert_eq!(
            c.entries(),
            &[
                EpochEntry { epoch: 2, start_offset: 150 },
                EpochEntry { epoch: 3, start_offset: 200 },
            ],
            "epoch 2 still covers offsets 150..199 and must remain answerable"
        );
        assert_eq!(c.epoch_for_offset(150), Some(2));
    }

    #[test]
    fn offsets_map_back_to_the_epoch_in_force() {
        let c = cache(&[(1, 0), (4, 100), (7, 250)]);

        assert_eq!(c.epoch_for_offset(0), Some(1));
        assert_eq!(c.epoch_for_offset(99), Some(1));
        assert_eq!(c.epoch_for_offset(100), Some(4));
        assert_eq!(c.epoch_for_offset(300), Some(7));
    }

    #[test]
    fn the_store_keeps_partitions_apart() {
        let store = LeaderEpochStore::new();

        store.observe_append("orders", 0, 1, 0);
        store.observe_append("orders", 0, 2, 50);
        store.observe_append("orders", 1, 9, 0);

        assert_eq!(store.latest_epoch("orders", 0), Some(2));
        assert_eq!(store.latest_epoch("orders", 1), Some(9));
        assert_eq!(store.latest_epoch("orders", 2), None);

        assert_eq!(store.end_offset_for_epoch("orders", 0, 1, 90), (1, 50));
        assert_eq!(
            store.end_offset_for_epoch("unknown", 0, 1, 90),
            (UNDEFINED_EPOCH, UNDEFINED_OFFSET),
            "a partition we hold no history for must not be answered with a guess"
        );
    }

    #[test]
    fn the_store_truncates_per_partition() {
        let store = LeaderEpochStore::new();
        store.observe_append("orders", 0, 1, 0);
        store.observe_append("orders", 0, 2, 100);
        store.observe_append("orders", 1, 1, 0);
        store.observe_append("orders", 1, 2, 100);

        store.truncate_from_end("orders", 0, 50);

        assert_eq!(store.latest_epoch("orders", 0), Some(1));
        assert_eq!(store.latest_epoch("orders", 1), Some(2), "the other partition is untouched");
    }
}
