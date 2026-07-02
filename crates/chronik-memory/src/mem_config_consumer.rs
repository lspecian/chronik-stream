//! Per-namespace `mem.config.{tenant}` Kafka consumer (AM-2.3).
//!
//! Watches a compacted topic for tenant-specific overrides to the decay
//! half-lives that [`crate::ranking::half_life`] otherwise hard-codes. This
//! lets operators tune decay per tenant (e.g. "give us longer fact memory
//! but shorter task memory") without redeploying the server.
//!
//! # Wire shape
//!
//! Topic:  `mem.config.{tenant}` (cleanup.policy=compact)
//! Key:    config-key string. Currently:
//!         `half_life.fact` / `half_life.event` / `half_life.instruction`
//!         / `half_life.task` / `half_life.concept`
//! Value:  JSON `MemConfigRecordEnvelope { schema_version: 1, value: JsonValue }`.
//!         Empty/null value = compaction tombstone (revert to default).
//!
//! Half-life value payloads look like:
//!
//! ```json
//! { "schema_version": 1, "value": { "half_life_secs": 604800 } }
//! ```
//!
//! # Where the config gets consulted
//!
//! [`MemConfig::half_life`] takes precedence over
//! [`crate::ranking::half_life`]. The recall path threads a
//! `MemConfig` through its scoring so tenant overrides land end-to-end;
//! that wiring is a small follow-up. This commit lands the consumer +
//! store so operators can start producing configs today.

use crate::error::MemoryError;
use crate::schema::MemoryType;
use chrono::Duration;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Topic prefix. The full topic name is `mem.config.{tenant}`.
pub const CONFIG_TOPIC_PREFIX: &str = "mem.config.";

/// Topic-regex used to subscribe to every tenant's `mem.config.*` topic.
pub const CONFIG_TOPICS_REGEX: &str = r"^mem\.config\.[^.]+$";

/// One decoded record from `mem.config.{tenant}`.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigEvent {
    /// Set an override for `(tenant, key)`.
    Set {
        /// Tenant id (derived from the topic name).
        tenant: String,
        /// Config key (Kafka record key).
        key: String,
        /// Decoded payload.
        value: ConfigValue,
    },
    /// Compaction tombstone — revert to the default.
    Unset {
        /// Tenant id (derived from the topic name).
        tenant: String,
        /// Config key.
        key: String,
    },
}

/// Envelope written to the topic value. Kept forward-compatible via
/// `schema_version`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemConfigRecordEnvelope {
    /// Wire schema version. Fixed at 1 for now.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Decoded typed value.
    pub value: ConfigValue,
}

fn default_schema_version() -> u32 {
    1
}

/// Typed config payload. Additional variants land as future config keys
/// arrive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ConfigValue {
    /// `{ "half_life_secs": N }` — override the memory-type half-life.
    HalfLife {
        /// Half-life in whole seconds.
        half_life_secs: i64,
    },
    /// Fallback bucket for unknown / future config shapes. The consumer
    /// applies these only when the key resolves to a matching handler.
    Other(serde_json::Value),
}

/// Errors from [`parse_config_record`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Kafka record key was not valid UTF-8.
    #[error("mem.config key not utf-8: {0}")]
    KeyNotUtf8(String),
    /// Kafka record key was empty.
    #[error("mem.config key was empty")]
    EmptyKey,
    /// Value payload failed to decode.
    #[error("mem.config value JSON decode failed: {0}")]
    BadJson(String),
    /// Topic name didn't look like `mem.config.{tenant}`.
    #[error("mem.config topic {0:?} does not match `mem.config.<tenant>`")]
    BadTopic(String),
}

/// Extract the tenant name from a `mem.config.<tenant>` topic string.
pub fn tenant_from_topic(topic: &str) -> Result<&str, ParseError> {
    topic
        .strip_prefix(CONFIG_TOPIC_PREFIX)
        .filter(|t| !t.is_empty() && !t.contains('.'))
        .ok_or_else(|| ParseError::BadTopic(topic.to_string()))
}

