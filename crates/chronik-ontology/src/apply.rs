//! The I/O edge of the Ontology SDK: produce the keyed records built by
//! [`crate::schema`] to Kafka. Record *building* is pure and unit-tested there;
//! this module only does the producing, so the CLI stays a thin wrapper.

use std::time::Duration;

use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};

use crate::schema::OntRecord;

/// A failure while producing SDK records to Kafka.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("failed to create Kafka producer: {0}")]
    Producer(String),
    #[error("produce to {topic} failed: {reason}")]
    Produce { topic: String, reason: String },
    #[error("init-namespace failed: {0}")]
    InitNamespace(String),
}

/// Provision a tenant's memory backing so its `mem.*` topics are created
/// **searchable** — without this, facts produced by `ingest` land in a
/// non-searchable topic and `get_object`/`as_of` (which resolve via `/_search`)
/// find nothing, even though `traverse`/`related` (edge index) still work. Calls
/// the Unified-API memory admin endpoint; idempotent for an already-provisioned
/// tenant. Must run BEFORE the first produce to `mem.fact.{tenant}` (a topic
/// auto-created by a plain produce gets the non-searchable default).
pub async fn init_namespace(api: &str, tenant: &str, agent: &str) -> Result<(), ApplyError> {
    let url = format!("{}/memory/v1/admin/init-namespace", api.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "tenant": tenant, "agent": agent }))
        .send()
        .await
        .map_err(|e| ApplyError::InitNamespace(format!("request to {url}: {e}")))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ApplyError::InitNamespace(format!("{code}: {body}")));
    }
    Ok(())
}

fn producer(brokers: &str) -> Result<FutureProducer, ApplyError> {
    ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("queue.buffering.max.messages", "1000000")
        // Fail fast on a down/unreachable broker instead of retrying for the
        // librdkafka default (5 min): a CLI should error in seconds.
        .set("message.timeout.ms", "15000")
        .create()
        .map_err(|e| ApplyError::Producer(e.to_string()))
}

/// Produce a batch of keyed records and return how many were sent. Each record
/// is awaited (delivery confirmed) so a broken broker surfaces immediately.
pub async fn publish_records(brokers: &str, records: &[OntRecord]) -> Result<usize, ApplyError> {
    let p = producer(brokers)?;
    for r in records {
        p.send(
            FutureRecord::to(&r.topic).key(&r.key).payload(&r.value),
            Duration::from_secs(10),
        )
        .await
        .map_err(|(e, _)| ApplyError::Produce { topic: r.topic.clone(), reason: e.to_string() })?;
    }
    Ok(records.len())
}
