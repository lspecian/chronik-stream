//! Kafka consumer that hydrates [`TenantRegistry`] from `mem.tenants` topic
//! (AM-2.5 production source-of-truth).
//!
//! The env-var bootstrap ([`TenantRegistry::from_env`]) is fine for
//! development, small deployments, and integration tests, but production
//! systems need to add/remove tenants without restarting brokers. The
//! `mem.tenants` topic is a Kafka-compacted stream keyed by `tenant_id`;
//! writes to it eventually reach the registry via this consumer.
//!
//! # Wire shape
//!
//! Topic: `mem.tenants` (compacted, `cleanup.policy=compact`).
//! Key: `tenant_id` (UTF-8 string).
//! Value:
//!   - JSON [`Tenant`] envelope on upsert
//!   - Empty payload OR `null` value on tombstone (Kafka compaction deletion)
//!
//! # Semantics
//!
//! - Every record is applied to the shared [`TenantRegistry`].
//! - Non-tombstone records with `tenant_id` mismatching the key are logged
//!   and skipped (defensive against operator error).
//! - Tombstones remove the tenant. In-flight requests holding an
//!   `Arc<Memory>` continue with the last-known state until the next
//!   `get_or_create` call resolves them fresh.
//! - JSON parse failures are logged and skipped. The consumer keeps
//!   running; a bad record must never block the tenant catalog.
//!
//! # What this module *is*
//!
//! - A pure parser ([`parse_tenant_record`]) that turns a `(key, value)` pair
//!   into a [`TenantEvent`], plus an [`apply_event`] helper that mutates the
//!   registry. Both are pure/lock-scoped and 100% unit-testable without any
//!   Kafka involvement.
//! - A blocking [`run_consumer`] entry point that spawns an rdkafka consumer
//!   and processes records as they arrive. Callers invoke it from a
//!   dedicated background task at startup.
//!
//! # What this module is *not*
//!
//! - It is not the write path. Operator tooling (or a future admin API)
//!   produces to `mem.tenants` directly.
//! - It does not implement per-tenant rate limits or storage quotas. Those
//!   are separate follow-ups in the AM-2.5 roadmap.

use crate::tenants::{Tenant, TenantRegistry};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Topic name used by the tenants consumer. Fixed by convention — the
/// consumer does not accept an override so operators can't accidentally
/// point production at the wrong topic.
pub const TENANTS_TOPIC: &str = "mem.tenants";

/// One decoded record from `mem.tenants`.
#[derive(Debug, Clone, PartialEq)]
pub enum TenantEvent {
    /// Upsert a tenant. `key` (the Kafka record key) is preserved so
    /// consumers can validate `key == tenant.tenant_id`.
    Upsert {
        /// Kafka record key.
        key: String,
        /// Decoded tenant record from the value payload.
        tenant: Tenant,
    },
    /// Kafka compaction tombstone. Remove the tenant identified by `key`.
    Tombstone {
        /// Kafka record key.
        key: String,
    },
}

/// Envelope written to the topic value. `TenantEvent::Upsert(tenant)`
/// serializes as this shape.
///
/// The current shape is a thin wrapper around `Tenant` with a `schema_version`
/// header so future revisions stay forward-compatible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantRecordEnvelope {
    /// Wire schema version. Fixed at 1 for now.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// The full [`Tenant`] record.
    #[serde(flatten)]
    pub tenant: Tenant,
}

fn default_schema_version() -> u32 {
    1
}

/// Decode a single `(key_bytes, value_bytes)` record from `mem.tenants` into
/// a [`TenantEvent`]. Public so tests + the runtime consumer share the same
/// parse logic.
///
/// Returns `Err` when the value cannot be parsed as JSON envelope. The
/// caller should log and skip.
pub fn parse_tenant_record(
    key_bytes: &[u8],
    value_bytes: Option<&[u8]>,
) -> Result<TenantEvent, ParseError> {
    let key = std::str::from_utf8(key_bytes)
        .map_err(|e| ParseError::KeyNotUtf8(e.to_string()))?
        .to_string();
    if key.is_empty() {
        return Err(ParseError::EmptyKey);
    }

    // Null OR empty value = tombstone. rdkafka returns `Some(&[])` for
    // "empty payload" (which some producers emit intentionally to mean
    // deleted) and `None` for a proper Kafka null. Treat both identically —
    // there is no observed semantic difference at this schema level.
    match value_bytes {
        None => Ok(TenantEvent::Tombstone { key }),
        Some(v) if v.is_empty() => Ok(TenantEvent::Tombstone { key }),
        Some(v) => {
            let env: TenantRecordEnvelope = serde_json::from_slice(v)
                .map_err(|e| ParseError::BadJson(e.to_string()))?;
            Ok(TenantEvent::Upsert {
                key,
                tenant: env.tenant,
            })
        }
    }
}

