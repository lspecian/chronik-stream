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
