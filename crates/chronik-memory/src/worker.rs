//! Background extraction worker (AMS-2.2).
//!
//! Long-running async task that consumes raw conversation turns from
//! `mem.raw.{ns}` via an rdkafka `StreamConsumer` consumer group, micro-batches
//! them, runs the configured [`Extractor`], and produces typed memories.
//!
//! # Semantics
//!
//! - **At-least-once + idempotent keys = effectively-once** for compactable
//!   types (fact / instruction / task). Events get at-least-once: a duplicate
//!   event after a crash is possible. Callers who care must dedup downstream.
//! - **Offset commits** happen only **after** successful produce of the typed
//!   memories. If the worker crashes mid-batch, the next start re-consumes the
//!   uncommitted batch and re-extracts. The compacted typed topics absorb the
//!   duplicate.
//! - **Backpressure**: not (yet) needed in the lib API — the consumer pulls
//!   one batch, processes it, then pulls the next. Producer awaits keep this
//!   loop in lockstep with downstream throughput. AMS-2.2 follow-up adds an
//!   explicit bounded mpsc when the worker grows multiple parallel processors.
//!
//! # Lifecycle
//!
//! ```no_run
//! # use chronik_memory::{Memory, worker::{Worker, WorkerConfig}};
//! # async fn run(mem: Memory) -> chronik_memory::Result<()> {
//! let worker = Worker::new(mem, WorkerConfig::default());
//! tokio::select! {
//!     _ = worker.run() => {},
//!     _ = tokio::signal::ctrl_c() => {
//!         tracing::info!("shutting down on Ctrl-C");
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use crate::client::Memory;
use crate::error::{MemoryError, Result};
use crate::extractor::Turn;
use crate::ingest::IngestAck;
use crate::remember::build_envelope;
use crate::schema::MemoryRecord;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::message::Message;
use rdkafka::topic_partition_list::TopicPartitionList;
use rdkafka::Offset;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::StreamExt;
use tracing::{debug, info, instrument, warn};

/// Tunable parameters for the worker.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Consumer group id. Defaults to `chronik-memory-extractor-{namespace-suffix}`
    /// constructed from the [`Memory`] client at worker construction time.
    pub group_id: Option<String>,
    /// Maximum messages per batch before forcing a flush.
    pub max_batch_size: usize,
    /// Maximum wall time per batch — flushes whatever is buffered when this
    /// elapses, even if the batch is below `max_batch_size`.
    pub max_batch_window: Duration,
    /// `auto.offset.reset` policy passed to rdkafka. `earliest` is the right
    /// default for an extraction worker — we want to extract from history when
    /// a new worker joins.
    pub auto_offset_reset: &'static str,
    /// Worker shuts down after this many consecutive empty polls. `None` = run forever.
    pub stop_after_idle_polls: Option<usize>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            group_id: None,
            max_batch_size: 200,
            max_batch_window: Duration::from_secs(5),
            auto_offset_reset: "earliest",
            stop_after_idle_polls: None,
        }
    }
}

/// Background extraction worker. Construct with [`Worker::new`], then call [`Worker::run`].
pub struct Worker {
    memory: Memory,
    config: WorkerConfig,
}

impl Worker {
    /// Build a new worker that consumes from `memory`'s raw topic and produces
    /// extracted memories back into the same namespace's typed topics.
    ///
    /// The [`Memory`] client must have an extractor configured — calling
    /// [`Worker::run`] otherwise returns `MemoryError::Config`.
    pub fn new(memory: Memory, config: WorkerConfig) -> Self {
        Self { memory, config }
    }

    fn group_id(&self) -> String {
        if let Some(g) = &self.config.group_id {
            return g.clone();
        }
        // Derive a stable group id from the namespace path so two replicas of
        // the same worker share a group and parallelize partitions.
        let safe = self
            .memory
            .namespace()
            .chars()
            .map(|c| if c == ':' { '.' } else { c })
            .collect::<String>();
        format!("chronik-memory-extractor.{safe}")
    }

    fn build_consumer(&self, brokers: &str) -> Result<StreamConsumer> {
        ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("group.id", self.group_id())
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", self.config.auto_offset_reset)
            .set("session.timeout.ms", "30000")
            .set("max.poll.interval.ms", "300000")
            .create::<StreamConsumer>()
            .map_err(|e| MemoryError::Kafka(format!("consumer init: {e}")))
    }