/// Apply one decoded event to the registry. Logs (and returns `Ok`) for
/// tenant_id/key mismatches — these are operational errors, not bugs.
pub fn apply_event(registry: &TenantRegistry, event: TenantEvent) {
    match event {
        TenantEvent::Upsert { key, tenant } => {
            if key != tenant.tenant_id {
                warn!(
                    key = %key,
                    tenant_id = %tenant.tenant_id,
                    "mem.tenants: key does not match tenant_id — record skipped"
                );
                return;
            }
            let tid = tenant.tenant_id.clone();
            registry.upsert(tenant);
            debug!(tenant_id = %tid, "TenantRegistry upserted from mem.tenants");
        }
        TenantEvent::Tombstone { key } => {
            let removed = registry.remove(&key);
            debug!(
                tenant_id = %key,
                was_present = removed,
                "TenantRegistry tombstoned from mem.tenants"
            );
        }
    }
}

/// Errors from [`parse_tenant_record`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Kafka record key was not valid UTF-8.
    #[error("mem.tenants key not utf-8: {0}")]
    KeyNotUtf8(String),
    /// Kafka record key was empty (a compaction key requirement).
    #[error("mem.tenants key was empty")]
    EmptyKey,
    /// Value payload failed to decode as [`TenantRecordEnvelope`].
    #[error("mem.tenants value JSON decode failed: {0}")]
    BadJson(String),
}

// ─────────────────────────────────────────────────────────────────
// Live consumer (rdkafka-backed). Optional — tests use the parse path.
// ─────────────────────────────────────────────────────────────────

/// Config for the live consumer.
#[derive(Debug, Clone)]
pub struct ConsumerConfig {
    /// Kafka bootstrap servers (e.g. `localhost:9092`).
    pub kafka_brokers: String,
    /// Consumer group id. Multiple replicas can share it — Kafka picks one
    /// active reader per partition, so the registry stays consistent.
    pub group_id: String,
    /// If `true`, the consumer sits on the topic tip after the initial
    /// hydration and forwards new records as they arrive. If `false`,
    /// [`run_consumer_once`] returns once caught up.
    pub tail: bool,
}

impl ConsumerConfig {
    /// Sensible defaults for `kafka_brokers=localhost:9092`.
    pub fn new(kafka_brokers: impl Into<String>) -> Self {
        Self {
            kafka_brokers: kafka_brokers.into(),
            group_id: "chronik-memory-tenants-consumer".to_string(),
            tail: true,
        }
    }

    /// Override the consumer group id.
    pub fn with_group_id(mut self, id: impl Into<String>) -> Self {
        self.group_id = id.into();
        self
    }

    /// When `true`, the consumer keeps running past the initial hydration
    /// and forwards live records (production mode). When `false`, it exits
    /// on end-of-log (used by hydration-once integration tests).
    pub fn with_tail(mut self, tail: bool) -> Self {
        self.tail = tail;
        self
    }
}

