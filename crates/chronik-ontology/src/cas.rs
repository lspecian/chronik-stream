//! O-3 CAS-append spike (standalone) — roadmap §6/§9.
//!
//! The one hard engine primitive Actions need is **single-aggregate
//! compare-and-swap append**: "append these events to the stream keyed `K` only
//! if `K`'s latest offset is still `N`", atomically. The broker does not have it
//! yet ([produce_handler.rs](../../chronik-server/src/produce_handler.rs) has
//! idempotent-sequence dedup but no expected-offset validation), and adding it
//! is a correctness-critical change to the core produce path — so the roadmap
//! says to **spike it standalone first**.
//!
//! This module is that spike: [`CasLog`] models exactly what the broker
//! CAS-append must do, and its unit tests pin the semantics the §9 OPEN
//! decisions call for — concurrent same-key (exactly one wins), stale-token
//! rejection, idempotent retry (no double-apply), and atomic all-or-nothing
//! batch append. The design proven here is the contract for the later
//! produce-path integration.
//!
//! **Resolved design decisions (§9):**
//! - **Token scope:** the concurrency token is the aggregate's head **offset**;
//!   all events for aggregate `K` route to one ordered stream (in the broker:
//!   partition key = `K`, so one partition offset is authoritative for `K`).
//! - **Atomic batch:** the whole `emitted_events` batch is CAS-checked and
//!   appended under one critical section — all-or-nothing, never per-event.
//! - **Idempotency:** a retry carrying the same `idempotency_key` returns the
//!   original ack and appends nothing (safe under producer-timeout ambiguity).

use std::collections::HashMap;
use std::sync::Mutex;

/// The aggregate key — the unit of compare-and-swap.
pub type AggregateKey = String;

/// Ack for a successful CAS-append: the offsets the batch landed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendAck {
    pub key: AggregateKey,
    pub first_offset: i64,
    pub last_offset: i64,
}

/// A CAS precondition failure: the aggregate moved since the caller read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasConflict {
    pub key: AggregateKey,
    pub expected: i64,
    pub actual: i64,
}

#[derive(Default)]
struct Inner {
    /// key -> appended events; the offset of an event is its index in the Vec.
    streams: HashMap<AggregateKey, Vec<serde_json::Value>>,
    /// idempotency_key -> the ack produced when it was first applied.
    applied: HashMap<String, AppendAck>,
}

/// Thread-safe in-memory model of the broker's single-aggregate CAS-append.
#[derive(Default)]
pub struct CasLog {
    inner: Mutex<Inner>,
}

impl CasLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The head offset for `key` — the offset of its latest event, or `-1` when
    /// the aggregate has no events yet (so a first append expects `-1`).
    pub fn head_offset(&self, key: &str) -> i64 {
        let g = self.inner.lock().unwrap();
        g.streams.get(key).map(|v| v.len() as i64 - 1).unwrap_or(-1)
    }

    /// Compare-and-swap append. Appends `events` to `key` **iff** its head offset
    /// is still `expected_offset`, atomically. A retry with a previously-seen
    /// `idempotency_key` is a no-op that returns the original ack.
    pub fn cas_append(
        &self,
        key: &str,
        expected_offset: i64,
        events: Vec<serde_json::Value>,
        idempotency_key: &str,
    ) -> Result<AppendAck, CasConflict> {
        let mut g = self.inner.lock().unwrap();
        // Idempotency first: a retry never re-applies or conflicts.
        if let Some(prior) = g.applied.get(idempotency_key) {
            return Ok(prior.clone());
        }
        let head = g.streams.get(key).map(|v| v.len() as i64 - 1).unwrap_or(-1);
        if head != expected_offset {
            return Err(CasConflict {
                key: key.to_string(),
                expected: expected_offset,
                actual: head,
            });
        }
        // Atomic all-or-nothing append of the whole batch.
        let stream = g.streams.entry(key.to_string()).or_default();
        let first_offset = stream.len() as i64;
        for e in events {
            stream.push(e);
        }
        let last_offset = stream.len() as i64 - 1;
        let ack = AppendAck {
            key: key.to_string(),
            first_offset,
            last_offset,
        };
        g.applied.insert(idempotency_key.to_string(), ack.clone());
        Ok(ack)
    }

    /// Number of events currently in `key`'s stream (test/introspection).
    pub fn len(&self, key: &str) -> usize {
        self.inner.lock().unwrap().streams.get(key).map(|v| v.len()).unwrap_or(0)
    }

    /// Snapshot `key`'s current event stream — for projecting the aggregate's
    /// state (the read an Action's validator does before proposing). Returns an
    /// empty vec for an aggregate with no events yet.
    pub fn snapshot(&self, key: &str) -> Vec<serde_json::Value> {
        self.inner.lock().unwrap().streams.get(key).cloned().unwrap_or_default()
    }
}

