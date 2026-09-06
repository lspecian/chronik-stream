//! O-3 Action Types — validated writeback as an event-sourcing command handler.
//!
//! Productizes the [`crate::cas`] spike into the roadmap's propose→confirm→apply
//! lifecycle (§6) **entirely at the ontology layer** over [`CasLog`] — the
//! coordinator/optimistic-CAS approach, so **no chronik core change** (broker
//! CAS-append is cancelled). Each Action:
//!   1. **propose** — record intent + read the aggregate's current head as the
//!      concurrency token; **zero domain effect** (free dry-run/preview).
//!   2. **confirm** — required for `confirm`/`approval` risk tiers; `auto` skips.
//!   3. **apply** — a [`CommandHandler`] validates against current state and
//!      derives the domain events; they are **CAS-appended** iff the aggregate
//!      is still at the proposal's `read_offset` (lost-update guard), idempotent
//!      by key, and an [`AuditRecord`] is written.
//!
//! Cross-object invariants (e.g. "can't close a ticket with an open blocker")
//! are checked by the handler via a [`StateReader`], which can snapshot any
//! aggregate — the coordinator serializes the check+append per aggregate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cas::CasLog;

/// Per-action risk tier (drives whether a human gate is required before apply).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskTier {
    /// Reads / safe writes — apply immediately, no confirmation.
    Auto,
    /// Writes — require an explicit confirm before apply.
    Confirm,
    /// Destructive — require confirm (an approver identity is recorded).
    Approval,
}

/// A registered Action Type definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionType {
    pub name: String,
    pub risk_tier: RiskTier,
    #[serde(default)]
    pub description: Option<String>,
}

/// A proposed action — the concurrency token (`read_offset`) is captured at
/// propose time; a stale token at apply is a lost-update conflict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub proposal_id: u64,
    pub aggregate_key: String,
    pub action_type: String,
    pub params: Value,
    pub idempotency_key: String,
    pub read_offset: i64,
    /// Set once confirmed (`confirm`/`approval` tiers); `None` for un-confirmed.
    pub confirmed_by: Option<String>,
}

/// Audit trail entry for an applied action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub proposal_id: u64,
    pub action_type: String,
    pub aggregate_key: String,
    pub confirmed_by: Option<String>,
    pub emitted: Vec<Value>,
    pub first_offset: i64,
    pub last_offset: i64,
}

/// Outcome of applying an action.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionOutcome {
    /// Applied — validator passed, events CAS-appended, audited.
    Applied(AuditRecord),
    /// The risk tier requires a confirmation the proposal doesn't carry.
    NeedsConfirmation,
    /// The command handler rejected the action (invariant/permission) — no append.
    Rejected(String),
    /// A concurrent apply moved the aggregate since it was read (lost-update guard).
    Conflict { expected: i64, actual: i64 },
    /// The action type was never registered.
    UnknownAction(String),
}

/// Read access to any aggregate's event stream — lets a handler check
/// cross-object invariants (a blocker's state, an owner's permissions, …).
pub trait StateReader {
    fn snapshot(&self, aggregate_key: &str) -> Vec<Value>;
}
impl StateReader for CasLog {
    fn snapshot(&self, aggregate_key: &str) -> Vec<Value> {
        CasLog::snapshot(self, aggregate_key)
    }
}

/// Domain command handler: validate a proposed action against current state and
/// derive the domain events to emit, or reject with a reason. **Pure** — no I/O;
/// reads state only through the supplied [`StateReader`].
pub trait CommandHandler {
    fn handle(
        &self,
        action_type: &str,
        params: &Value,
        aggregate_key: &str,
        state: &dyn StateReader,
    ) -> Result<Vec<Value>, String>;
}

/// The ontology-layer Action engine: a registry + the CAS coordinator + an audit
/// log. One engine owns one [`CasLog`] (the event store for its aggregates).
pub struct ActionEngine {
    log: CasLog,
    registry: HashMap<String, ActionType>,
    audit: Mutex<Vec<AuditRecord>>,
    /// idempotency_key -> the audit record produced when it was first applied,
    /// so a retry replays the original outcome without re-validating stale state.
    applied_by_idem: Mutex<HashMap<String, AuditRecord>>,
    next_id: AtomicU64,
}