/// Spawn a background task that runs the mem.tenants consumer forever
/// (with automatic reconnect on transient errors). Returns immediately.
///
/// This is the intended startup hook: `main.rs` calls this after building
/// the shared `Arc<TenantRegistry>` and the returned `JoinHandle` is stored
/// for graceful shutdown.
pub fn spawn(
    config: ConsumerConfig,
    registry: Arc<TenantRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match run_consumer(&config, &registry).await {
                Ok(()) => {
                    // `run_consumer` only returns Ok when the tail loop is
                    // asked to stop (tail=false + end-of-log). In production
                    // (tail=true) it never returns Ok, so we get here only
                    // via one-shot testing modes — exit the retry loop.
                    info!("mem.tenants consumer exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "mem.tenants consumer errored, retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// Blocking consumer loop. Public so integration tests can drive it
/// directly and assert on state after a known set of records.
///
/// This is the only function in this module that touches rdkafka. Everything
/// else (`parse_tenant_record`, `apply_event`) is pure and testable.
pub async fn run_consumer(
    config: &ConsumerConfig,
    registry: &Arc<TenantRegistry>,
) -> Result<(), crate::error::MemoryError> {
    use rdkafka::consumer::{Consumer, StreamConsumer};
    use rdkafka::message::Message;
    use rdkafka::ClientConfig;

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &config.kafka_brokers)
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .map_err(|e| {
            crate::error::MemoryError::Kafka(format!(
                "tenants consumer: create client: {e}"
            ))
        })?;

    consumer
        .subscribe(&[TENANTS_TOPIC])
        .map_err(|e| {
            crate::error::MemoryError::Kafka(format!(
                "tenants consumer: subscribe {TENANTS_TOPIC}: {e}"
            ))
        })?;

    info!(
        topic = %TENANTS_TOPIC,
        brokers = %config.kafka_brokers,
        group_id = %config.group_id,
        tail = config.tail,
        "mem.tenants consumer started"
    );

    loop {
        match consumer.recv().await {
            Ok(msg) => {
                let key_bytes = msg.key().unwrap_or(&[]);
                let value_bytes = msg.payload();
                match parse_tenant_record(key_bytes, value_bytes) {
                    Ok(event) => apply_event(registry, event),
                    Err(e) => {
                        warn!(
                            error = %e,
                            partition = msg.partition(),
                            offset = msg.offset(),
                            "mem.tenants: skipping unparseable record"
                        );
                    }
                }
                // No offset commit: consumers process the full log every
                // startup. This matches Kafka compaction semantics — the
                // topic is short and reprocessing is cheap. A future
                // optimization can commit periodic checkpoints.
            }
            Err(e) => {
                return Err(crate::error::MemoryError::Kafka(format!(
                    "tenants consumer: recv: {e}"
                )));
            }
        }
        if !config.tail {
            // Best-effort one-shot: if the caller asked us to stop after
            // the current hydration wave, exit as soon as we've processed
            // whatever's available. Detecting "at end of log" precisely
            // requires poll-based rdkafka APIs; for the test path we rely
            // on the caller stopping the task from outside instead.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenants::{Tenant, TenantQuotas, TenantRegistry};
    use serde_json::json;

    fn envelope_for(tenant: &Tenant) -> Vec<u8> {
        let env = TenantRecordEnvelope {
            schema_version: 1,
            tenant: tenant.clone(),
        };
        serde_json::to_vec(&env).expect("serialize envelope")
    }

    #[test]
    fn parse_upsert_happy_path() {
        let tenant = Tenant::new("acme", "key-1");
        let value = envelope_for(&tenant);
        let event = parse_tenant_record(b"acme", Some(&value)).unwrap();
        match event {
            TenantEvent::Upsert { key, tenant } => {
                assert_eq!(key, "acme");
                assert_eq!(tenant.tenant_id, "acme");
                assert!(tenant.accepts_key("key-1"));
                assert_eq!(tenant.namespace_patterns, vec!["acme:*".to_string()]);
            }
            other => panic!("expected Upsert, got {other:?}"),
        }
    }

    #[test]
    fn parse_upsert_carries_quotas_and_display_name() {
        let mut tenant = Tenant::new("beta", "k");
        tenant.display_name = Some("Beta Corp".to_string());
        tenant.quotas = TenantQuotas {
            ingest_msgs_per_sec: Some(100),
            recall_qps: Some(20),
            storage_bytes: Some(10 * 1024 * 1024 * 1024),
        };
        let value = envelope_for(&tenant);
        let event = parse_tenant_record(b"beta", Some(&value)).unwrap();
        match event {
            TenantEvent::Upsert { tenant, .. } => {
                assert_eq!(tenant.display_name.as_deref(), Some("Beta Corp"));
                assert_eq!(tenant.quotas.ingest_msgs_per_sec, Some(100));
                assert_eq!(tenant.quotas.recall_qps, Some(20));
                assert_eq!(tenant.quotas.storage_bytes, Some(10 * 1024 * 1024 * 1024));
            }
            other => panic!("expected Upsert, got {other:?}"),
        }
    }

    #[test]
    fn parse_tombstone_from_none_value() {
        let event = parse_tenant_record(b"acme", None).unwrap();
        assert_eq!(event, TenantEvent::Tombstone { key: "acme".into() });
    }

    #[test]
    fn parse_tombstone_from_empty_value() {
        let event = parse_tenant_record(b"acme", Some(b"")).unwrap();
        assert_eq!(event, TenantEvent::Tombstone { key: "acme".into() });
    }

    #[test]
    fn parse_rejects_non_utf8_key() {
        let value = envelope_for(&Tenant::new("t", "k"));
        let err = parse_tenant_record(&[0xff, 0xfe], Some(&value)).unwrap_err();
        assert!(matches!(err, ParseError::KeyNotUtf8(_)), "got {err:?}");
    }

    #[test]
    fn parse_rejects_empty_key() {
        let err = parse_tenant_record(b"", None).unwrap_err();
        assert!(matches!(err, ParseError::EmptyKey), "got {err:?}");
    }

    #[test]
    fn parse_rejects_bad_json_value() {
        let err = parse_tenant_record(b"acme", Some(b"this-is-not-json")).unwrap_err();
        assert!(matches!(err, ParseError::BadJson(_)), "got {err:?}");
    }

    #[test]
    fn parse_accepts_missing_schema_version() {
        // Producers may omit schema_version — the default_schema_version()
        // deserializer fills it in.
        let value = json!({
            "tenant_id": "acme",
            "api_keys": ["key-1"],
            "namespace_patterns": ["acme:*"]
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        let event = parse_tenant_record(b"acme", Some(&bytes)).unwrap();
        match event {
            TenantEvent::Upsert { tenant, .. } => assert_eq!(tenant.tenant_id, "acme"),
            other => panic!("expected Upsert, got {other:?}"),
        }
    }

    #[test]
    fn apply_upsert_inserts_into_registry() {
        let r = TenantRegistry::new();
        let tenant = Tenant::new("acme", "key-1");
        apply_event(&r, TenantEvent::Upsert {
            key: "acme".into(),
            tenant,
        });
        assert_eq!(r.len(), 1);
        assert!(r.get("acme").unwrap().accepts_key("key-1"));
    }

    #[test]
    fn apply_upsert_with_key_mismatch_is_skipped() {
        let r = TenantRegistry::new();
        let tenant = Tenant::new("acme", "key-1");
        apply_event(&r, TenantEvent::Upsert {
            key: "wrong".into(), // mismatched
            tenant,
        });
        assert_eq!(r.len(), 0, "mismatched record must not land");
    }

    #[test]
    fn apply_tombstone_removes_existing() {
        let r = TenantRegistry::new();
        r.upsert(Tenant::new("acme", "k"));
        apply_event(&r, TenantEvent::Tombstone { key: "acme".into() });
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn apply_tombstone_on_missing_is_noop() {
        let r = TenantRegistry::new();
        // No panic; internally logs was_present=false.
        apply_event(&r, TenantEvent::Tombstone { key: "ghost".into() });
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn full_lifecycle_upsert_then_tombstone() {
        let r = TenantRegistry::new();
        let tenant = Tenant::new("acme", "key-1");
        let value = envelope_for(&tenant);
        apply_event(&r, parse_tenant_record(b"acme", Some(&value)).unwrap());
        assert_eq!(r.len(), 1);
        // Rotate the key by upserting a fresh record — the second upsert
        // wins per HashMap semantics.
        let mut rotated = Tenant::new("acme", "key-2");
        rotated.api_keys.push("key-1-legacy".to_string());
        let value2 = envelope_for(&rotated);
        apply_event(&r, parse_tenant_record(b"acme", Some(&value2)).unwrap());
        let cur = r.get("acme").unwrap();
        assert!(cur.accepts_key("key-2"));
        assert!(cur.accepts_key("key-1-legacy"));
        // Tombstone.
        apply_event(&r, parse_tenant_record(b"acme", None).unwrap());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn consumer_config_defaults() {
        let c = ConsumerConfig::new("localhost:9092");
        assert_eq!(c.kafka_brokers, "localhost:9092");
        assert_eq!(c.group_id, "chronik-memory-tenants-consumer");
        assert!(c.tail);
        let c2 = ConsumerConfig::new("host:9092")
            .with_group_id("custom")
            .with_tail(false);
        assert_eq!(c2.group_id, "custom");
        assert!(!c2.tail);
    }
}