/// The Action lifecycle over CAS-append (roadmap §6): propose (no effect) →
/// [confirm] → apply (validated + CAS-appended). Here we model apply: run the
/// validator, then CAS-append the emitted events using the proposal's
/// `read_offset` as the concurrency token.
pub struct ActionApply<'a> {
    pub aggregate_key: AggregateKey,
    /// Offset the proposal was computed against (its concurrency token).
    pub read_offset: i64,
    pub idempotency_key: String,
    pub emitted_events: Vec<serde_json::Value>,
    /// Validator: cross-object invariants / permissions. Returns Err(reason) to
    /// reject before any append (dry-run has zero effect — it just never calls
    /// this).
    pub validate: &'a dyn Fn(&[serde_json::Value]) -> Result<(), String>,
}

/// Outcome of applying an Action.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied(AppendAck),
    /// A concurrent apply moved the aggregate — the classic lost-update guard.
    Conflict(CasConflict),
    /// The validator rejected the command (no append happened).
    Rejected(String),
}

impl CasLog {
    /// Validate, then CAS-append the Action's events atomically.
    pub fn apply_action(&self, action: ActionApply<'_>) -> ApplyOutcome {
        if let Err(reason) = (action.validate)(&action.emitted_events) {
            return ApplyOutcome::Rejected(reason);
        }
        match self.cas_append(
            &action.aggregate_key,
            action.read_offset,
            action.emitted_events,
            &action.idempotency_key,
        ) {
            Ok(ack) => ApplyOutcome::Applied(ack),
            Err(c) => ApplyOutcome::Conflict(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ev(n: i64) -> serde_json::Value {
        serde_json::json!({ "n": n })
    }
    fn accept(_: &[serde_json::Value]) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn first_append_expects_minus_one_then_head_advances() {
        let log = CasLog::new();
        assert_eq!(log.head_offset("K"), -1);
        let ack = log.cas_append("K", -1, vec![ev(1)], "i1").unwrap();
        assert_eq!(ack, AppendAck { key: "K".into(), first_offset: 0, last_offset: 0 });
        assert_eq!(log.head_offset("K"), 0);
    }

    #[test]
    fn stale_token_is_rejected() {
        let log = CasLog::new();
        log.cas_append("K", -1, vec![ev(1)], "i1").unwrap(); // head now 0
        // A caller that read at -1 is stale.
        let err = log.cas_append("K", -1, vec![ev(2)], "i2").unwrap_err();
        assert_eq!(err, CasConflict { key: "K".into(), expected: -1, actual: 0 });
        assert_eq!(log.len("K"), 1); // nothing appended
    }

    #[test]
    fn idempotent_retry_does_not_double_apply() {
        let log = CasLog::new();
        let a1 = log.cas_append("K", -1, vec![ev(1)], "same").unwrap();
        // Same idempotency key (a producer-timeout retry) — no double-apply.
        let a2 = log.cas_append("K", -1, vec![ev(1)], "same").unwrap();
        assert_eq!(a1, a2);
        assert_eq!(log.len("K"), 1);
    }

    #[test]
    fn atomic_batch_all_or_nothing() {
        let log = CasLog::new();
        // A 3-event batch lands atomically at offsets 0..=2.
        let ack = log.cas_append("K", -1, vec![ev(1), ev(2), ev(3)], "i1").unwrap();
        assert_eq!(ack, AppendAck { key: "K".into(), first_offset: 0, last_offset: 2 });
        // A conflicting batch appends NOTHING.
        let _ = log.cas_append("K", 0, vec![ev(4), ev(5)], "i2").unwrap_err();
        assert_eq!(log.len("K"), 3);
    }

    #[test]
    fn concurrent_same_key_exactly_one_wins() {
        // The core lost-update guard: N threads all read head=-1 and race to
        // append; exactly ONE succeeds, the rest get a CasConflict.
        let log = Arc::new(CasLog::new());
        let mut handles = Vec::new();
        for i in 0..16 {
            let log = Arc::clone(&log);
            handles.push(std::thread::spawn(move || {
                log.cas_append("K", -1, vec![ev(i)], &format!("idem-{i}"))
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wins = results.iter().filter(|r| r.is_ok()).count();
        let conflicts = results.iter().filter(|r| r.is_err()).count();
        assert_eq!(wins, 1, "exactly one writer may win the CAS");
        assert_eq!(conflicts, 15);
        assert_eq!(log.len("K"), 1);
    }

    #[test]
    fn apply_action_rejects_via_validator_without_append() {
        let log = CasLog::new();
        let outcome = log.apply_action(ActionApply {
            aggregate_key: "K".into(),
            read_offset: -1,
            idempotency_key: "i1".into(),
            emitted_events: vec![ev(1)],
            validate: &|_| Err("cross-object invariant violated".into()),
        });
        assert_eq!(outcome, ApplyOutcome::Rejected("cross-object invariant violated".into()));
        assert_eq!(log.len("K"), 0);
    }

    #[test]
    fn apply_action_conflict_on_concurrent_move() {
        let log = CasLog::new();
        log.cas_append("K", -1, vec![ev(0)], "seed").unwrap(); // head 0
        // Proposal computed against read_offset -1 is now stale.
        let outcome = log.apply_action(ActionApply {
            aggregate_key: "K".into(),
            read_offset: -1,
            idempotency_key: "i1".into(),
            emitted_events: vec![ev(1)],
            validate: &accept,
        });
        assert_eq!(
            outcome,
            ApplyOutcome::Conflict(CasConflict { key: "K".into(), expected: -1, actual: 0 })
        );
    }
}