/// Decode one `(topic, key_bytes, value_bytes)` record from a
/// `mem.config.*` topic into a [`ConfigEvent`].
pub fn parse_config_record(
    topic: &str,
    key_bytes: &[u8],
    value_bytes: Option<&[u8]>,
) -> Result<ConfigEvent, ParseError> {
    let tenant = tenant_from_topic(topic)?.to_string();
    let key = std::str::from_utf8(key_bytes)
        .map_err(|e| ParseError::KeyNotUtf8(e.to_string()))?
        .to_string();
    if key.is_empty() {
        return Err(ParseError::EmptyKey);
    }
    match value_bytes {
        None => Ok(ConfigEvent::Unset { tenant, key }),
        Some(v) if v.is_empty() => Ok(ConfigEvent::Unset { tenant, key }),
        Some(v) => {
            let env: MemConfigRecordEnvelope = serde_json::from_slice(v)
                .map_err(|e| ParseError::BadJson(e.to_string()))?;
            Ok(ConfigEvent::Set {
                tenant,
                key,
                value: env.value,
            })
        }
    }
}

/// Shared config store. Cheap to clone (`Arc` interior).
///
/// Currently only half-life overrides are supported; the store is a
/// `DashMap<(tenant, MemoryType), Duration>`. Other config keys land in a
/// parallel bag if / when they're introduced.
#[derive(Debug, Default, Clone)]
pub struct MemConfig {
    half_lives: Arc<DashMap<HalfLifeKey, Duration>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HalfLifeKey {
    tenant: String,
    kind: MemoryType,
}

impl MemConfig {
    /// Fresh empty config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of `(tenant, kind)` half-life overrides currently set.
    pub fn len(&self) -> usize {
        self.half_lives.len()
    }

    /// `true` if no overrides are set.
    pub fn is_empty(&self) -> bool {
        self.half_lives.is_empty()
    }

    /// Resolve the effective half-life for `(tenant, kind)`. Returns the
    /// override when set; otherwise falls back to
    /// [`crate::ranking::half_life`].
    pub fn half_life(&self, tenant: &str, kind: MemoryType) -> Duration {
        let key = HalfLifeKey {
            tenant: tenant.to_string(),
            kind,
        };
        self.half_lives
            .get(&key)
            .map(|v| *v.value())
            .unwrap_or_else(|| crate::ranking::half_life(kind))
    }

    /// Set an override.
    pub fn set_half_life(&self, tenant: &str, kind: MemoryType, half_life: Duration) {
        let key = HalfLifeKey {
            tenant: tenant.to_string(),
            kind,
        };
        self.half_lives.insert(key, half_life);
    }

    /// Remove an override (reverts to default). Returns whether one was set.
    pub fn unset_half_life(&self, tenant: &str, kind: MemoryType) -> bool {
        let key = HalfLifeKey {
            tenant: tenant.to_string(),
            kind,
        };
        self.half_lives.remove(&key).is_some()
    }

