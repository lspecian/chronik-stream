//! Task state-machine consumer materializing
//! `mem.task.current.{tenant}` (AM-2.1 follow-up).
//!
//! # What it does
//!
//! Reads new task transitions from `mem.task.{tenant}` (append-only event
//! log) and materializes a compacted "current state per task" view by
//! producing to `mem.task.current.{tenant}` with the record's `task_id`
//! as the Kafka key. The latest transition per `task_id` wins after log
//! compaction folds the topic.
//!
//! This is the read-model half of task tracking. Writers append every
//! transition to the audit log (`mem.task.{tenant}`); readers query the
//! compacted view (`mem.task.current.{tenant}`) for "what is my open
//! task list?" without scanning the full history.
//!
//! # Scope
//!
//! - **Non-Task records are ignored.** The subscriber pattern
//!   `mem.task.{tenant}` catches non-Task records only if a caller
//!   misuses the topic; we defensively skip them and count a
//!   `parse_errors` bump.
//! - **Cancelled + Done are still materialized** — they carry the final
//!   state and are useful for recall filters like "tasks completed
//!   this week". Callers that want only open work filter on
//!   `state=open` at recall time.
//! - **In-memory mirror** of the compacted view: [`TaskCurrentIndex`]
//!   maintains `(namespace, task_id) → TaskBody` so callers can query
//!   the current state without a Kafka round-trip. Recovery on restart
//!   rehydrates from beginning-of-log (topic is compacted → finite
//!   backlog).
//!
//! # Testability
//!
//! Parse + apply logic is pure. The rdkafka-touching function is
//! [`run_consumer`]; the rest is covered by the `tests` module below.

use crate::error::{MemoryError, Result};
use crate::schema::{Body, MemoryRecord, TaskBody, TaskState};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Errors from [`parse_task_record`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Kafka record key was not valid UTF-8.
    #[error("mem.task key not utf-8: {0}")]
    KeyNotUtf8(String),
    /// Value payload failed to decode as [`MemoryRecord`].
    #[error("mem.task value JSON decode failed: {0}")]
    BadJson(String),
    /// Record body was not a Task — the source topic is misused.
    #[error("mem.task record is not a Task body")]
    NotATask,
}

/// One decoded record from a `mem.task.*` topic.
#[derive(Debug, Clone)]
pub enum TaskEvent {
    /// A task transition landed. The full [`MemoryRecord`] envelope +
    /// its [`TaskBody`] are preserved so the consumer can update its
    /// index AND relay the record onward for materialization.
    Transition(MemoryRecord),
    /// A null-value tombstone was observed on the topic. Currently
    /// tasks don't get tombstoned individually — task lifecycle uses
    /// state=cancelled|done as terminal markers — so this event is
    /// counted but ignored.
    Tombstone { key: String },
}

/// Parse a `mem.task.{tenant}` record.
pub fn parse_task_record(key: &[u8], value: Option<&[u8]>) -> std::result::Result<TaskEvent, ParseError> {
    let key = std::str::from_utf8(key)
        .map_err(|e| ParseError::KeyNotUtf8(e.to_string()))?
        .to_string();
    match value {
        None => Ok(TaskEvent::Tombstone { key }),
        Some(v) if v.is_empty() => Ok(TaskEvent::Tombstone { key }),
        Some(v) => {
            let record: MemoryRecord = serde_json::from_slice(v)
                .map_err(|e| ParseError::BadJson(e.to_string()))?;
            match &record.body {
                Body::Task(_) => Ok(TaskEvent::Transition(record)),
                _ => Err(ParseError::NotATask),
            }
        }
    }
}

/// In-memory `(namespace, task_id) → TaskBody` mirror of
/// `mem.task.current.{tenant}`. Cheap to clone (`Arc` interior).
#[derive(Debug, Default, Clone)]
pub struct TaskCurrentIndex {
    inner: Arc<DashMap<TaskKey, TaskBody>>,
}

/// Composite key for the index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskKey {
    /// Full namespace path (drives per-tenant isolation).
    pub namespace: String,
    /// Stable task identifier.
    pub task_id: String,
}

impl TaskCurrentIndex {
    /// Fresh empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of `(namespace, task_id)` entries.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` if empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Insert / update the current state for a task. Latest wins
    /// unconditionally — the caller is responsible for ordering
    /// (Kafka consumer polls in log order).
    pub fn set(&self, namespace: &str, body: TaskBody) {
        let key = TaskKey {
            namespace: namespace.to_string(),
            task_id: body.task_id.clone(),
        };
        self.inner.insert(key, body);
    }

