//! Micro-batcher for the hot vector path.
//!
//! Collects `(topic, partition, offset, text)` tuples from the produce path
//! and flushes them to an embedding provider in batches, on either a
//! batch-size or time-window trigger. The embedded vectors are then added
//! to the `HotVectorIndex` for near-real-time ANN search.
//!
//! See `docs/ROADMAP_HOT_PATH.md` (HP-2.2).
//!
//! # Overflow semantics
//!
//! On a sustained overflow (embedder can't keep up with produce), the
//! bounded tokio mpsc channel's `try_send` drops the **new** entry rather
//! than the oldest. The roadmap originally called for drop-oldest, but
//! drop-newest gives the same steady-state behavior (queue stays full with
//! the items already in it) and is ~10× simpler in async Rust. Under burst,
//! this means the first items in the burst survive and later ones are
//! dropped. The cold path still catches every record from WAL, so the only
//! user-visible effect is a transient freshness regression on the dropped
//! offsets until WalIndexer commits them to cold HNSW.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chronik_embeddings::provider::EmbeddingProvider;
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, trace, warn};

use crate::hot_vector_index::HotVectorIndex;

/// Per-provider defaults for the batcher (HP-2.8).
#[derive(Debug, Clone)]
pub struct MicroBatcherConfig {
    /// Flush when this many items have been collected.
    pub batch_size: usize,
    /// Flush when this long has elapsed since the last flush (even if the
    /// batch is smaller than `batch_size`).
    pub window: Duration,
    /// Queue capacity. When full, new `enqueue` calls drop the new entry.
    pub queue_size: usize,
}

impl MicroBatcherConfig {
    /// Defaults for a local HTTP embedder. Small batch + short window keeps
    /// latency low because the embedder's network cost is negligible.
    pub fn local_defaults() -> Self {
        Self {
            batch_size: 32,
            window: Duration::from_millis(20),
            queue_size: 10_000,
        }
    }

    /// Defaults for OpenAI. Larger batch + longer window to amortize the
    /// network round-trip and stay within per-tier RPM limits.
    pub fn openai_defaults() -> Self {
        Self {
            batch_size: 256,
            window: Duration::from_millis(200),
            queue_size: 20_000,
        }
    }

    /// Read env-var overrides on top of the provider defaults.
    pub fn with_env_overrides(mut self, provider_kind: ProviderKind) -> Self {
        let suffix = match provider_kind {
            ProviderKind::Local => "LOCAL",
            ProviderKind::OpenAi => "OPENAI",
        };
        if let Some(v) = std::env::var(format!("CHRONIK_HOT_VECTOR_BATCH_SIZE_{}", suffix))
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            self.batch_size = v;
        }
        if let Some(v) = std::env::var(format!("CHRONIK_HOT_VECTOR_BATCH_WINDOW_MS_{}", suffix))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            self.window = Duration::from_millis(v);
        }
        if let Some(v) = std::env::var("CHRONIK_HOT_VECTOR_QUEUE_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            self.queue_size = v;
        }
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ProviderKind {
    Local,
    OpenAi,
}

/// Item handed from the produce-path hook to the batcher.
#[derive(Debug)]
pub struct PendingEmbed {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub text: String,
    /// HP-3 Item 2: record's produce timestamp (ms since epoch). Sampled at
    /// flush time to report freshness (produce → visible-in-hot-vector).
    pub produce_timestamp_ms: i64,
}

/// Shared counters for observability.
#[derive(Debug, Default)]
pub struct MicroBatcherMetrics {
    pub enqueued: AtomicU64,
    pub dropped_overflow: AtomicU64,
    pub batches_flushed: AtomicU64,
    pub embedder_errors: AtomicU64,
    pub vectors_added: AtomicU64,
}