    /// Forget every override owned by `tenant` (tombstone integration).
    pub fn forget_tenant(&self, tenant: &str) {
        self.half_lives.retain(|k, _| k.tenant != tenant);
    }
}

/// Parse the `half_life.<type>` key suffix into a [`MemoryType`].
fn memory_type_from_key(key: &str) -> Option<MemoryType> {
    let suffix = key.strip_prefix("half_life.")?;
    match suffix {
        "fact" => Some(MemoryType::Fact),
        "event" => Some(MemoryType::Event),
        "instruction" => Some(MemoryType::Instruction),
        "task" => Some(MemoryType::Task),
        "concept" => Some(MemoryType::Concept),
        _ => None,
    }
}

/// Apply one decoded event to the shared config. Logs (and returns `Ok`)
/// when the event references an unknown key so future clients don't crash
/// older servers.
pub fn apply_event(config: &MemConfig, event: ConfigEvent) {
    match event {
        ConfigEvent::Set { tenant, key, value } => {
            if let Some(kind) = memory_type_from_key(&key) {
                if let ConfigValue::HalfLife { half_life_secs } = value {
                    if half_life_secs > 0 {
                        let d = Duration::seconds(half_life_secs);
                        config.set_half_life(&tenant, kind, d);
                        debug!(
                            tenant = %tenant,
                            ?kind,
                            secs = half_life_secs,
                            "mem.config: set half-life override"
                        );
                    } else {
                        warn!(
                            tenant = %tenant,
                            ?kind,
                            secs = half_life_secs,
                            "mem.config: ignoring non-positive half-life override"
                        );
                    }
                    return;
                }
            }
            warn!(
                tenant = %tenant,
                key = %key,
                "mem.config: skipping record with unknown key or value shape"
            );
        }
        ConfigEvent::Unset { tenant, key } => {
            if let Some(kind) = memory_type_from_key(&key) {
                let removed = config.unset_half_life(&tenant, kind);
                debug!(
                    tenant = %tenant,
                    ?kind,
                    was_present = removed,
                    "mem.config: unset half-life override"
                );
                return;
            }
            debug!(
                tenant = %tenant,
                key = %key,
                "mem.config: tombstone for unknown key — ignored"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Live consumer.
// ─────────────────────────────────────────────────────────────────

/// Config for [`run_consumer`] / [`spawn`].
#[derive(Debug, Clone)]
pub struct MemConfigConsumerConfig {
    /// Kafka bootstrap servers.
    pub kafka_brokers: String,
    /// Consumer group id.
    pub group_id: String,
}

impl MemConfigConsumerConfig {
    /// Sensible defaults.
    pub fn new(kafka_brokers: impl Into<String>) -> Self {
        Self {
            kafka_brokers: kafka_brokers.into(),
            group_id: "chronik-memory-config-consumer".to_string(),
        }
    }

    /// Override consumer group id.
    pub fn with_group_id(mut self, id: impl Into<String>) -> Self {
        self.group_id = id.into();
        self
    }
}

/// Spawn a background task that runs the mem.config consumer forever.
pub fn spawn(
    config: MemConfigConsumerConfig,
    mem_config: MemConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match run_consumer(&config, &mem_config).await {
                Ok(()) => {
                    info!("mem.config consumer exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "mem.config consumer errored, retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// The rdkafka-backed consumer loop. Subscribes to every tenant's
/// `mem.config.*` topic via [`CONFIG_TOPICS_REGEX`].
pub async fn run_consumer(
    config: &MemConfigConsumerConfig,
    mem_config: &MemConfig,
) -> Result<(), MemoryError> {
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
            MemoryError::Kafka(format!("mem.config consumer: create client: {e}"))
        })?;

    consumer.subscribe(&[CONFIG_TOPICS_REGEX]).map_err(|e| {
        MemoryError::Kafka(format!(
            "mem.config consumer: subscribe {CONFIG_TOPICS_REGEX}: {e}"
        ))
    })?;

    info!(
        pattern = %CONFIG_TOPICS_REGEX,
        brokers = %config.kafka_brokers,
        group_id = %config.group_id,
        "mem.config consumer started"
    );

    loop {
        match consumer.recv().await {
            Ok(msg) => {
                let topic = msg.topic();
                let key_bytes = msg.key().unwrap_or(&[]);
                let value_bytes = msg.payload();
                match parse_config_record(topic, key_bytes, value_bytes) {
                    Ok(event) => apply_event(mem_config, event),
                    Err(e) => warn!(
                        error = %e,
                        topic = %topic,
                        partition = msg.partition(),
                        offset = msg.offset(),
                        "mem.config: skipping unparseable record"
                    ),
                }
            }
            Err(e) => {
                return Err(MemoryError::Kafka(format!(
                    "mem.config consumer: recv: {e}"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_from_topic_ok() {
        assert_eq!(tenant_from_topic("mem.config.acme").unwrap(), "acme");
        assert_eq!(tenant_from_topic("mem.config.beta-corp").unwrap(), "beta-corp");
    }

    #[test]
    fn tenant_from_topic_rejects_bad_shapes() {
        assert!(tenant_from_topic("mem.config.").is_err());
        assert!(tenant_from_topic("mem.config").is_err());
        assert!(tenant_from_topic("mem.raw.acme").is_err());
        // Dotted tenant is rejected (we can't tell a tenant name with dots
        // from a subpath).
        assert!(tenant_from_topic("mem.config.acme.extra").is_err());
    }

    #[test]
    fn parse_set_half_life_happy_path() {
        let payload = serde_json::to_vec(&MemConfigRecordEnvelope {
            schema_version: 1,
            value: ConfigValue::HalfLife {
                half_life_secs: 604_800,
            },
        })
        .unwrap();
        let event =
            parse_config_record("mem.config.acme", b"half_life.fact", Some(&payload)).unwrap();
        match event {
            ConfigEvent::Set { tenant, key, value } => {
                assert_eq!(tenant, "acme");
                assert_eq!(key, "half_life.fact");
                assert_eq!(
                    value,
                    ConfigValue::HalfLife {
                        half_life_secs: 604_800
                    }
                );
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn parse_unset_via_null_value() {
        let event =
            parse_config_record("mem.config.acme", b"half_life.fact", None).unwrap();
        assert!(matches!(event, ConfigEvent::Unset { .. }));
    }

    #[test]
    fn parse_unset_via_empty_value() {
        let event =
            parse_config_record("mem.config.acme", b"half_life.fact", Some(b"")).unwrap();
        assert!(matches!(event, ConfigEvent::Unset { .. }));
    }

    #[test]
    fn parse_rejects_non_utf8_key() {
        let err =
            parse_config_record("mem.config.acme", &[0xff], None).unwrap_err();
        assert!(matches!(err, ParseError::KeyNotUtf8(_)));
    }

    #[test]
    fn parse_rejects_empty_key() {
        let err = parse_config_record("mem.config.acme", b"", None).unwrap_err();
        assert!(matches!(err, ParseError::EmptyKey));
    }

    #[test]
    fn parse_rejects_bad_topic() {
        let err = parse_config_record("mem.raw.acme", b"k", None).unwrap_err();
        assert!(matches!(err, ParseError::BadTopic(_)));
    }

    #[test]
    fn parse_rejects_bad_json_value() {
        let err = parse_config_record(
            "mem.config.acme",
            b"half_life.fact",
            Some(b"not-json"),
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::BadJson(_)));
    }

    #[test]
    fn memory_type_from_key_maps_all_variants() {
        assert_eq!(memory_type_from_key("half_life.fact"), Some(MemoryType::Fact));
        assert_eq!(memory_type_from_key("half_life.event"), Some(MemoryType::Event));
        assert_eq!(
            memory_type_from_key("half_life.instruction"),
            Some(MemoryType::Instruction)
        );
        assert_eq!(memory_type_from_key("half_life.task"), Some(MemoryType::Task));
        assert_eq!(memory_type_from_key("half_life.concept"), Some(MemoryType::Concept));
        assert_eq!(memory_type_from_key("half_life.unknown"), None);
        assert_eq!(memory_type_from_key("other.key"), None);
    }

    #[test]
    fn mem_config_defaults_to_ranking_constants() {
        let cfg = MemConfig::new();
        // Fact default is 365 days per crate::ranking.
        let d = cfg.half_life("acme", MemoryType::Fact);
        assert_eq!(d, crate::ranking::half_life(MemoryType::Fact));
    }

    #[test]
    fn mem_config_override_takes_precedence() {
        let cfg = MemConfig::new();
        cfg.set_half_life("acme", MemoryType::Fact, Duration::days(7));
        assert_eq!(cfg.half_life("acme", MemoryType::Fact), Duration::days(7));
        // Other tenants still get the default.
        assert_eq!(
            cfg.half_life("beta", MemoryType::Fact),
            crate::ranking::half_life(MemoryType::Fact)
        );
    }

    #[test]
    fn mem_config_unset_reverts_to_default() {
        let cfg = MemConfig::new();
        cfg.set_half_life("acme", MemoryType::Fact, Duration::days(7));
        assert!(cfg.unset_half_life("acme", MemoryType::Fact));
        assert_eq!(
            cfg.half_life("acme", MemoryType::Fact),
            crate::ranking::half_life(MemoryType::Fact)
        );
        // Second unset is a no-op.
        assert!(!cfg.unset_half_life("acme", MemoryType::Fact));
    }

    #[test]
    fn mem_config_forget_tenant_drops_all_overrides() {
        let cfg = MemConfig::new();
        cfg.set_half_life("acme", MemoryType::Fact, Duration::days(7));
        cfg.set_half_life("acme", MemoryType::Event, Duration::days(1));
        cfg.set_half_life("beta", MemoryType::Fact, Duration::days(30));
        assert_eq!(cfg.len(), 3);
        cfg.forget_tenant("acme");
        assert_eq!(cfg.len(), 1);
        assert_eq!(
            cfg.half_life("acme", MemoryType::Fact),
            crate::ranking::half_life(MemoryType::Fact)
        );
        assert_eq!(cfg.half_life("beta", MemoryType::Fact), Duration::days(30));
    }

    #[test]
    fn apply_event_set_updates_shared_state() {
        let cfg = MemConfig::new();
        apply_event(
            &cfg,
            ConfigEvent::Set {
                tenant: "acme".into(),
                key: "half_life.fact".into(),
                value: ConfigValue::HalfLife {
                    half_life_secs: 604_800,
                },
            },
        );
        assert_eq!(cfg.half_life("acme", MemoryType::Fact), Duration::seconds(604_800));
    }

    #[test]
    fn apply_event_unset_reverts() {
        let cfg = MemConfig::new();
        cfg.set_half_life("acme", MemoryType::Fact, Duration::days(1));
        apply_event(
            &cfg,
            ConfigEvent::Unset {
                tenant: "acme".into(),
                key: "half_life.fact".into(),
            },
        );
        assert_eq!(
            cfg.half_life("acme", MemoryType::Fact),
            crate::ranking::half_life(MemoryType::Fact)
        );
    }

    #[test]
    fn apply_event_ignores_non_positive_half_life() {
        let cfg = MemConfig::new();
        apply_event(
            &cfg,
            ConfigEvent::Set {
                tenant: "acme".into(),
                key: "half_life.fact".into(),
                value: ConfigValue::HalfLife { half_life_secs: 0 },
            },
        );
        // Nothing landed → still the default.
        assert_eq!(
            cfg.half_life("acme", MemoryType::Fact),
            crate::ranking::half_life(MemoryType::Fact)
        );
    }

    #[test]
    fn apply_event_ignores_unknown_key() {
        let cfg = MemConfig::new();
        apply_event(
            &cfg,
            ConfigEvent::Set {
                tenant: "acme".into(),
                key: "unknown.key".into(),
                value: ConfigValue::Other(serde_json::json!({})),
            },
        );
        assert_eq!(cfg.len(), 0);
    }

    #[test]
    fn config_regex_matches_expected_topics() {
        let re = regex::Regex::new(CONFIG_TOPICS_REGEX).unwrap();
        assert!(re.is_match("mem.config.acme"));
        assert!(re.is_match("mem.config.beta-corp"));
        assert!(!re.is_match("mem.config.acme.extra"));
        assert!(!re.is_match("mem.raw.acme"));
        assert!(!re.is_match("mem.config"));
        assert!(!re.is_match("mem.config."));
    }

    #[test]
    fn consumer_config_defaults() {
        let c = MemConfigConsumerConfig::new("localhost:9092");
        assert_eq!(c.group_id, "chronik-memory-config-consumer");
        let c2 = c.with_group_id("custom");
        assert_eq!(c2.group_id, "custom");
    }
}
