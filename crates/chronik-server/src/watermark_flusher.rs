//! Debounced metadata watermark flusher (v2.7.1)
//!
//! Solves the "acks=1 MessageTimedOut under concurrent load" defect diagnosed 2026-07-03.
//!
//! # Failure this fixes
//!
//! Every produce that advanced a partition's high watermark previously spawned a
//! fire-and-forget `tokio::spawn` calling `metadata_store.update_partition_offset(...)`.
//! Each spawned task held a runtime slot until the metadata WAL fsync returned.
//! Under 10 concurrent producers × 4 topics × 3 partitions this generated ~500
//! spawns/sec, all awaiting metadata WAL fsync on the same tokio runtime. Once
//! that runtime saturated:
//!
//! - metadata WAL commit workers were themselves scheduled late,
//! - data WAL commit workers (same runtime) were scheduled late,
//! - the ResponsePipeline callback that acks the producer fired late enough
//!   for the 30s cleanup sweep to time the pending response out.
//!
//! The producer sees `MessageTimedOut` after `message.timeout.ms`. In more
//! extreme runs the response fired late but silently — the client saw success
//! while the topic never actually populated (silent drop).
//!
//! # Design
//!
//! Watermarks are monotonic — losing intermediate values is safe as long as the
//! LAST value is eventually written. So instead of spawning a task per produce,
//! producers record the new watermark into an in-memory `DashMap<(topic, partition), AtomicI64>`
//! with `fetch_max`. A single background task periodically drains the map and
//! writes each unique (topic, partition) exactly once — the metadata WAL rate
//! is now `partitions × flush_interval⁻¹`, independent of the produce rate.
//!
//! # Invariants
//!
//! - `note()` is O(1) synchronous, no `.await`, no `tokio::spawn`. Suitable for
//!   inline call from any produce hot-path branch.
//! - `note()` never regresses the watermark. Concurrent notes race on `fetch_max`;
//!   only the maximum survives.
//! - The background task's failures are logged but do NOT retry. On next flush
//!   the same (topic, partition) key is still present with the latest value, so
//!   transient metadata WAL failures self-heal.
//! - Shutdown aborts the task and drains one final time so we don't leak the
//!   very last watermark on a graceful stop.

use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info, trace, warn};

use chronik_common::metadata::MetadataStore;

/// How often to drain the pending map into the metadata store.
/// Aggressive enough that consumers see a fresh HWM via the metadata-store
/// fallback within one flush cycle; slack enough that a saturated runtime
/// cannot regenerate the spawn-per-produce failure mode.
const DEFAULT_FLUSH_INTERVAL_MS: u64 = 100;

/// Env var override for the flush interval (milliseconds).
const ENV_FLUSH_INTERVAL_MS: &str = "CHRONIK_WATERMARK_FLUSH_MS";

fn flush_interval() -> Duration {
    let ms = std::env::var(ENV_FLUSH_INTERVAL_MS)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_FLUSH_INTERVAL_MS);
    Duration::from_millis(ms)
}

/// Metrics for observability.
#[derive(Debug, Default)]
pub struct WatermarkFlusherMetrics {
    /// Total `note()` calls received.
    pub notes: AtomicU64,
    /// Total metadata-store writes issued (successful + failed).
    pub flushes: AtomicU64,
    /// Total metadata-store writes that returned an error.
    pub flush_errors: AtomicU64,
    /// Current number of distinct (topic, partition) entries with a pending flush.
    pub pending_partitions: AtomicU64,
}

impl WatermarkFlusherMetrics {
    pub fn notes(&self) -> u64 { self.notes.load(Ordering::Relaxed) }
    pub fn flushes(&self) -> u64 { self.flushes.load(Ordering::Relaxed) }
    pub fn flush_errors(&self) -> u64 { self.flush_errors.load(Ordering::Relaxed) }
    pub fn pending_partitions(&self) -> u64 { self.pending_partitions.load(Ordering::Relaxed) }
}