impl Default for ActionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionEngine {
    pub fn new() -> Self {
        Self {
            log: CasLog::new(),
            registry: HashMap::new(),
            audit: Mutex::new(Vec::new()),
            applied_by_idem: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Access the underlying event store (to seed initial aggregate state).
    pub fn log(&self) -> &CasLog {
        &self.log
    }

    /// Register (or replace) an Action Type.
    pub fn register(&mut self, action_type: ActionType) {
        self.registry.insert(action_type.name.clone(), action_type);
    }

    pub fn action_type(&self, name: &str) -> Option<&ActionType> {
        self.registry.get(name)
    }

    pub fn audit_log(&self) -> Vec<AuditRecord> {
        self.audit.lock().unwrap().clone()
    }

    /// Propose an action — records intent and captures the aggregate's current
    /// head as the concurrency token. **No domain effect.**
    pub fn propose(
        &self,
        aggregate_key: &str,
        action_type: &str,
        params: Value,
        idempotency_key: &str,
    ) -> Proposal {
        Proposal {
            proposal_id: self.next_id.fetch_add(1, Ordering::SeqCst),
            aggregate_key: aggregate_key.to_string(),
            action_type: action_type.to_string(),
            params,
            idempotency_key: idempotency_key.to_string(),
            read_offset: self.log.head_offset(aggregate_key),
            confirmed_by: None,
        }
    }

    /// Confirm a proposal (for `confirm`/`approval` tiers). Records the approver.
    pub fn confirm(&self, mut proposal: Proposal, approver: &str) -> Proposal {
        proposal.confirmed_by = Some(approver.to_string());
        proposal
    }

    /// Dry-run: run the handler to preview the events an apply WOULD emit,
    /// without appending anything. Returns the events or the rejection reason.
    /// (A Proposal event carries no domain effect, so preview is free.)
    pub fn preview(
        &self,
        proposal: &Proposal,
        handler: &dyn CommandHandler,
    ) -> Result<Vec<Value>, String> {
        handler.handle(
            &proposal.action_type,
            &proposal.params,
            &proposal.aggregate_key,
            &self.log,
        )
    }

    /// Apply a proposal. Correct optimistic-concurrency order:
    /// **idempotency → risk-tier gate → token check → validate → CAS-append**.
    /// - Idempotency first, so a retry replays the original outcome and never
    ///   re-validates against state that has since changed.
    /// - Token check before validation, so a caller whose read is stale gets a
    ///   clean `Conflict` ("re-read and retry"), not a confusing `Rejected`
    ///   derived from state they never saw.
    pub fn apply(&self, proposal: &Proposal, handler: &dyn CommandHandler) -> ActionOutcome {
        // 1. Idempotency: a retry replays the original applied outcome.
        if let Some(rec) = self
            .applied_by_idem
            .lock()
            .unwrap()
            .get(&proposal.idempotency_key)
        {
            return ActionOutcome::Applied(rec.clone());
        }
        // 2. Known action?
        let Some(at) = self.registry.get(&proposal.action_type) else {
            return ActionOutcome::UnknownAction(proposal.action_type.clone());
        };
        // 3. Risk-tier gate: confirm/approval must be confirmed before apply.
        if matches!(at.risk_tier, RiskTier::Confirm | RiskTier::Approval)
            && proposal.confirmed_by.is_none()
        {
            return ActionOutcome::NeedsConfirmation;
        }
        // 4. Token check: if the aggregate moved since the proposal read it, the
        //    proposer's decision is based on stale state — conflict, don't apply.
        let head = self.log.head_offset(&proposal.aggregate_key);
        if head != proposal.read_offset {
            return ActionOutcome::Conflict {
                expected: proposal.read_offset,
                actual: head,
            };
        }
        // 5. Validate + derive events (cross-object invariants read via the log,
        //    which now matches read_offset since the token is fresh).
        let emitted = match handler.handle(
            &proposal.action_type,
            &proposal.params,
            &proposal.aggregate_key,
            &self.log,
        ) {
            Ok(ev) => ev,
            Err(reason) => return ActionOutcome::Rejected(reason),
        };
        // 6. CAS-append the whole batch atomically at the read token. The
        //    internal check also catches the rare race between step 4 and here.
        match self.log.cas_append(
            &proposal.aggregate_key,
            proposal.read_offset,
            emitted.clone(),
            &proposal.idempotency_key,
        ) {
            Ok(ack) => {
                let record = AuditRecord {
                    proposal_id: proposal.proposal_id,
                    action_type: proposal.action_type.clone(),
                    aggregate_key: proposal.aggregate_key.clone(),
                    confirmed_by: proposal.confirmed_by.clone(),
                    emitted,
                    first_offset: ack.first_offset,
                    last_offset: ack.last_offset,
                };
                self.audit.lock().unwrap().push(record.clone());
                self.applied_by_idem
                    .lock()
                    .unwrap()
                    .insert(proposal.idempotency_key.clone(), record.clone());
                ActionOutcome::Applied(record)
            }
            Err(c) => ActionOutcome::Conflict {
                expected: c.expected,
                actual: c.actual,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── A tiny issue-tracker command handler (the O-3 exit domain) ──────────
    struct IssueTracker;

    fn project_status(events: &[Value]) -> String {
        let mut status = "open".to_string();
        let mut blockers: Vec<String> = Vec::new();
        for e in events {
            let val = e.get("value").and_then(|v| v.as_str()).unwrap_or("");
            match e.get("kind").and_then(|v| v.as_str()) {
                Some("status") => status = val.to_string(),
                Some("blocked_by") => blockers.push(val.to_string()),
                Some("unblocked") => blockers.retain(|b| b != val),
                _ => {}
            }
        }
        status
    }
    fn project_blockers(events: &[Value]) -> Vec<String> {
        let mut blockers: Vec<String> = Vec::new();
        for e in events {
            let val = e.get("value").and_then(|v| v.as_str()).unwrap_or("");
            match e.get("kind").and_then(|v| v.as_str()) {
                Some("blocked_by") => blockers.push(val.to_string()),
                Some("unblocked") => blockers.retain(|b| b != val),
                _ => {}
            }
        }
        blockers
    }

    impl CommandHandler for IssueTracker {
        fn handle(
            &self,
            action_type: &str,
            _params: &Value,
            aggregate_key: &str,
            state: &dyn StateReader,
        ) -> Result<Vec<Value>, String> {
            let events = state.snapshot(aggregate_key);
            let status = project_status(&events);
            match action_type {
                "resolve" => {
                    if status == "open" || status == "in_progress" {
                        Ok(vec![json!({"kind":"status","value":"resolved"})])
                    } else {
                        Err(format!("resolve requires open/in_progress (is {status})"))
                    }
                }
                "close" => {
                    for b in project_blockers(&events) {
                        let bs = project_status(&state.snapshot(&b));
                        if bs != "resolved" && bs != "closed" {
                            return Err(format!("close blocked: {b} is {bs}"));
                        }
                    }
                    Ok(vec![json!({"kind":"status","value":"closed"})])
                }
                other => Err(format!("unknown command {other}")),
            }
        }
    }

    fn engine_with_tickets() -> ActionEngine {
        let mut e = ActionEngine::new();
        e.register(ActionType { name: "resolve".into(), risk_tier: RiskTier::Auto, description: None });
        e.register(ActionType { name: "close".into(), risk_tier: RiskTier::Confirm, description: None });
        // seed: T1 open; T2 open & blocked_by T1
        e.log().cas_append("T1", -1, vec![json!({"kind":"status","value":"open"})], "seed-T1").unwrap();
        e.log()
            .cas_append(
                "T2",
                -1,
                vec![json!({"kind":"status","value":"open"}), json!({"kind":"blocked_by","value":"T1"})],
                "seed-T2",
            )
            .unwrap();
        e
    }

    #[test]
    fn propose_has_no_domain_effect_and_captures_token() {
        let e = engine_with_tickets();
        let before = e.log().len("T1");
        let p = e.propose("T1", "resolve", json!({}), "i1");
        assert_eq!(p.read_offset, 0); // head after the single seed event
        assert_eq!(e.log().len("T1"), before); // nothing appended
        assert!(p.confirmed_by.is_none());
    }

    #[test]
    fn preview_is_dry_run() {
        let e = engine_with_tickets();
        let p = e.propose("T1", "resolve", json!({}), "i1");
        let evs = e.preview(&p, &IssueTracker).unwrap();
        assert_eq!(evs, vec![json!({"kind":"status","value":"resolved"})]);
        assert_eq!(e.log().len("T1"), 1); // preview appended nothing
    }

    #[test]
    fn auto_tier_applies_without_confirmation_and_audits() {
        let e = engine_with_tickets();
        let p = e.propose("T1", "resolve", json!({}), "i1");
        match e.apply(&p, &IssueTracker) {
            ActionOutcome::Applied(rec) => {
                assert_eq!(rec.action_type, "resolve");
                assert_eq!(rec.emitted.len(), 1);
            }
            o => panic!("expected Applied, got {o:?}"),
        }
        assert_eq!(project_status(&e.log().snapshot("T1")), "resolved");
        assert_eq!(e.audit_log().len(), 1);
    }

    #[test]
    fn confirm_tier_needs_confirmation_then_applies() {
        let e = engine_with_tickets();
        // First resolve the blocker (auto) so close is legal.
        let rp = e.propose("T1", "resolve", json!({}), "ir");
        assert!(matches!(e.apply(&rp, &IssueTracker), ActionOutcome::Applied(_)));

        // close is confirm-tier: un-confirmed apply is gated.
        let p = e.propose("T2", "close", json!({}), "ic");
        assert_eq!(e.apply(&p, &IssueTracker), ActionOutcome::NeedsConfirmation);
        assert_eq!(project_status(&e.log().snapshot("T2")), "open"); // unchanged

        // confirm, then apply succeeds.
        let p = e.confirm(p, "alice");
        assert!(matches!(e.apply(&p, &IssueTracker), ActionOutcome::Applied(rec) if rec.confirmed_by.as_deref()==Some("alice")));
        assert_eq!(project_status(&e.log().snapshot("T2")), "closed");
    }

    #[test]
    fn invariant_rejection_no_append() {
        let e = engine_with_tickets();
        // close T2 while blocker T1 is still open -> rejected, nothing emitted.
        let p = e.confirm(e.propose("T2", "close", json!({}), "ic"), "alice");
        match e.apply(&p, &IssueTracker) {
            ActionOutcome::Rejected(r) => assert!(r.contains("blocked")),
            o => panic!("expected Rejected, got {o:?}"),
        }
        assert_eq!(project_status(&e.log().snapshot("T2")), "open");
        assert!(e.audit_log().is_empty());
    }

    #[test]
    fn stale_token_conflicts_lost_update_guard() {
        let e = engine_with_tickets();
        let p1 = e.propose("T1", "resolve", json!({}), "a");
        let p2 = e.propose("T1", "resolve", json!({}), "b"); // both read head=0
        assert!(matches!(e.apply(&p1, &IssueTracker), ActionOutcome::Applied(_)));
        // p2's token is now stale -> conflict.
        match e.apply(&p2, &IssueTracker) {
            ActionOutcome::Conflict { expected, actual } => {
                assert_eq!(expected, 0);
                assert_eq!(actual, 1);
            }
            o => panic!("expected Conflict, got {o:?}"),
        }
    }

    #[test]
    fn idempotent_retry_does_not_double_apply() {
        let e = engine_with_tickets();
        let p = e.propose("T1", "resolve", json!({}), "same");
        assert!(matches!(e.apply(&p, &IssueTracker), ActionOutcome::Applied(_)));
        let before = e.log().len("T1");
        // Retry with the same idempotency key -> replay, no double append.
        assert!(matches!(e.apply(&p, &IssueTracker), ActionOutcome::Applied(_)));
        assert_eq!(e.log().len("T1"), before);
    }

    #[test]
    fn unknown_action_is_reported() {
        let e = engine_with_tickets();
        let p = e.propose("T1", "teleport", json!({}), "x");
        assert_eq!(e.apply(&p, &IssueTracker), ActionOutcome::UnknownAction("teleport".into()));
    }
}
