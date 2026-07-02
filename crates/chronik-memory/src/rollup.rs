//! AM-3.1 event rollup scaffold — window `mem.event.{tenant}` records
//! by `(actor, object)` and emit synthesis triggers when a count
//! threshold is reached.
//!
//! # Scope (MVP)
//!
//! - Count-based windows: emit a trigger after N events land on the
//!   same `(namespace, actor, object)` key.
//! - Trigger v1 only — count-based, no LLM classifier deciding whether
//!   the rollup is warranted (that's v2, kept out of MVP).
//! - Summary emission is delegated to a [`RollupSummarizer`] trait so
//!   this module has no Kafka / LLM dependency. Two impls:
//!   - [`NoopSummarizer`] (default) — counts triggers, doesn't emit.
//!   - Callers wire an LLM-backed summarizer (e.g. from
//!     [`crate::extractor::AnthropicExtractor`]) in a wrapper crate.
//!
//! # Concurrency
//!
//! `RollupBuffer` is `DashMap<Key, Vec<MemoryRecord>>`. Ingestion is
//! lock-free per key. Emission drains + trims the vec under the shard
//! lock, so the trigger runs at most once per threshold crossing per
//! `(namespace, actor, object)`.
//!
//! # Testability
//!
//! Buffer + trigger + summarizer are pure. Wire-up to a live
//! `mem.event.*` consumer belongs in `chronik-server`.

use crate::schema::{Body, MemoryRecord};
use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, warn};

/// Trait-erased summary emitter. When a rollup window crosses its
/// count threshold, [`RollupBuffer::observe`] hands the drained batch
/// to the summarizer.
///
/// Implementations produce a summary memory (or a trigger event to a
/// summary-worker topic). Failure is best-effort — logged + swallowed.
#[async_trait]
pub trait RollupSummarizer: Send + Sync + std::fmt::Debug {
    /// Summarize the drained events. Returns `Ok(())` on success.
    async fn summarize(
        &self,
        namespace: &str,
        actor: &str,
        object: &str,
        events: &[MemoryRecord],
    ) -> crate::error::Result<()>;
}

/// No-op summarizer — logs the trigger and counts it via
/// [`RollupStats`] but doesn't emit anything. Used by tests and by
/// default deployments (opt-in to a real summarizer via config).
#[derive(Debug, Default, Clone)]
pub struct NoopSummarizer;

#[async_trait]
impl RollupSummarizer for NoopSummarizer {
    async fn summarize(
        &self,
        namespace: &str,
        actor: &str,
        object: &str,
        events: &[MemoryRecord],
    ) -> crate::error::Result<()> {
        debug!(
            namespace,
            actor,
            object,
            n = events.len(),
            "rollup: NoopSummarizer trigger (would summarize)"
        );
        Ok(())
    }
}

/// Test-only summarizer that captures every trigger into a shared
/// buffer for later inspection.
#[derive(Debug, Default, Clone)]
pub struct CapturingSummarizer {
    /// `(namespace, actor, object, event_count)` per call.
    pub captured: Arc<parking_lot::Mutex<Vec<(String, String, String, usize)>>>,
}

impl CapturingSummarizer {
    /// Fresh empty capture buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the captured calls in call order.
    pub fn snapshot(&self) -> Vec<(String, String, String, usize)> {
        self.captured.lock().clone()
    }
}

#[async_trait]
impl RollupSummarizer for CapturingSummarizer {
    async fn summarize(
        &self,
        namespace: &str,
        actor: &str,
        object: &str,
        events: &[MemoryRecord],
    ) -> crate::error::Result<()> {
        self.captured.lock().push((
            namespace.to_string(),
            actor.to_string(),
            object.to_string(),
            events.len(),
        ));
        Ok(())
    }
}

/// Composite key for rollup windows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RollupKey {
    /// Full namespace path.
    pub namespace: String,
    /// Event `actor` (`(actor, verb, object)` triple).
    pub actor: String,
    /// Event `object` — `""` when the event's object is `None`.
    pub object: String,
}

/// In-memory event window. Cheap to clone (`Arc` interior).
#[derive(Debug, Default, Clone)]
pub struct RollupBuffer {
    inner: Arc<DashMap<RollupKey, Vec<MemoryRecord>>>,
    /// Count threshold above which a summarizer is triggered on the
    /// key. Configured at construction; `0` disables triggering.
    threshold: usize,
}

