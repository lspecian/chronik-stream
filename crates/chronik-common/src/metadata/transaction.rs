//! Transaction coordinator state types for exactly-once semantics (EOS).
//!
//! These types model the Kafka transaction coordinator's durable state: which
//! producer owns a `transactional.id`, its epoch (for fencing), the current
//! transaction's lifecycle state, and the set of partitions and consumer groups
//! enrolled in the in-flight transaction. The state is event-sourced through the
//! metadata WAL (see `MetadataEventPayload::{BeginTransaction, AddPartitionsToTransaction,
//! CommitTransaction, AbortTransaction, ProducerFenced, ...}`) so it survives
//! restart and replicates to followers.
//!
//! This is the foundation the rest of EOS builds on: `EndTxn` needs the enrolled
//! partition set to write COMMIT/ABORT control markers, and fencing needs the
//! producer_id/epoch mapping.

use std::collections::HashSet;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a transaction, mirroring Kafka's TransactionState.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionState {
    /// Producer registered (InitProducerId) but no transaction started yet, or the
    /// previous transaction fully completed. No partitions enrolled.
    Empty,
    /// A transaction is in progress: partitions/offsets are being added and records
    /// produced. Records written in this window are "uncommitted".
    Ongoing,
    /// EndTxn(commit) received; about to write COMMIT markers.
    PrepareCommit,
    /// EndTxn(abort) received (or timeout/fence); about to write ABORT markers.
    PrepareAbort,
    /// COMMIT markers written; transaction committed. Equivalent to Empty for the
    /// next transaction but distinguished for observability.
    CompleteCommit,
    /// ABORT markers written; transaction aborted.
    CompleteAbort,
}

impl TransactionState {
    /// Whether a new partition/offset can be enrolled in the current transaction.
    pub fn accepts_enrollment(&self) -> bool {
        matches!(self, TransactionState::Empty | TransactionState::Ongoing
            | TransactionState::CompleteCommit | TransactionState::CompleteAbort)
    }

    /// Whether the transaction is mid-completion (markers being written).
    pub fn is_completing(&self) -> bool {
        matches!(self, TransactionState::PrepareCommit | TransactionState::PrepareAbort)
    }
}

/// Durable coordinator state for a single `transactional.id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionMetadata {
    /// The client-supplied transactional id (stable across producer sessions).
    pub transactional_id: String,
    /// The producer id currently bound to this transactional id.
    pub producer_id: i64,
    /// The producer epoch. Bumped on each InitProducerId to fence zombies.
    pub producer_epoch: i16,
    /// Current lifecycle state.
    pub state: TransactionState,
    /// Client-requested transaction timeout.
    pub timeout_ms: i32,
    /// Partitions enrolled in the in-flight transaction (AddPartitionsToTxn).
    /// This is the set EndTxn writes control markers to.
    pub partitions: HashSet<(String, u32)>,
    /// Consumer groups whose offsets are part of the transaction (AddOffsetsToTxn).
    pub groups: HashSet<String>,
    /// Wall-clock millis of the last state transition (for timeout/observability).
    pub last_updated_ms: i64,
}

impl TransactionMetadata {
    /// Create fresh coordinator state for a newly-registered producer (InitProducerId),
    /// in the Empty state with no enrolled partitions.
    pub fn new(transactional_id: String, producer_id: i64, producer_epoch: i16, timeout_ms: i32, now_ms: i64) -> Self {
        Self {
            transactional_id,
            producer_id,
            producer_epoch,
            state: TransactionState::Empty,
            timeout_ms,
            partitions: HashSet::new(),
            groups: HashSet::new(),
            last_updated_ms: now_ms,
        }
    }

    /// True if the given (producer_id, producer_epoch) is fenced by this state —
    /// i.e. it is stale relative to the currently-bound producer. A zombie producer
    /// from a previous session must be rejected with INVALID_PRODUCER_EPOCH /
    /// PRODUCER_FENCED.
    pub fn is_fenced(&self, producer_id: i64, producer_epoch: i16) -> bool {
        producer_id != self.producer_id || producer_epoch < self.producer_epoch
    }
}