/// Handle for enqueueing items into the batcher. Clonable — the produce
/// path holds one, the worker holds the receiver.
#[derive(Clone)]
pub struct MicroBatcherHandle {
    tx: mpsc::Sender<PendingEmbed>,
    metrics: Arc<MicroBatcherMetrics>,
}

impl MicroBatcherHandle {
    /// Non-blocking enqueue. Returns immediately:
    /// - `true` if the item was queued
    /// - `false` if the queue was full (item dropped; caller may log)
    pub fn enqueue(&self, item: PendingEmbed) -> bool {
        match self.tx.try_send(item) {
            Ok(()) => {
                self.metrics.enqueued.fetch_add(1, Ordering::Relaxed);
                chronik_monitoring::MetricsRecorder::record_hot_vector_enqueue(false);
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                // HP-3.2: sampled WARN on sustained overflow so operators
                // see drops in logs, not just in the dropped_overflow metric.
                // Log the first drop and every 1000th thereafter.
                let dropped = self.metrics
                    .dropped_overflow
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                chronik_monitoring::MetricsRecorder::record_hot_vector_enqueue(true);
                if dropped == 1 || dropped % 1000 == 0 {
                    warn!(
                        hot_path = "vector",
                        event = "queue_overflow",
                        dropped_total = dropped,
                        "hot vector batcher queue full — embedder can't keep up (cold path will still embed from WAL)"
                    );
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub fn metrics(&self) -> &Arc<MicroBatcherMetrics> {
        &self.metrics
    }
}

/// Start the batcher: spawns a background task that consumes from the
/// channel and flushes to the embedder. Returns the producer handle for
/// the produce path and the JoinHandle of the worker.
pub fn start(
    config: MicroBatcherConfig,
    embedder: Arc<dyn EmbeddingProvider>,
    index: Arc<HotVectorIndex>,
) -> (MicroBatcherHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<PendingEmbed>(config.queue_size);
    let metrics = Arc::new(MicroBatcherMetrics::default());
    let handle = MicroBatcherHandle {
        tx,
        metrics: Arc::clone(&metrics),
    };
    let worker = tokio::spawn(run_worker(rx, config, embedder, index, metrics));
    (handle, worker)
}

async fn run_worker(
    mut rx: mpsc::Receiver<PendingEmbed>,
    config: MicroBatcherConfig,
    embedder: Arc<dyn EmbeddingProvider>,
    index: Arc<HotVectorIndex>,
    metrics: Arc<MicroBatcherMetrics>,
) {
    let mut buf: Vec<PendingEmbed> = Vec::with_capacity(config.batch_size);
    let mut ticker = interval(config.window);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    debug!(
        batch_size = config.batch_size,
        window_ms = config.window.as_millis() as u64,
        queue = config.queue_size,
        provider = embedder.name(),
        "hot vector batcher started"
    );

    loop {
        tokio::select! {
            maybe_item = rx.recv() => {
                match maybe_item {
                    Some(item) => {
                        buf.push(item);
                        if buf.len() >= config.batch_size {
                            flush(&mut buf, embedder.as_ref(), index.as_ref(), &metrics).await;
                        }
                    }
                    None => {
                        // Sender dropped — flush remaining and exit.
                        if !buf.is_empty() {
                            flush(&mut buf, embedder.as_ref(), index.as_ref(), &metrics).await;
                        }
                        debug!("hot vector batcher worker exiting (channel closed)");
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                if !buf.is_empty() {
                    flush(&mut buf, embedder.as_ref(), index.as_ref(), &metrics).await;
                }
            }
        }
    }
}

/// HP Gap 1: Truncate a UTF-8 string at a char boundary so it fits under
/// `max_bytes`. We don't tokenize (that would require a tokenizer dep per
/// provider); instead we use a conservative 2 bytes/token ratio, which
/// handles English (~4 bytes/token) and multilingual text (~2 bytes/token).
fn truncate_to_byte_budget(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

async fn flush(
    buf: &mut Vec<PendingEmbed>,
    embedder: &dyn EmbeddingProvider,
    index: &HotVectorIndex,
    metrics: &Arc<MicroBatcherMetrics>,
) {
    if buf.is_empty() {
        return;
    }
    let batch = std::mem::take(buf);
    // HP Gap 1: cap each text at the embedder's declared input budget.
    // Conservative 2-bytes-per-token ratio covers both English and
    // multilingual payloads. Oversize inputs that would otherwise cause
    // the embedder to reject the entire batch are truncated instead.
    let max_bytes = embedder.max_input_tokens().saturating_mul(2);
    let texts: Vec<&str> = batch
        .iter()
        .map(|p| truncate_to_byte_budget(p.text.as_str(), max_bytes))
        .collect();

    let start = Instant::now();
    let result = embedder.embed_batch(&texts).await;
    let elapsed = start.elapsed();

    metrics.batches_flushed.fetch_add(1, Ordering::Relaxed);

    let embeddings = match result {
        Ok(b) => b,
        Err(e) => {
            metrics.embedder_errors.fetch_add(1, Ordering::Relaxed);
            chronik_monitoring::MetricsRecorder::record_hot_vector_flush(false, 0);
            // HP-3 Session-7 minor-gap fix: record the failed request + its
            // (estimated-zero) tokens so the global embedding counters see
            // the hot path's activity. Without this, operators only see
            // cold-pipeline embedding stats and the hot path is invisible.
            chronik_monitoring::MetricsRecorder::record_embedding_request(
                false,
                batch.len() as u64,
                0,
                0,
            );
            warn!(
                hot_path = "vector",
                event = "embedder_error",
                provider = embedder.name(),
                batch = batch.len(),
                elapsed_ms = elapsed.as_millis() as u64,
                "hot vector embed_batch failed: {}",
                e
            );
            // Drop the batch. Cold path will re-embed from WAL; only freshness
            // for these offsets is affected (temporary).
            return;
        }
    };

    // HP-3 Session-7 minor-gap fix: tokens used by the hot path go into
    // the global `chronik_embedding_tokens_total` counter, same shape as
    // the cold pipeline. `total_tokens` is `None` for providers that don't
    // report usage (e.g. local models); pass 0 in that case.
    let tokens_used = embeddings.total_tokens.unwrap_or(0);
    chronik_monitoring::MetricsRecorder::record_embedding_request(
        true,
        batch.len() as u64,
        embeddings.embeddings.len() as u64,
        tokens_used as u64,
    );

    if embeddings.embeddings.len() != batch.len() {
        warn!(
            expected = batch.len(),
            got = embeddings.embeddings.len(),
            "hot vector: embedder returned wrong-size batch; dropping"
        );
        return;
    }

    let mut added_count = 0u64;
    // HP-3 Item 2: compute "now" once per flush for consistent lag samples.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    for (pending, embedding) in batch.into_iter().zip(embeddings.embeddings.into_iter()) {
        let produce_ts = pending.produce_timestamp_ms;
        if let Err(e) = index
            .add_vector(
                &pending.topic,
                pending.partition,
                pending.offset,
                embedding.vector,
            )
            .await
        {
            trace!(
                topic = %pending.topic,
                partition = pending.partition,
                offset = pending.offset,
                "hot vector add_vector failed: {}", e
            );
        } else {
            metrics.vectors_added.fetch_add(1, Ordering::Relaxed);
            added_count += 1;
            // HP-3 Item 2: record visibility lag for this newly-added vector.
            if produce_ts > 0 {
                let lag = (now_ms - produce_ts).max(0) as u64;
                chronik_monitoring::MetricsRecorder::record_hot_vector_visibility_lag(lag);
            }
        }
    }
    chronik_monitoring::MetricsRecorder::record_hot_vector_flush(true, added_count);

    trace!(
        elapsed_ms = elapsed.as_millis() as u64,
        "hot vector flush complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hot_vector_index::{HotDistanceMetric, HotVectorConfig};
    use async_trait::async_trait;
    use chronik_embeddings::provider::{
        EmbeddingBatch, EmbeddingProvider, EmbeddingResult,
    };
    use std::sync::atomic::AtomicUsize;

    /// Deterministic fake provider — maps each character of the input into
    /// one dimension of a fixed-length vector. Good enough to verify the
    /// batcher plumbing without an external service.
    struct FakeEmbedder {
        dims: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl EmbeddingProvider for FakeEmbedder {
        async fn embed(&self, text: &str) -> anyhow::Result<EmbeddingResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut v = vec![0.0_f32; self.dims];
            for (i, ch) in text.chars().take(self.dims).enumerate() {
                v[i] = ch as u32 as f32 / 1000.0;
            }
            Ok(EmbeddingResult {
                vector: v,
                text: text.to_string(),
                token_count: Some(text.len()),
            })
        }

        async fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<EmbeddingBatch> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut results = Vec::with_capacity(texts.len());
            for t in texts {
                let mut v = vec![0.0_f32; self.dims];
                for (i, ch) in t.chars().take(self.dims).enumerate() {
                    v[i] = ch as u32 as f32 / 1000.0;
                }
                results.push(EmbeddingResult {
                    vector: v,
                    text: t.to_string(),
                    token_count: Some(t.len()),
                });
            }
            Ok(EmbeddingBatch::new(results, Duration::from_millis(1)))
        }

        fn dimensions(&self) -> usize {
            self.dims
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "fake-model"
        }
    }

    fn index(dims: usize) -> Arc<HotVectorIndex> {
        Arc::new(HotVectorIndex::new(HotVectorConfig {
            max_vectors_per_partition: 1000,
            dimensions: dims,
            metric: HotDistanceMetric::Cosine,
        }))
    }

    #[tokio::test]
    async fn batcher_flushes_on_batch_size() {
        let idx = index(4);
        let embedder = Arc::new(FakeEmbedder {
            dims: 4,
            calls: AtomicUsize::new(0),
        });
        let config = MicroBatcherConfig {
            batch_size: 3,
            window: Duration::from_secs(10), // large enough it won't fire
            queue_size: 100,
        };
        let (h, _worker) = start(config, embedder.clone(), Arc::clone(&idx));

        for i in 0..3 {
            assert!(h.enqueue(PendingEmbed {
                topic: "t".into(),
                partition: 0,
                offset: i,
                text: format!("msg-{}", i), produce_timestamp_ms: 0,
            }));
        }

        for _ in 0..50 {
            if idx.vector_count("t", 0).await == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(idx.vector_count("t", 0).await, 3);
        assert!(embedder.calls.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn batcher_flushes_on_window() {
        let idx = index(4);
        let embedder = Arc::new(FakeEmbedder {
            dims: 4,
            calls: AtomicUsize::new(0),
        });
        let config = MicroBatcherConfig {
            batch_size: 1000,
            window: Duration::from_millis(50),
            queue_size: 100,
        };
        let (h, _worker) = start(config, embedder.clone(), Arc::clone(&idx));

        for i in 0..2 {
            assert!(h.enqueue(PendingEmbed {
                topic: "t".into(),
                partition: 0,
                offset: i,
                text: format!("msg-{}", i), produce_timestamp_ms: 0,
            }));
        }

        // Wait for the window tick.
        for _ in 0..50 {
            if idx.vector_count("t", 0).await == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(idx.vector_count("t", 0).await, 2);
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // Multi-byte UTF-8 at the cut point — must not split a code point.
        let s = "abc\u{1F600}xyz"; // "abc😀xyz", 😀 = 4 bytes
        // Truncate at byte=5 (middle of the emoji).
        let out = truncate_to_byte_budget(s, 5);
        assert!(out.chars().all(|c| c != '\u{FFFD}'));
        // The emoji is truncated, only "abc" remains.
        assert_eq!(out, "abc");

        // Below budget: unchanged.
        let short = "hello";
        assert_eq!(truncate_to_byte_budget(short, 100), "hello");

        // Exactly at budget.
        assert_eq!(truncate_to_byte_budget("hello", 5), "hello");
    }

    /// HP-3.3: Verify that when the embedder returns an error, the batch
    /// is dropped (cold path will re-embed from WAL), no vectors are added,
    /// and the error counter increments.
    #[tokio::test]
    async fn embedder_error_is_non_fatal() {
        use chronik_embeddings::provider::EmbeddingBatch;

        struct FailingEmbedder;
        #[async_trait]
        impl EmbeddingProvider for FailingEmbedder {
            async fn embed(&self, _text: &str) -> anyhow::Result<EmbeddingResult> {
                anyhow::bail!("embedder down")
            }
            async fn embed_batch(&self, _texts: &[&str]) -> anyhow::Result<EmbeddingBatch> {
                anyhow::bail!("embedder down")
            }
            fn dimensions(&self) -> usize {
                4
            }
            fn name(&self) -> &str {
                "failing"
            }
            fn model(&self) -> &str {
                "fake-model"
            }
        }

        let idx = index(4);
        let embedder = Arc::new(FailingEmbedder);
        let config = MicroBatcherConfig {
            batch_size: 2,
            window: Duration::from_secs(10),
            queue_size: 100,
        };
        let (h, _worker) = start(config, embedder, Arc::clone(&idx));
        h.enqueue(PendingEmbed {
            topic: "t".into(),
            partition: 0,
            offset: 0,
            text: "a".into(), produce_timestamp_ms: 0,
        });
        h.enqueue(PendingEmbed {
            topic: "t".into(),
            partition: 0,
            offset: 1,
            text: "b".into(), produce_timestamp_ms: 0,
        });
        // Give the worker a chance to flush.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // No vectors should have been added.
        assert_eq!(idx.vector_count("t", 0).await, 0);
        // Error counter incremented.
        assert!(h.metrics().embedder_errors.load(Ordering::Relaxed) >= 1);
        // Batcher is still alive — subsequent sends are accepted.
        assert!(h.enqueue(PendingEmbed {
            topic: "t".into(),
            partition: 0,
            offset: 2,
            text: "c".into(), produce_timestamp_ms: 0,
        }));
    }

    #[tokio::test]
    async fn overflow_increments_drop_counter() {
        let idx = index(4);
        let embedder = Arc::new(FakeEmbedder {
            dims: 4,
            calls: AtomicUsize::new(0),
        });
        let config = MicroBatcherConfig {
            batch_size: 1000,
            window: Duration::from_secs(60), // worker won't drain
            queue_size: 2,
        };
        let (h, _worker) = start(config, embedder.clone(), Arc::clone(&idx));

        // The two enqueues fill the channel.
        assert!(h.enqueue(PendingEmbed {
            topic: "t".into(), partition: 0, offset: 0, text: "a".into(), produce_timestamp_ms: 0,
        }));
        assert!(h.enqueue(PendingEmbed {
            topic: "t".into(), partition: 0, offset: 1, text: "b".into(), produce_timestamp_ms: 0,
        }));
        // Subsequent enqueues are dropped.
        let dropped1 = !h.enqueue(PendingEmbed {
            topic: "t".into(), partition: 0, offset: 2, text: "c".into(), produce_timestamp_ms: 0,
        });
        let dropped2 = !h.enqueue(PendingEmbed {
            topic: "t".into(), partition: 0, offset: 3, text: "d".into(), produce_timestamp_ms: 0,
        });
        assert!(dropped1 || dropped2, "at least one should overflow");
        assert!(h.metrics().dropped_overflow.load(Ordering::Relaxed) >= 1);
    }
}