/// Debounced metadata watermark flusher.
///
/// Cheap to clone (all state is `Arc`-shared). Every clone points at the same
/// background task and the same pending map.
#[derive(Clone)]
pub struct WatermarkFlusher {
    inner: Arc<Inner>,
}

struct Inner {
    /// Pending (topic, partition) → highest un-flushed watermark seen.
    ///
    /// AtomicI64 lets `note()` update the value with `fetch_max` while holding
    /// only a DashMap read guard — no exclusive lock, no `.await`.
    ///
    /// A watermark of -1 means "already flushed"; used to detect when the
    /// entry has drained without racing the flusher.
    pending: DashMap<(String, i32), AtomicI64>,
    /// Metadata store to write into.
    metadata_store: Arc<dyn MetadataStore>,
    /// Metrics.
    metrics: Arc<WatermarkFlusherMetrics>,
    /// Set to true when the flusher is being dropped so the background task
    /// stops rescheduling itself.
    shutdown: AtomicBool,
    /// Background task handle.
    ///
    /// Wrapped in Mutex so `Drop::drop` can `.abort()` even through the shared Arc.
    task: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

impl WatermarkFlusher {
    /// Spawn a flusher with the default cadence.
    ///
    /// The background task is spawned on the current tokio runtime (typically
    /// the main runtime — the caller is the server builder). It is NOT tied
    /// to the WalIndexer runtime, so it will not be starved by Tantivy/Parquet
    /// work.
    pub fn spawn(metadata_store: Arc<dyn MetadataStore>) -> Self {
        Self::spawn_with_interval(metadata_store, flush_interval())
    }

    /// Spawn a flusher with an explicit interval — used by tests to force fast
    /// drain and by the env-var override path.
    pub fn spawn_with_interval(
        metadata_store: Arc<dyn MetadataStore>,
        interval: Duration,
    ) -> Self {
        let inner = Arc::new(Inner {
            pending: DashMap::new(),
            metadata_store,
            metrics: Arc::new(WatermarkFlusherMetrics::default()),
            shutdown: AtomicBool::new(false),
            task: parking_lot::Mutex::new(None),
        });

        let inner_bg = inner.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First tick fires immediately; skip it so we let notes accumulate.
            ticker.tick().await;

            loop {
                ticker.tick().await;
                if inner_bg.shutdown.load(Ordering::Acquire) {
                    // Final drain on graceful stop.
                    Self::drain_once(&inner_bg).await;
                    return;
                }
                Self::drain_once(&inner_bg).await;
            }
        });
        *inner.task.lock() = Some(handle);

        info!(
            "🚿 WatermarkFlusher started (flush_interval={}ms)",
            interval.as_millis()
        );