/// Running counters emitted by the buffer.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RollupStats {
    /// Records the buffer has observed.
    pub events_observed: u64,
    /// Records skipped because they weren't `Body::Event`.
    pub non_event_records: u64,
    /// Distinct `(namespace, actor, object)` keys currently tracked.
    pub windows_open: u64,
    /// Number of times a window crossed the threshold and fired.
    pub triggers_fired: u64,
    /// Number of times the summarizer errored.
    pub summarizer_errors: u64,
}

impl RollupBuffer {
    /// Build a buffer with the given count threshold. `threshold=10`
    /// matches the roadmap default; `0` disables triggering
    /// (observe-only mode).
    pub fn new(threshold: usize) -> Self {
        Self {
            inner: Default::default(),
            threshold,
        }
    }

    /// Number of open windows.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` if no windows are open.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Observe one record. Non-`Body::Event` records are skipped and
    /// counted. When the record pushes its window across the
    /// threshold, the buffer drains that window and returns
    /// `Some(RollupKey, Vec<MemoryRecord>)` so the caller can hand the
    /// batch to a summarizer. Otherwise returns `None`.
    ///
    /// **Non-blocking**: draining happens under the DashMap shard
    /// write lock; the summarizer call belongs to the caller so this
    /// method stays sync.
    pub fn observe(&self, record: &MemoryRecord, stats: &mut RollupStats) -> Option<(RollupKey, Vec<MemoryRecord>)> {
        stats.events_observed += 1;
        let Body::Event(body) = &record.body else {
            stats.non_event_records += 1;
            return None;
        };
        let key = RollupKey {
            namespace: record.namespace.clone(),
            actor: body.actor.clone(),
            object: body.object.clone().unwrap_or_default(),
        };
        // Compute + potentially drain inside a bounded block so the
        // DashMap entry write lock is released before we call
        // `self.inner.len()` — otherwise `len()` walks every shard and
        // can deadlock on the shard we're holding.
        let fire = {
            let mut entry = self.inner.entry(key.clone()).or_default();
            entry.push(record.clone());
            if self.threshold > 0 && entry.len() >= self.threshold {
                let drained = std::mem::take(entry.value_mut());
                Some(drained)
            } else {
                None
            }
        };
        stats.windows_open = self.inner.len() as u64;
        if let Some(drained) = fire {
            stats.triggers_fired += 1;
            return Some((key, drained));
        }
        None
    }

    /// Snapshot the count per window. Handy for diagnostics; not on
    /// the hot path.
    pub fn snapshot_counts(&self) -> Vec<(RollupKey, usize)> {
        self.inner
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().len()))
            .collect()
    }

    /// Drop a specific window (e.g. when the namespace is
    /// deprovisioned).
    pub fn forget(&self, key: &RollupKey) -> bool {
        self.inner.remove(key).is_some()
    }
}