    /// Run the consume / extract / produce / commit loop.
    ///
    /// Returns when the consumer stream ends, when `stop_after_idle_polls` is
    /// hit, or when an unrecoverable error bubbles up. Cooperates with
    /// `tokio::select!` for caller-driven shutdown.
    #[instrument(skip(self), fields(namespace = %self.memory.namespace()))]
    pub async fn run(&self) -> Result<WorkerStats> {
        let extractor = self
            .memory
            .extractor()
            .cloned()
            .ok_or_else(|| {
                MemoryError::Config(
                    "Worker requires Memory builder to set .extractor(...)".into(),
                )
            })?;

        let raw_topic = self.memory.topic_layout().raw();
        let brokers = self.memory.kafka_brokers().to_string();
        let consumer = self.build_consumer(&brokers)?;
        consumer
            .subscribe(&[raw_topic.as_str()])
            .map_err(|e| MemoryError::Kafka(format!("subscribe {raw_topic}: {e}")))?;
        info!(group_id = %self.group_id(), topic = %raw_topic, "worker subscribed");

        let mut stats = WorkerStats::default();
        let mut idle_polls = 0usize;
        let mut stream = consumer.stream();

        loop {
            // Collect a micro-batch: up to max_batch_size messages, or until
            // max_batch_window elapses.
            let batch_deadline = Instant::now() + self.config.max_batch_window;
            let mut turns: Vec<Turn> = Vec::with_capacity(self.config.max_batch_size);
            let mut commit_offsets: Vec<(String, i32, i64)> = Vec::new();

            while turns.len() < self.config.max_batch_size {
                let remaining = batch_deadline.saturating_duration_since(Instant::now());
                let next = tokio::time::timeout(remaining, stream.next()).await;
                match next {
                    Ok(Some(Ok(m))) => {
                        if let Some(t) = decode_turn(&m) {
                            turns.push(t);
                        } else {
                            warn!(
                                topic = m.topic(),
                                offset = m.offset(),
                                "skipping un-decodable message in mem.raw.*"
                            );
                        }
                        commit_offsets.push((
                            m.topic().to_string(),
                            m.partition(),
                            m.offset(),
                        ));
                    }
                    Ok(Some(Err(e))) => {
                        return Err(MemoryError::Kafka(format!("consumer error: {e}")));
                    }
                    Ok(None) => {
                        // Stream ended (rare for StreamConsumer in normal flow).
                        return Ok(stats);
                    }
                    Err(_) => break, // window elapsed
                }
            }

            if turns.is_empty() {
                idle_polls = idle_polls.saturating_add(1);
                if let Some(max) = self.config.stop_after_idle_polls {
                    if idle_polls >= max {
                        info!(idle_polls, "worker stopping after idle threshold");
                        return Ok(stats);
                    }
                }
                continue;
            }
            idle_polls = 0;

            // Run extraction. Errors here are not auto-retried — log and skip
            // the batch; the offsets stay uncommitted, so a re-consume will try
            // again. (For systematic failures the caller will see no progress
            // and intervene.)
            stats.batches_processed += 1;
            stats.turns_consumed += turns.len() as u64;
            let extracted = match extractor.extract(&turns).await {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, batch_size = turns.len(), "extractor failed; not committing batch");
                    stats.extractor_errors += 1;
                    continue;
                }
            };

            // Produce each extracted memory; map batch indexes → committed
            // raw-topic offsets. We only have one offset list (commit_offsets)
            // since rdkafka returns messages in order; turn[i] corresponds to
            // commit_offsets[i].
            let extractor_id = extractor.id().to_string();
            for ex in &extracted {
                let raw_offsets: Vec<i64> = ex
                    .source_indexes
                    .iter()
                    .filter_map(|&i| commit_offsets.get(i).map(|(_, _, o)| *o))
                    .collect();
                if raw_offsets.is_empty() {
                    continue; // already filtered upstream, but defensive
                }

                let mut env: MemoryRecord = match build_envelope(
                    self.memory.tenant(),
                    self.memory.namespace(),
                    ex.body.clone(),
                    ex.key.clone(),
                    ex.confidence,
                    &extractor_id,
                    &raw_topic,
                ) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!(error = %e, "skipping extraction with invalid envelope");
                        continue;
                    }
                };
                env.source.offsets = raw_offsets;

                match produce_memory_via_client(&self.memory, &env).await {
                    Ok(_) => stats.memories_produced += 1,
                    Err(e) => {
                        warn!(error = %e, "produce failed; not committing batch");
                        stats.produce_errors += 1;
                        // Bail out of this batch's commit so it gets retried.
                        commit_offsets.clear();
                        break;
                    }
                }
            }

            // Commit offsets only if produce succeeded for the whole batch.
            if !commit_offsets.is_empty() {
                let mut tpl = TopicPartitionList::new();
                // Per-partition: highest offset + 1 (Kafka commit semantics).
                let mut max_per_partition: std::collections::HashMap<(String, i32), i64> =
                    std::collections::HashMap::new();
                for (t, p, o) in &commit_offsets {
                    let key = (t.clone(), *p);
                    let entry = max_per_partition.entry(key).or_insert(*o);
                    if *o > *entry {
                        *entry = *o;
                    }
                }
                for ((t, p), o) in &max_per_partition {
                    tpl.add_partition_offset(t, *p, Offset::Offset(o + 1))
                        .map_err(|e| MemoryError::Kafka(format!("tpl add: {e}")))?;
                }
                consumer
                    .commit(&tpl, CommitMode::Async)
                    .map_err(|e| MemoryError::Kafka(format!("commit: {e}")))?;
                stats.batches_committed += 1;
                debug!(?max_per_partition, "committed offsets");
            }
        }
    }
}