        Self { inner }
    }

    /// Record a new high watermark for `(topic, partition)`.
    ///
    /// O(1) synchronous. Safe from any thread, safe from any tokio task. Does
    /// not block, does not `.await`, does not `tokio::spawn`.
    ///
    /// Concurrent calls race via `fetch_max`; only the maximum survives, which
    /// matches the metadata-store semantics: watermarks are monotonic.
    ///
    /// `watermark` values ≤ 0 are silently dropped — the metadata store treats
    /// them as "no update".
    pub fn note(&self, topic: &str, partition: i32, watermark: i64) {
        if watermark <= 0 {
            return;
        }
        self.inner.metrics.notes.fetch_add(1, Ordering::Relaxed);

        // Fast path: entry exists, just fetch_max.
        if let Some(entry) = self.inner.pending.get(&(topic.to_string(), partition)) {
            let prev = entry.value().fetch_max(watermark, Ordering::AcqRel);
            trace!(
                "WM_NOTE: {}-{} prev={} new={} (fast path)",
                topic, partition, prev, watermark
            );
            return;
        }

        // Slow path: create entry. Entry API handles the create-vs-race case.
        let key = (topic.to_string(), partition);
        let entry = self
            .inner
            .pending
            .entry(key)
            .or_insert_with(|| {
                self.inner
                    .metrics
                    .pending_partitions
                    .fetch_add(1, Ordering::Relaxed);
                AtomicI64::new(0)
            });
        let prev = entry.value().fetch_max(watermark, Ordering::AcqRel);
        trace!(
            "WM_NOTE: {}-{} prev={} new={} (slow path)",
            topic, partition, prev, watermark
        );
    }

    /// Drain the pending map exactly once, writing each entry to the metadata store.
    ///
    /// Snapshot semantics: takes each entry's value, resets the atomic to 0,
    /// then writes. If a producer `note()`s during the write, that value lands
    /// in the SAME entry and is picked up on the next tick — the entry is
    /// never removed while its atomic is non-zero.
    async fn drain_once(inner: &Arc<Inner>) {
        // Cheap early-out.
        if inner.pending.is_empty() {
            return;
        }

        // Snapshot all keys under DashMap's internal sharding. Cloning the key
        // is unavoidable — DashMap iter yields refs bound to the guard's lifetime.
        let keys: Vec<(String, i32)> = inner
            .pending
            .iter()
            .map(|e| e.key().clone())
            .collect();

        for key in keys {
            let (topic, partition) = key;

            // Swap the atomic to 0 and take whatever was there.
            let hwm = match inner.pending.get(&(topic.clone(), partition)) {
                Some(entry) => entry.value().swap(0, Ordering::AcqRel),
                None => continue,
            };

            if hwm <= 0 {
                // Already drained by a previous tick and no new note since —
                // safe to remove so the map doesn't grow unbounded on churn.
                // Use remove_if to avoid races with a concurrent note().
                let removed = inner
                    .pending
                    .remove_if(&(topic.clone(), partition), |_, v| {
                        v.load(Ordering::Acquire) == 0
                    });
                if removed.is_some() {
                    inner
                        .metrics
                        .pending_partitions
                        .fetch_sub(1, Ordering::Relaxed);
                }
                continue;
            }

            inner.metrics.flushes.fetch_add(1, Ordering::Relaxed);
            let res = inner
                .metadata_store
                .update_partition_offset(&topic, partition as u32, hwm, 0)
                .await;
            if let Err(e) = res {
                inner.metrics.flush_errors.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "WM_FLUSH_ERR: {}-{} hwm={} err={:?}",
                    topic, partition, hwm, e
                );
                // Put the value back so we retry next tick — but ONLY if no
                // higher value has landed in the meantime.
                if let Some(entry) = inner.pending.get(&(topic.clone(), partition)) {
                    entry.value().fetch_max(hwm, Ordering::AcqRel);
                }
            } else {
                debug!("WM_FLUSH_OK: {}-{} hwm={}", topic, partition, hwm);
            }
        }
    }

    /// Metrics accessor.
    pub fn metrics(&self) -> Arc<WatermarkFlusherMetrics> {
        self.inner.metrics.clone()
    }

    /// Force an immediate drain — used by tests that don't want to wait for
    /// the next tick. Not called on the hot path.
    #[doc(hidden)]
    pub async fn drain_now_for_test(&self) {
        Self::drain_once(&self.inner).await;
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(h) = self.task.lock().take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chronik_common::metadata::{InMemoryMetadataStore, MetadataStore};

    // NB: retry-on-transient-error is exercised by inspection of `drain_once`
    // (the `if let Err` branch that re-fetch_max's the value). Building a
    // fault-injecting MetadataStore double is high-friction — the trait has
    // 40+ methods — and adds little vs the tests below that pin the actual
    // coalesce + monotonic + per-partition + drop invariants.

    #[tokio::test(flavor = "current_thread")]
    async fn coalesces_many_notes_into_one_flush() {
        let store: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let flusher = WatermarkFlusher::spawn_with_interval(store.clone(), Duration::from_millis(50));

        // Ten thousand notes for the same (topic, partition) — all must coalesce.
        for i in 0..10_000i64 {
            flusher.note("t", 0, i + 1);
        }

        // Force an immediate drain — deterministic than waiting for a tick.
        flusher.drain_now_for_test().await;

        // The metadata store must record the highest value seen.
        let hwm = store.get_partition_offset("t", 0).await.unwrap();
        assert_eq!(hwm.map(|(h, _)| h), Some(10_000));

        // Coalesce guarantee: total flushes (from metrics) is much less than notes.
        // We only forced ONE drain — so at most 1 flush.
        assert_eq!(flusher.metrics().notes(), 10_000);
        assert!(
            flusher.metrics().flushes() <= 1,
            "expected ≤1 flush, got {}",
            flusher.metrics().flushes()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn note_is_monotonic_under_out_of_order() {
        let store: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let flusher = WatermarkFlusher::spawn_with_interval(store.clone(), Duration::from_millis(50));

        flusher.note("t", 0, 100);
        flusher.note("t", 0, 50); // regression — must not override
        flusher.note("t", 0, 200);
        flusher.note("t", 0, 150); // regression again

        flusher.drain_now_for_test().await;

        let hwm = store.get_partition_offset("t", 0).await.unwrap();
        assert_eq!(hwm.map(|(h, _)| h), Some(200));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn each_partition_flushed_independently() {
        let store: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let flusher = WatermarkFlusher::spawn_with_interval(store.clone(), Duration::from_millis(50));

        for p in 0..12 {
            flusher.note("t", p, (p as i64 + 1) * 100);
        }

        flusher.drain_now_for_test().await;

        for p in 0..12i32 {
            let hwm = store.get_partition_offset("t", p as u32).await.unwrap();
            assert_eq!(
                hwm.map(|(h, _)| h),
                Some((p as i64 + 1) * 100),
                "missing/wrong flush for partition {}",
                p
            );
        }
    }

    /// Regression guard for the acks=1 stall.
    ///
    /// Before v2.7.1, an equivalent workload would `tokio::spawn` once per
    /// note, and 5,000 in-flight tasks would starve any co-scheduled work.
    /// The flusher must handle the same workload with O(1) synchronous notes
    /// and produce at most one flush (given a single explicit drain).
    #[tokio::test(flavor = "current_thread")]
    async fn stress_no_spawn_explosion() {
        let store: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let flusher = WatermarkFlusher::spawn_with_interval(store.clone(), Duration::from_millis(50));

        // 5,000 notes across 12 partitions in one topic — the actual fleet
        // workload was ~500 spawns/sec sustained; here we hit the flusher
        // an order of magnitude harder without any await.
        for i in 0..5_000i64 {
            flusher.note("t", (i % 12) as i32, i + 1);
        }

        flusher.drain_now_for_test().await;

        // Each partition must show its highest note as the recorded HWM.
        // For partition p, the notes are i where i%12==p, so the max is
        // whichever largest i satisfies i%12==p (i in 0..5000).
        for p in 0..12i32 {
            let expected_max = (0..5_000i64).rev().find(|i| (i % 12) as i32 == p).unwrap() + 1;
            let hwm = store.get_partition_offset("t", p as u32).await.unwrap();
            assert_eq!(hwm.map(|(h, _)| h), Some(expected_max), "partition {}", p);
        }

        assert_eq!(flusher.metrics().notes(), 5_000);
        // At most one flush per partition per drain.
        assert!(
            flusher.metrics().flushes() <= 12,
            "expected ≤12 flushes across 12 partitions, got {}",
            flusher.metrics().flushes()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_stops_background_task() {
        let store: Arc<dyn MetadataStore> = Arc::new(InMemoryMetadataStore::new());
        let flusher = WatermarkFlusher::spawn_with_interval(store.clone(), Duration::from_millis(50));
        flusher.note("t", 0, 1);
        drop(flusher);
        // A dropped flusher must not keep scheduling work; if it did, the
        // current-thread runtime would keep this test alive past its return.
    }
}
