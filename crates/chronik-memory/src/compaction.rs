//! Synchronous compaction pass runner (AM-2.3 `POST /memory/v1/compact`).
//!
//! The lifecycle consumer ([`crate::lifecycle_consumer`]) processes new fact
//! writes as they land — one embedding call per write, decision emitted
//! immediately. That's the steady-state path.
//!
//! [`CompactionController`] is the **admin one-shot**: given the current
//! [`CandidateStore`] snapshot, run
//! [`SemanticDedup::decide`](crate::lifecycle::SemanticDedup::decide) against
//! every fact currently in the store, emit tombstones for
//! `Drop` / `Supersede`, and return a [`CompactionReport`]. It exists so
//! operators can:
//!
//! - re-run dedup after a threshold / config change without waiting for
//!   the next full ingest cycle;
//! - clean up a store rehydrated from a topic that predates the consumer
//!   (older records that never went through `SemanticDedup::decide`);
//! - trigger a scheduled compaction on demand.
//!
//! # Contract
//!
//! - `run(namespace_filter, dry_run)` iterates every `(namespace, subject,
//!   predicate)` group.
//! - When `namespace_filter` is `Some("acme:agent:bot:user:luis")`, only
//!   groups whose `namespace` equals it are processed; the rest are
//!   skipped and counted under `groups_skipped`.
//! - For a group with N ≥ 2 records, each record is checked against the
//!   remaining N-1 as its "existing" set via
//!   [`SemanticDedup::decide`]. This mirrors what the consumer does
//!   incrementally.
//! - Emits are best-effort: an emit failure is logged and counted as a
//!   `emit_errors` bump but does **not** abort the pass.
//! - `dry_run=true` skips emission entirely — the report still reflects
//!   the counts of Drop/Supersede decisions that would have been emitted.
//!
//! # Concurrency
//!
//! The pass acquires no external locks. `CandidateStore` is a
//! `DashMap<CandidateKey, Vec<MemoryRecord>>` — iteration is per-shard, so
//! concurrent inserts from the lifecycle consumer during a compact call are
//! safe but may be observed / missed depending on the shard the write hit.
//! Sufficient for the "operator triggers this occasionally" contract; not
//! a substitute for the always-on consumer.

use crate::embeddings::Embedder;
use crate::error::Result;
use crate::extractor::Extracted;
use crate::lifecycle::{DedupDecision, SemanticDedup};
use crate::lifecycle_consumer::{as_extracted, CandidateStore, DecisionEmitter};
use crate::schema::MemoryRecord;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, warn};

/// Summary of a compact pass. Serialized as the HTTP response body.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionReport {
    /// Total `(namespace, subject, predicate)` groups the pass visited
    /// (i.e. matched the filter, if any).
    pub groups_scanned: u64,
    /// Groups skipped because they did not match `namespace_filter`.
    pub groups_skipped: u64,
    /// Total fact records visited across all scanned groups.
    pub records_scanned: u64,
    /// `Keep` decisions.
    pub keeps: u64,
    /// `Drop` decisions.
    pub drops: u64,
    /// `Supersede` decisions.
    pub supersedes: u64,
    /// Records the pass could not evaluate because
    /// [`SemanticDedup::decide`] errored (typically an embedding fetch).
    pub decide_errors: u64,
    /// Emit failures. Only counted when `dry_run=false`.
    pub emit_errors: u64,
    /// `true` when the pass ran with emission suppressed.
    pub dry_run: bool,
}

/// Runs [`SemanticDedup::decide`] over every fact in a [`CandidateStore`]
/// and emits Drop / Supersede tombstones via a [`DecisionEmitter`].
///
/// Type-erased via [`CompactionRunner`] so the HTTP layer can hold an
/// `Arc<dyn CompactionRunner>` without leaking the embedder generic.
pub struct CompactionController<E: Embedder + Clone + 'static> {
    store: CandidateStore,
    dedup: Arc<SemanticDedup<E>>,
    emitter: Arc<dyn DecisionEmitter>,
    /// Topic template used when emitting: `mem.fact.{tenant}`.
    /// The tenant is derived from `MemoryRecord.namespace` at emit time.
    source_topic_prefix: String,
}

impl<E: Embedder + Clone + 'static> std::fmt::Debug for CompactionController<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactionController")
            .field("dedup_threshold", &self.dedup.threshold())
            .field("store_groups", &self.store.len())
            .finish()
    }
}