    /// Look up the current state for `(namespace, task_id)`.
    pub fn get(&self, namespace: &str, task_id: &str) -> Option<TaskBody> {
        let key = TaskKey {
            namespace: namespace.to_string(),
            task_id: task_id.to_string(),
        };
        self.inner.get(&key).map(|v| v.value().clone())
    }

    /// Snapshot every task with the given `state` in `namespace`.
    /// Sorted by `due_at` (unset = last) then `task_id`.
    pub fn list_by_state(&self, namespace: &str, state: TaskState) -> Vec<TaskBody> {
        let mut out: Vec<TaskBody> = self
            .inner
            .iter()
            .filter_map(|entry| {
                (entry.key().namespace == namespace && entry.value().state == state)
                    .then(|| entry.value().clone())
            })
            .collect();
        out.sort_by(|a, b| match (a.due_at, b.due_at) {
            (Some(x), Some(y)) => x.cmp(&y).then(a.task_id.cmp(&b.task_id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.task_id.cmp(&b.task_id),
        });
        out
    }

    /// Drop a task from the index (invoked by tombstones — currently a
    /// no-op path since tasks use terminal states, but kept for
    /// symmetry).
    pub fn forget(&self, namespace: &str, task_id: &str) -> bool {
        let key = TaskKey {
            namespace: namespace.to_string(),
            task_id: task_id.to_string(),
        };
        self.inner.remove(&key).is_some()
    }
}

/// Apply a [`TaskEvent`] to the shared index. Pure — no Kafka.
pub fn apply_event(index: &TaskCurrentIndex, event: &TaskEvent) -> TaskCurrentApply {
    match event {
        TaskEvent::Transition(record) => {
            if let Body::Task(body) = &record.body {
                index.set(&record.namespace, body.clone());
                TaskCurrentApply::Transitioned {
                    namespace: record.namespace.clone(),
                    task_id: body.task_id.clone(),
                    state: body.state,
                }
            } else {
                TaskCurrentApply::Skipped
            }
        }
        TaskEvent::Tombstone { .. } => TaskCurrentApply::Skipped,
    }
}

/// Outcome of [`apply_event`] — surfaced for stats + logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCurrentApply {
    /// A task was inserted / updated. Carries the namespace + task_id
    /// + new state so the caller can relay the state to observers.
    Transitioned {
        namespace: String,
        task_id: String,
        state: TaskState,
    },
    /// The event carried no relevant transition — ignored.
    Skipped,
}

/// Config for [`run_consumer`] / [`spawn`].
#[derive(Debug, Clone)]
pub struct TaskCurrentConfig {
    /// Kafka bootstrap servers.
    pub kafka_brokers: String,
    /// Consumer group. Multiple replicas can share it — rdkafka picks
    /// one active reader per partition.
    pub group_id: String,
    /// Topic to subscribe: `mem.task.{tenant}`. Callers wanting to
    /// watch every tenant should fan out one consumer per tenant.
    pub topic: String,
}

impl TaskCurrentConfig {
    /// Sensible defaults.
    pub fn new(kafka_brokers: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            kafka_brokers: kafka_brokers.into(),
            group_id: "chronik-memory-task-current-consumer".to_string(),
            topic: topic.into(),
        }
    }

    /// Override group id.
    pub fn with_group_id(mut self, id: impl Into<String>) -> Self {
        self.group_id = id.into();
        self
    }
}

/// Running counters. Snapshot-friendly.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskCurrentStats {
    /// Records the consumer parsed successfully.
    pub records_processed: u64,
    /// Transitions successfully applied to the index.
    pub transitions_applied: u64,
    /// Tombstones observed on the topic.
    pub tombstones_observed: u64,
    /// Records dropped because parse failed.
    pub parse_errors: u64,
    /// Records skipped because the body was not a Task.
    pub non_task_records: u64,
}