/// Cumulative counters for a worker run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerStats {
    /// Number of micro-batches the worker pulled from Kafka.
    pub batches_processed: u64,
    /// Number of micro-batches whose offsets were committed.
    pub batches_committed: u64,
    /// Total turns consumed (sum across batches).
    pub turns_consumed: u64,
    /// Total typed memories produced.
    pub memories_produced: u64,
    /// Number of batches that failed at the extractor step.
    pub extractor_errors: u64,
    /// Number of batches that failed during produce.
    pub produce_errors: u64,
}

/// Decode a `mem.raw.*` Kafka message into a [`Turn`]. The wire format is the
/// JSON written by [`crate::ingest::RawTurnRecord`].
fn decode_turn(m: &rdkafka::message::BorrowedMessage<'_>) -> Option<Turn> {
    let payload = m.payload()?;
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let role = v.get("role")?.as_str()?.to_string();
    let content = v.get("content")?.as_str()?.to_string();
    let ts = v
        .get("ts")
        .and_then(|t| t.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));
    let channel = v.get("channel").and_then(|c| c.as_str()).map(str::to_string);
    let external_id = v
        .get("external_id")
        .and_then(|e| e.as_str())
        .map(str::to_string);
    Some(Turn {
        role,
        content,
        ts,
        channel,
        external_id,
    })
}

/// Internal helper — produce a typed memory using the public Memory client
/// API. Kept as a free fn so the produce path is the same as `remember_record`.
async fn produce_memory_via_client(
    memory: &Memory,
    env: &MemoryRecord,
) -> Result<IngestAck> {
    memory.remember_record(env).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdkafka::message::OwnedMessage;

    /// Decode-from-message smoke test using a `BorrowedMessage` is awkward
    /// because rdkafka's BorrowedMessage isn't trivially constructable. We
    /// test the JSON-shape part via a separate helper that mirrors decode_turn.
    fn decode_turn_from_payload(payload: &[u8]) -> Option<Turn> {
        let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
        let role = v.get("role")?.as_str()?.to_string();
        let content = v.get("content")?.as_str()?.to_string();
        let ts = v
            .get("ts")
            .and_then(|t| t.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        Some(Turn {
            role,
            content,
            ts,
            channel: None,
            external_id: None,
        })
    }

    #[test]
    fn decode_turn_happy() {
        let p = serde_json::to_vec(&serde_json::json!({
            "role": "user",
            "content": "hello",
            "ts": "2026-04-25T10:00:00Z"
        }))
        .unwrap();
        let t = decode_turn_from_payload(&p).unwrap();
        assert_eq!(t.role, "user");
        assert_eq!(t.content, "hello");
        assert!(t.ts.is_some());
    }

    #[test]
    fn decode_turn_missing_required_returns_none() {
        let p = serde_json::to_vec(&serde_json::json!({"role": "user"})).unwrap();
        assert!(decode_turn_from_payload(&p).is_none());
        let p2 = b"not json";
        assert!(decode_turn_from_payload(p2).is_none());
    }

    #[test]
    fn worker_stats_default_is_zero() {
        let s = WorkerStats::default();
        assert_eq!(s.batches_processed, 0);
        assert_eq!(s.memories_produced, 0);
    }

    #[test]
    fn worker_config_default_is_phase2_spec() {
        let c = WorkerConfig::default();
        assert_eq!(c.max_batch_size, 200);
        assert_eq!(c.max_batch_window, Duration::from_secs(5));
        assert_eq!(c.auto_offset_reset, "earliest");
    }

    #[allow(dead_code)]
    fn _shut_unused_owned_message_warning(_: OwnedMessage) {}
}