impl<E: Embedder + Clone + 'static> CompactionController<E> {
    /// New controller sharing the given store, dedup helper, and emitter.
    ///
    /// `source_topic_prefix` is prepended to `{tenant}` when emitting a
    /// tombstone — defaults to `"mem.fact"` (matches the lifecycle
    /// consumer's subscription). Callers with a bespoke topic layout can
    /// override.
    pub fn new(
        store: CandidateStore,
        dedup: Arc<SemanticDedup<E>>,
        emitter: Arc<dyn DecisionEmitter>,
    ) -> Self {
        Self {
            store,
            dedup,
            emitter,
            source_topic_prefix: "mem.fact".to_string(),
        }
    }

    /// Override the topic prefix used when emitting tombstones. The
    /// tombstone lands on `{prefix}.{tenant}`.
    pub fn with_source_topic_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.source_topic_prefix = prefix.into();
        self
    }

    /// Iterate every `(namespace, subject, predicate)` group and re-run
    /// dedup on each fact.
    ///
    /// When `namespace_filter` is set, only groups whose `namespace`
    /// equals it are processed; the rest are counted under
    /// `groups_skipped`.
    ///
    /// When `dry_run=true`, `Drop`/`Supersede` decisions are counted but
    /// not emitted.
    pub async fn run(
        &self,
        namespace_filter: Option<&str>,
        dry_run: bool,
    ) -> Result<CompactionReport> {
        let mut report = CompactionReport {
            dry_run,
            ..Default::default()
        };

        // Snapshot the store into a plain Vec so the pass doesn't hold a
        // DashMap shard lock while doing embedding calls.
        let groups = self.snapshot_groups();

        for (key_ns, records) in groups {
            if let Some(want_ns) = namespace_filter {
                if key_ns != want_ns {
                    report.groups_skipped = report.groups_skipped.saturating_add(1);
                    continue;
                }
            }
            report.groups_scanned = report.groups_scanned.saturating_add(1);
            report.records_scanned =
                report.records_scanned.saturating_add(records.len() as u64);

            self.process_group(&records, dry_run, &mut report).await;
        }

        Ok(report)
    }

    /// Return `(namespace, Vec<MemoryRecord>)` for every group currently in
    /// the store. The `namespace` string is `MemoryRecord.namespace` (same
    /// value across every record in a group by construction).
    fn snapshot_groups(&self) -> Vec<(String, Vec<MemoryRecord>)> {
        let mut out = Vec::new();
        // Reuse the emitter-free helper via candidates_for: pick one record
        // per unique fact body and look up its group. Instead, take the
        // straightforward path — iterate DashMap entries directly.
        // We stay lock-friendly by cloning the value vectors.
        self.store
            .inner_for_compaction()
            .iter()
            .for_each(|entry| {
                let ns = entry.key().namespace.clone();
                out.push((ns, entry.value().clone()));
            });
        out
    }

    async fn process_group(
        &self,
        records: &[MemoryRecord],
        dry_run: bool,
        report: &mut CompactionReport,
    ) {
        if records.len() < 2 {
            report.keeps = report.keeps.saturating_add(records.len() as u64);
            return;
        }

        for (i, target) in records.iter().enumerate() {
            let others: Vec<MemoryRecord> = records
                .iter()
                .enumerate()
                .filter_map(|(j, r)| (j != i).then(|| r.clone()))
                .collect();
            let extracted: Extracted = as_extracted(target);
            let decision = match self.dedup.decide(&extracted, &others).await {
                Ok(d) => d,
                Err(e) => {
                    warn!(
                        memory_id = %target.memory_id,
                        error = %e,
                        "compact: dedup.decide errored"
                    );
                    report.decide_errors = report.decide_errors.saturating_add(1);
                    continue;
                }
            };
            match decision {
                DedupDecision::Keep => {
                    report.keeps = report.keeps.saturating_add(1);
                }
                DedupDecision::Drop { .. } => {
                    report.drops = report.drops.saturating_add(1);
                    if !dry_run {
                        self.emit_tombstone(&decision, target, report).await;
                    }
                }
                DedupDecision::Supersede { .. } => {
                    report.supersedes = report.supersedes.saturating_add(1);
                    if !dry_run {
                        self.emit_tombstone(&decision, target, report).await;
                    }
                }
            }
        }
    }

    async fn emit_tombstone(
        &self,
        decision: &DedupDecision,
        record: &MemoryRecord,
        report: &mut CompactionReport,
    ) {
        let tenant = record
            .namespace
            .split(':')
            .next()
            .unwrap_or(&record.namespace);
        let topic = format!("{}.{}", self.source_topic_prefix, tenant);
        if let Err(e) = self.emitter.emit(decision, &topic, record).await {
            warn!(
                memory_id = %record.memory_id,
                topic = %topic,
                error = %e,
                "compact: emitter.emit errored"
            );
            report.emit_errors = report.emit_errors.saturating_add(1);
        } else {
            debug!(
                memory_id = %record.memory_id,
                topic = %topic,
                ?decision,
                "compact: emitted tombstone"
            );
        }
    }
}