/// Spawn a background task that runs the task-current consumer
/// forever.
pub fn spawn(
    config: TaskCurrentConfig,
    index: TaskCurrentIndex,
    stats: Arc<parking_lot::Mutex<TaskCurrentStats>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match run_consumer(&config, &index, &stats).await {
                Ok(()) => {
                    info!("mem.task current consumer exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "mem.task current consumer errored, retrying in 5s"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// The rdkafka-backed consumer loop. Public so integration tests can
/// drive it directly.
pub async fn run_consumer(
    config: &TaskCurrentConfig,
    index: &TaskCurrentIndex,
    stats: &Arc<parking_lot::Mutex<TaskCurrentStats>>,
) -> Result<()> {
    use rdkafka::consumer::{Consumer, StreamConsumer};
    use rdkafka::message::Message;
    use rdkafka::ClientConfig;

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &config.kafka_brokers)
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "true")
        .set("auto.offset.reset", "earliest")
        .create()
        .map_err(|e| {
            MemoryError::Kafka(format!("task-current consumer: create client: {e}"))
        })?;

    consumer.subscribe(&[&config.topic]).map_err(|e| {
        MemoryError::Kafka(format!(
            "task-current consumer: subscribe {}: {e}",
            config.topic
        ))
    })?;

    info!(
        topic = %config.topic,
        group_id = %config.group_id,
        "task-current consumer: subscribed"
    );

    loop {
        let msg = consumer.recv().await.map_err(|e| {
            MemoryError::Kafka(format!("task-current consumer: recv: {e}"))
        })?;
        let key = msg.key().unwrap_or(&[]);
        let value = msg.payload();
        match parse_task_record(key, value) {
            Ok(event) => {
                let mut s = stats.lock();
                s.records_processed += 1;
                let outcome = apply_event(index, &event);
                match (&event, &outcome) {
                    (TaskEvent::Transition(_), TaskCurrentApply::Transitioned { .. }) => {
                        s.transitions_applied += 1;
                    }
                    (TaskEvent::Tombstone { .. }, _) => {
                        s.tombstones_observed += 1;
                    }
                    _ => s.non_task_records += 1,
                }
                debug!(?outcome, "task-current: event applied");
            }
            Err(e) => {
                let mut s = stats.lock();
                s.parse_errors += 1;
                warn!(error = %e, "task-current: parse failed, skipping");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, FactBody, MemoryRecord, Source, TaskBody, TaskState};
    use chrono::Utc;

    fn task_record(
        namespace: &str,
        task_id: &str,
        state: TaskState,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: format!("mem-{task_id}-{}", state as u8),
            tenant_id: namespace.split(':').next().unwrap_or(namespace).into(),
            namespace: namespace.into(),
            key: Some(task_id.into()),
            version: 1,
            created_at: now,
            valid_from: now,
            valid_to: None,
            confidence: 1.0,
            source: Source {
                topic: format!("mem.task.{}", namespace.split(':').next().unwrap_or("t")),
                offsets: vec![0],
                extractor: "test@1".into(),
            },
            tombstoned: false,
            body: Body::Task(TaskBody {
                task_id: task_id.into(),
                title: format!("task {task_id}"),
                state,
                due_at,
                owner: None,
                depends_on: Vec::new(),
            }),
        }
    }

    fn fact_record(namespace: &str) -> MemoryRecord {
        let now = Utc::now();
        MemoryRecord {
            memory_id: "mem-fact".into(),
            tenant_id: namespace.split(':').next().unwrap_or(namespace).into(),
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
    fn parse_happy_returns_transition() {
        let record = task_record("acme:agent:bot:user:luis", "t1", TaskState::Open, None);
        let bytes = serde_json::to_vec(&record).unwrap();
        let ev = parse_task_record(b"t1", Some(&bytes)).unwrap();
        match ev {
            TaskEvent::Transition(r) => assert_eq!(r.memory_id, record.memory_id),
            _ => panic!("expected Transition"),
        }
    }

    #[test]
    fn parse_null_value_is_tombstone() {
        let ev = parse_task_record(b"t1", None).unwrap();
        assert!(matches!(ev, TaskEvent::Tombstone { .. }));
    }

    #[test]
    fn parse_empty_value_is_tombstone() {
        let ev = parse_task_record(b"t1", Some(&[])).unwrap();
        assert!(matches!(ev, TaskEvent::Tombstone { .. }));
    }

    #[test]
    fn parse_bad_key_utf8_errors() {
        let err = parse_task_record(&[0xff, 0xfe], Some(b"{}")).unwrap_err();
        assert!(matches!(err, ParseError::KeyNotUtf8(_)));
    }

    #[test]
    fn parse_bad_json_errors() {
        let err = parse_task_record(b"t1", Some(b"not json")).unwrap_err();
        assert!(matches!(err, ParseError::BadJson(_)));
    }

    #[test]
    fn parse_non_task_body_rejected() {
        let record = fact_record("acme:agent:bot:user:luis");
        let bytes = serde_json::to_vec(&record).unwrap();
        let err = parse_task_record(b"u|p", Some(&bytes)).unwrap_err();
        assert!(matches!(err, ParseError::NotATask));
    }

    #[test]
    fn set_get_roundtrip() {
        let idx = TaskCurrentIndex::new();
        let r = task_record("ns", "t1", TaskState::Open, None);
        let Body::Task(body) = r.body else {
            unreachable!()
        };
        idx.set("ns", body.clone());
        let got = idx.get("ns", "t1").unwrap();
        assert_eq!(got.state, TaskState::Open);
    }

    #[test]
    fn set_overwrites_state_on_transition() {
        let idx = TaskCurrentIndex::new();
        let open = task_record("ns", "t1", TaskState::Open, None);
        let Body::Task(open_body) = open.body else {
            unreachable!()
        };
        idx.set("ns", open_body);
        let done = task_record("ns", "t1", TaskState::Done, None);
        let Body::Task(done_body) = done.body else {
            unreachable!()
        };
        idx.set("ns", done_body);
        assert_eq!(idx.get("ns", "t1").unwrap().state, TaskState::Done);
        assert_eq!(idx.len(), 1, "still one entry, latest wins");
    }

    #[test]
    fn list_by_state_filters_and_orders_by_due_at() {
        let idx = TaskCurrentIndex::new();
        let t1 = TaskBody {
            task_id: "t1".into(),
            title: "one".into(),
            state: TaskState::Open,
            due_at: Some("2026-08-01T09:00:00Z".parse().unwrap()),
            owner: None,
            depends_on: vec![],
        };
        let t2 = TaskBody {
            task_id: "t2".into(),
            title: "two".into(),
            state: TaskState::Open,
            due_at: Some("2026-07-01T09:00:00Z".parse().unwrap()),
            owner: None,
            depends_on: vec![],
        };
        let t3 = TaskBody {
            task_id: "t3".into(),
            title: "three".into(),
            state: TaskState::Done,
            due_at: None,
            owner: None,
            depends_on: vec![],
        };
        idx.set("ns", t1);
        idx.set("ns", t2);
        idx.set("ns", t3);
        let open = idx.list_by_state("ns", TaskState::Open);
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].task_id, "t2"); // earlier due wins
        assert_eq!(open[1].task_id, "t1");
        let done = idx.list_by_state("ns", TaskState::Done);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].task_id, "t3");
    }

    #[test]
    fn list_by_state_isolates_by_namespace() {
        let idx = TaskCurrentIndex::new();
        let a = TaskBody {
            task_id: "a".into(),
            title: "".into(),
            state: TaskState::Open,
            due_at: None,
            owner: None,
            depends_on: vec![],
        };
        let b = TaskBody {
            task_id: "b".into(),
            title: "".into(),
            state: TaskState::Open,
            due_at: None,
            owner: None,
            depends_on: vec![],
        };
        idx.set("acme:ns", a);
        idx.set("beta:ns", b);
        assert_eq!(idx.list_by_state("acme:ns", TaskState::Open).len(), 1);
        assert_eq!(idx.list_by_state("beta:ns", TaskState::Open).len(), 1);
        assert_eq!(idx.list_by_state("gamma:ns", TaskState::Open).len(), 0);
    }

    #[test]
    fn apply_transition_updates_index() {
        let idx = TaskCurrentIndex::new();
        let r = task_record("ns", "t1", TaskState::InProgress, None);
        let ev = TaskEvent::Transition(r);
        let out = apply_event(&idx, &ev);
        assert!(matches!(
            out,
            TaskCurrentApply::Transitioned { task_id, state, .. }
                if task_id == "t1" && state == TaskState::InProgress
        ));
        assert_eq!(idx.get("ns", "t1").unwrap().state, TaskState::InProgress);
    }

    #[test]
    fn apply_tombstone_is_skipped() {
        let idx = TaskCurrentIndex::new();
        let ev = TaskEvent::Tombstone {
            key: "t1".into(),
        };
        let out = apply_event(&idx, &ev);
        assert!(matches!(out, TaskCurrentApply::Skipped));
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn forget_drops_entry() {
        let idx = TaskCurrentIndex::new();
        let body = TaskBody {
            task_id: "t1".into(),
            title: "".into(),
            state: TaskState::Open,
            due_at: None,
            owner: None,
            depends_on: vec![],
        };
        idx.set("ns", body);
        assert!(idx.forget("ns", "t1"));
        assert!(!idx.forget("ns", "t1"), "second forget returns false");
        assert!(idx.get("ns", "t1").is_none());
    }

    #[test]
    fn config_new_and_with_group_id() {
        let c = TaskCurrentConfig::new("brokers", "mem.task.acme");
        assert_eq!(c.kafka_brokers, "brokers");
        assert_eq!(c.topic, "mem.task.acme");
        assert_eq!(c.group_id, "chronik-memory-task-current-consumer");
        let c2 = c.with_group_id("custom");
        assert_eq!(c2.group_id, "custom");
    }

    #[test]
    fn stats_default_is_zero() {
        let s = TaskCurrentStats::default();
        assert_eq!(s.records_processed, 0);
        assert_eq!(s.transitions_applied, 0);
        assert_eq!(s.tombstones_observed, 0);
        assert_eq!(s.parse_errors, 0);
        assert_eq!(s.non_task_records, 0);
    }
}