/// Convenience: observe a record, and if a trigger fires, invoke the
/// summarizer inline. Errors from the summarizer are logged +
/// counted, never propagated — the buffer stays consistent.
pub async fn observe_and_maybe_summarize(
    buffer: &RollupBuffer,
    stats: &Arc<parking_lot::Mutex<RollupStats>>,
    summarizer: &dyn RollupSummarizer,
    record: &MemoryRecord,
) {
    let fire = {
        let mut s = stats.lock();
        buffer.observe(record, &mut s)
    };
    if let Some((key, events)) = fire {
        if let Err(e) = summarizer
            .summarize(&key.namespace, &key.actor, &key.object, &events)
            .await
        {
            let mut s = stats.lock();
            s.summarizer_errors += 1;
            warn!(error = %e, "rollup: summarizer failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, EventBody, FactBody, MemoryRecord, Source};
    use chrono::Utc;

    fn event(namespace: &str, actor: &str, object: Option<&str>) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: format!("evt-{now}"),
            tenant_id: namespace.split(':').next().unwrap_or(namespace).into(),
            namespace: namespace.into(),
            key: None,
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "mem.event.t".into(),
                offsets: vec![0],
                extractor: "test@1".into(),
            },
            tombstoned: false,
            body: Body::Event(EventBody {
                actor: actor.into(),
                verb: "viewed".into(),
                object: object.map(str::to_string),
                channel: None,
                context: None,
                ts: now,
            }),
        }
    }

    fn fact(namespace: &str) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: "f".into(),
            tenant_id: "t".into(),
            namespace: namespace.into(),
            key: Some("u|p".into()),
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: "mem.raw.t".into(),
                offsets: vec![0],
                extractor: "test@1".into(),
            },
            tombstoned: false,
            body: Body::Fact(FactBody {
                subject: "u".into(),
                predicate: "p".into(),
                object: serde_json::json!("o"),
                polarity: "asserted".into(),
                text: "u p o".into(),
            }),
        }
    }

    #[test]
    fn empty_buffer_reports_zero() {
        let b = RollupBuffer::new(10);
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn non_event_records_are_skipped() {
        let b = RollupBuffer::new(10);
        let mut s = RollupStats::default();
        let fired = b.observe(&fact("ns"), &mut s);
        assert!(fired.is_none());
        assert_eq!(s.non_event_records, 1);
        assert_eq!(s.events_observed, 1);
        assert!(b.is_empty(), "non-Event records must not open a window");
    }

    #[test]
    fn below_threshold_stays_buffered() {
        let b = RollupBuffer::new(3);
        let mut s = RollupStats::default();
        for _ in 0..2 {
            let fired = b.observe(&event("ns", "u", Some("p:1")), &mut s);
            assert!(fired.is_none());
        }
        assert_eq!(s.events_observed, 2);
        assert_eq!(s.triggers_fired, 0);
        let counts = b.snapshot_counts();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].1, 2);
    }

    #[test]
    fn crossing_threshold_fires_and_drains() {
        let b = RollupBuffer::new(3);
        let mut s = RollupStats::default();
        let mut fires = 0;
        for _ in 0..3 {
            if b.observe(&event("ns", "u", Some("p:1")), &mut s).is_some() {
                fires += 1;
            }
        }
        assert_eq!(fires, 1);
        assert_eq!(s.triggers_fired, 1);
        // Drained → window empty again.
        let counts = b.snapshot_counts();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].1, 0);
    }

    #[test]
    fn threshold_zero_disables_triggering() {
        let b = RollupBuffer::new(0);
        let mut s = RollupStats::default();
        for _ in 0..100 {
            let fired = b.observe(&event("ns", "u", Some("p:1")), &mut s);
            assert!(fired.is_none());
        }
        assert_eq!(s.events_observed, 100);
        assert_eq!(s.triggers_fired, 0);
    }

    #[test]
    fn distinct_actor_object_pairs_have_separate_windows() {
        let b = RollupBuffer::new(10);
        let mut s = RollupStats::default();
        b.observe(&event("ns", "user:luis", Some("p:1")), &mut s);
        b.observe(&event("ns", "user:luis", Some("p:2")), &mut s);
        b.observe(&event("ns", "user:ana", Some("p:1")), &mut s);
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn missing_object_bucket_is_empty_string() {
        let b = RollupBuffer::new(10);
        let mut s = RollupStats::default();
        b.observe(&event("ns", "u", None), &mut s);
        let counts = b.snapshot_counts();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].0.object, "");
    }

    #[tokio::test]
    async fn observe_and_maybe_summarize_calls_summarizer_on_trigger() {
        let b = RollupBuffer::new(2);
        let stats = Arc::new(parking_lot::Mutex::new(RollupStats::default()));
        let cap = CapturingSummarizer::new();
        observe_and_maybe_summarize(&b, &stats, &cap, &event("ns", "u", Some("p"))).await;
        observe_and_maybe_summarize(&b, &stats, &cap, &event("ns", "u", Some("p"))).await;
        let snap = cap.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "ns");
        assert_eq!(snap[0].1, "u");
        assert_eq!(snap[0].2, "p");
        assert_eq!(snap[0].3, 2, "batch size at trigger time");
    }

    #[tokio::test]
    async fn observe_and_maybe_summarize_noops_below_threshold() {
        let b = RollupBuffer::new(10);
        let stats = Arc::new(parking_lot::Mutex::new(RollupStats::default()));
        let cap = CapturingSummarizer::new();
        observe_and_maybe_summarize(&b, &stats, &cap, &event("ns", "u", Some("p"))).await;
        assert!(cap.snapshot().is_empty());
        assert_eq!(stats.lock().triggers_fired, 0);
    }

    #[test]
    fn forget_drops_specific_window() {
        let b = RollupBuffer::new(10);
        let mut s = RollupStats::default();
        b.observe(&event("ns", "u", Some("p")), &mut s);
        let key = RollupKey {
            namespace: "ns".into(),
            actor: "u".into(),
            object: "p".into(),
        };
        assert!(b.forget(&key));
        assert!(b.is_empty());
        assert!(!b.forget(&key));
    }
}