/// Object-safe wrapper around [`CompactionController`] so the HTTP layer
/// can hold `Arc<dyn CompactionRunner>` without exposing the embedder
/// generic parameter.
#[async_trait::async_trait]
pub trait CompactionRunner: Send + Sync + std::fmt::Debug {
    /// Run a compaction pass. See
    /// [`CompactionController::run`](CompactionController::run).
    async fn run(
        &self,
        namespace_filter: Option<&str>,
        dry_run: bool,
    ) -> Result<CompactionReport>;
}

#[async_trait::async_trait]
impl<E: Embedder + Clone + 'static> CompactionRunner for CompactionController<E> {
    async fn run(
        &self,
        namespace_filter: Option<&str>,
        dry_run: bool,
    ) -> Result<CompactionReport> {
        CompactionController::run(self, namespace_filter, dry_run).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::Extracted;
    use crate::lifecycle_consumer::CapturingEmitter;
    use crate::schema::{Body, FactBody, MemoryRecord, MemoryType};
    use crate::embeddings::Embedder;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Deterministic embedder used by the compaction tests: same text →
    /// same vector, near-duplicate texts → near-parallel vectors.
    #[derive(Debug, Clone)]
    struct DeterministicEmbedder {
        calls: Arc<AtomicUsize>,
    }

    impl DeterministicEmbedder {
        fn new() -> Self {
            Self { calls: Arc::new(AtomicUsize::new(0)) }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Embedder for DeterministicEmbedder {
        async fn embed(&self, text: &str) -> crate::error::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Simple bag-of-chars vector — enough to make two similar
            // strings score high on cosine.
            let mut v = [0f32; 8];
            for (i, ch) in text.chars().enumerate() {
                let idx = (ch as usize + i * 3) % 8;
                v[idx] += 1.0;
            }
            Ok(v.to_vec())
        }
        fn id(&self) -> &str {
            "deterministic-test"
        }
    }

    fn make_fact(
        namespace: &str,
        memory_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        confidence: f32,
    ) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: memory_id.into(),
            namespace: namespace.into(),
            tenant_id: namespace.split(':').next().unwrap_or(namespace).into(),
            key: Some(format!("{}|{}|{}", namespace, subject, predicate)),
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            source: crate::schema::Source {
                topic: "mem.raw.test".into(),
                offsets: vec![0],
                extractor: "test-extractor@1".into(),
            },
            confidence,
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: subject.into(),
                predicate: predicate.into(),
                object: serde_json::Value::String(object.into()),
                polarity: "asserted".into(),
                text: format!("{} {} {}", subject, predicate, object),
            }),
        }
    }

    fn make_controller(
        store: CandidateStore,
        emitter: Arc<dyn DecisionEmitter>,
    ) -> CompactionController<DeterministicEmbedder> {
        let dedup = Arc::new(
            SemanticDedup::new(DeterministicEmbedder::new()).with_threshold(0.90),
        );
        CompactionController::new(store, dedup, emitter)
    }

    #[tokio::test]
    async fn empty_store_reports_zero() {
        let store = CandidateStore::new();
        let emitter: Arc<dyn DecisionEmitter> = Arc::new(CapturingEmitter::new());
        let ctrl = make_controller(store, emitter);
        let r = ctrl.run(None, false).await.unwrap();
        assert_eq!(r.groups_scanned, 0);
        assert_eq!(r.records_scanned, 0);
        assert_eq!(r.keeps, 0);
        assert_eq!(r.drops, 0);
        assert_eq!(r.supersedes, 0);
    }

    #[tokio::test]
    async fn single_record_group_is_kept() {
        let store = CandidateStore::new();
        let ns = "acme:agent:bot:user:luis";
        store.insert(&make_fact(ns, "m1", "luis", "lives_in", "lisbon", 0.9));
        let capture = CapturingEmitter::new();
        let emitter: Arc<dyn DecisionEmitter> = Arc::new(capture.clone());
        let ctrl = make_controller(store, emitter);

        let r = ctrl.run(None, false).await.unwrap();
        assert_eq!(r.groups_scanned, 1);
        assert_eq!(r.records_scanned, 1);
        // Single-record group short-circuits — one Keep.
        assert_eq!(r.keeps, 1);
        assert_eq!(r.drops, 0);
        assert_eq!(r.supersedes, 0);
        assert!(capture.snapshot().is_empty(), "no emission expected");
    }

    #[tokio::test]
    async fn duplicates_get_dropped_or_superseded() {
        let store = CandidateStore::new();
        let ns = "acme:agent:bot:user:luis";
        // Two near-identical records — same subject / predicate.
        store.insert(&make_fact(ns, "m1", "luis", "lives_in", "lisbon", 0.9));
        store.insert(&make_fact(ns, "m2", "luis", "lives_in", "lisbon", 0.95));
        let capture = CapturingEmitter::new();
        let emitter: Arc<dyn DecisionEmitter> = Arc::new(capture.clone());
        let ctrl = make_controller(store, emitter);

        let r = ctrl.run(None, false).await.unwrap();
        assert_eq!(r.groups_scanned, 1);
        assert_eq!(r.records_scanned, 2);
        // Both records evaluate against the other; higher-confidence
        // one supersedes, lower-confidence one drops.
        assert_eq!(r.drops + r.supersedes, 2, "each record decides once");
        assert_eq!(r.keeps, 0);
        assert_eq!(r.decide_errors, 0);
        assert_eq!(r.emit_errors, 0);
        let emitted = capture.snapshot();
        assert_eq!(emitted.len(), 2, "one tombstone per non-Keep");
        // Both emissions land on the right topic.
        for (_, topic, _) in &emitted {
            assert_eq!(topic, "mem.fact.acme");
        }
    }

    #[tokio::test]
    async fn dry_run_counts_but_does_not_emit() {
        let store = CandidateStore::new();
        let ns = "acme:agent:bot:user:luis";
        store.insert(&make_fact(ns, "m1", "luis", "lives_in", "lisbon", 0.9));
        store.insert(&make_fact(ns, "m2", "luis", "lives_in", "lisbon", 0.95));
        let capture = CapturingEmitter::new();
        let emitter: Arc<dyn DecisionEmitter> = Arc::new(capture.clone());
        let ctrl = make_controller(store, emitter);

        let r = ctrl.run(None, true).await.unwrap();
        assert!(r.dry_run);
        assert_eq!(r.drops + r.supersedes, 2);
        assert!(capture.snapshot().is_empty(), "dry-run must not emit");
    }

    #[tokio::test]
    async fn namespace_filter_skips_other_tenants() {
        let store = CandidateStore::new();
        let ns_a = "acme:agent:bot:user:luis";
        let ns_b = "beta:agent:bot:user:ana";
        store.insert(&make_fact(ns_a, "a1", "luis", "lives_in", "lisbon", 0.9));
        store.insert(&make_fact(ns_a, "a2", "luis", "lives_in", "lisbon", 0.95));
        store.insert(&make_fact(ns_b, "b1", "ana", "works_at", "acme", 0.8));
        let capture = CapturingEmitter::new();
        let emitter: Arc<dyn DecisionEmitter> = Arc::new(capture.clone());
        let ctrl = make_controller(store, emitter);

        let r = ctrl.run(Some(ns_a), false).await.unwrap();
        assert_eq!(r.groups_scanned, 1, "only the acme group scanned");
        assert_eq!(r.groups_skipped, 1, "beta group skipped");
        assert_eq!(r.records_scanned, 2);
        for (_, topic, _) in capture.snapshot() {
            assert_eq!(topic, "mem.fact.acme");
        }
    }

    #[tokio::test]
    async fn report_serializes_to_json() {
        let r = CompactionReport {
            groups_scanned: 2,
            groups_skipped: 0,
            records_scanned: 5,
            keeps: 3,
            drops: 1,
            supersedes: 1,
            decide_errors: 0,
            emit_errors: 0,
            dry_run: false,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"groups_scanned\":2"));
        assert!(s.contains("\"records_scanned\":5"));
        let back: CompactionReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }
}

