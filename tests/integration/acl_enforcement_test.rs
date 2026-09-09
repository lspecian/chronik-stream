//! End-to-end proof that ACL authorization is *enforced*.
//!
//! `crates/chronik-server/src/acl.rs` was 645 lines of complete-looking
//! authorizer with **zero call sites**: it compiled into the binary and never
//! ran, and the ACL admin APIs answered `SECURITY_DISABLED`. It could not have
//! worked either, because before Phase 0 there was no principal to authorize.
//!
//! These tests drive a real broker with a real client and assert *denials* —
//! and, just as importantly, that the denial is legible to the client rather
//! than an empty success. `ErrorHandler::build_error_response()` drops the error
//! code for Produce and Fetch and emits an empty topics array, so a naive
//! implementation makes a denied write look accepted; the handlers build their
//! own per-topic denial to avoid exactly that.

mod common;

use anyhow::Result;
use common::{exclusive, ObjectStorageType, TestCluster, TestClusterConfig};
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::time::Duration;

const USER: &str = "alice";
const PASSWORD: &str = "s3cret";
const ALLOWED_TOPIC: &str = "acl-allowed";
const DENIED_TOPIC: &str = "acl-denied";
/// alice may write and describe this topic but NOT read it, so it exists and has
/// records while Fetch is still denied — which is what makes the Fetch denial
/// distinguishable from "topic is empty".
const NO_READ_TOPIC: &str = "acl-no-read";

/// A broker requiring both authentication and authorization, where `alice` may
/// only write and read `acl-allowed`.
fn acl_cluster_config() -> TestClusterConfig {
    TestClusterConfig {
        num_servers: 1,
        object_storage: ObjectStorageType::Local,
        enable_auth: true,
        sasl_users: vec![(USER.to_string(), PASSWORD.to_string())],
        enable_acls: true,
        // Start closed: anything without a matching rule is denied.
        acl_allow_if_no_acl: false,
        acl_bindings: format!(
            "User:{user},Topic,{allowed},Read,Allow;\
             User:{user},Topic,{allowed},Write,Allow;\
             User:{user},Topic,{allowed},Describe,Allow;\
             User:{user},Topic,{no_read},Write,Allow;\
             User:{user},Topic,{no_read},Describe,Allow;\
             User:{user},Group,acl-group,Read,Allow",
            user = USER,
            allowed = ALLOWED_TOPIC,
            no_read = NO_READ_TOPIC
        ),
        ..Default::default()
    }
}

fn producer(bootstrap: &str) -> Result<FutureProducer> {
    Ok(ClientConfig::new()
        .set("bootstrap.servers", bootstrap)
        .set("security.protocol", "SASL_PLAINTEXT")
        .set("sasl.mechanism", "SCRAM-SHA-256")
        .set("sasl.username", USER)
        .set("sasl.password", PASSWORD)
        .set("message.timeout.ms", "8000")
        .set("socket.timeout.ms", "6000")
        .set("retries", "0")
        .create()?)
}

async fn try_produce(producer: &FutureProducer, topic: &str) -> Result<(), String> {
    let record: FutureRecord<str, str> = FutureRecord::to(topic).payload("payload");
    match producer.send(record, Duration::from_secs(10)).await {
        Ok(_) => Ok(()),
        Err((e, _)) => Err(e.to_string()),
    }
}

/// THE test: a principal with no ACL for a topic must not be able to write it.
#[tokio::test]
async fn produce_to_unauthorized_topic_is_denied() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(acl_cluster_config()).await?;
    let producer = producer(&cluster.bootstrap_servers())?;

    let result = try_produce(&producer, DENIED_TOPIC).await;

    assert!(
        result.is_err(),
        "produce to a topic with NO matching ACL succeeded - authorization is not \
         being enforced, or the denial was encoded as an empty success"
    );
    Ok(())
}

/// The positive control. Without it the test above could pass because the
/// broker is simply broken.
#[tokio::test]
async fn produce_to_authorized_topic_is_allowed() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(acl_cluster_config()).await?;
    let producer = producer(&cluster.bootstrap_servers())?;

    let result = try_produce(&producer, ALLOWED_TOPIC).await;

    assert!(
        result.is_ok(),
        "produce to an EXPLICITLY ALLOWED topic failed: {:?} - authorization is \
         rejecting traffic it should permit",
        result
    );
    Ok(())
}

/// A denied consumer must be told it is denied, not handed an empty topic.
///
/// This is the Fetch half of the "empty success" trap: a consumer that receives
/// an empty result set concludes the topic has no records and polls forever.
#[tokio::test]
async fn fetch_from_unauthorized_topic_is_denied() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(acl_cluster_config()).await?;

    // Populate the topic first. alice has Write and Describe on it but NOT
    // Read, so the topic genuinely exists and holds a record — otherwise an
    // empty poll would be indistinguishable from a correct denial and the test
    // would pass for the wrong reason.
    let producer = producer(&cluster.bootstrap_servers())?;
    try_produce(&producer, NO_READ_TOPIC)
        .await
        .expect("writing to the write-only topic must succeed");

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .set("security.protocol", "SASL_PLAINTEXT")
        .set("sasl.mechanism", "SCRAM-SHA-256")
        .set("sasl.username", USER)
        .set("sasl.password", PASSWORD)
        .set("group.id", "acl-group")
        .set("auto.offset.reset", "earliest")
        .set("session.timeout.ms", "6000")
        .create()?;

    consumer.subscribe(&[NO_READ_TOPIC])?;

    // Poll briefly; we expect an error, not records and not silence-as-success.
    let outcome = tokio::time::timeout(Duration::from_secs(10), consumer.recv()).await;

    match outcome {
        Ok(Err(e)) => {
            let msg = e.to_string();
            assert!(
                msg.to_lowercase().contains("auth"),
                "fetch from an unauthorized topic failed, but not with an authorization \
                 error: {}",
                msg
            );
        }
        Ok(Ok(_)) => panic!("received records from a topic the principal may not read"),
        Err(_) => panic!(
            "fetch from an unauthorized topic neither returned records nor reported an \
             error - the denial is being encoded as an empty success, so the consumer \
             sees a healthy empty topic"
        ),
    }
    Ok(())
}

/// A super user bypasses ACLs, which is how an operator recovers a cluster that
/// has been locked out by its own policy.
#[tokio::test]
async fn super_user_bypasses_acls() -> Result<()> {
    let _guard = exclusive().await;
    let mut config = acl_cluster_config();
    // No bindings at all, strict mode: only a super user can do anything.
    config.acl_bindings = String::new();
    let cluster = TestCluster::start_with_env(
        config,
        &[("CHRONIK_ACL_SUPER_USERS", "User:alice")],
    )
    .await?;

    let producer = producer(&cluster.bootstrap_servers())?;
    let result = try_produce(&producer, DENIED_TOPIC).await;

    assert!(
        result.is_ok(),
        "a super user was denied: {:?} - CHRONIK_ACL_SUPER_USERS is not being honoured, \
         which would leave a mis-configured cluster unrecoverable",
        result
    );
    Ok(())
}

/// With ACLs disabled (the default) nothing is checked.
#[tokio::test]
async fn acls_disabled_by_default_allows_everything() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(TestClusterConfig {
        num_servers: 1,
        object_storage: ObjectStorageType::Local,
        ..Default::default()
    })
    .await?;

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .set("message.timeout.ms", "8000")
        .set("retries", "0")
        .create()?;

    let result = try_produce(&producer, DENIED_TOPIC).await;
    assert!(
        result.is_ok(),
        "produce failed with authorization disabled: {:?} - the default path has \
         regressed",
        result
    );
    Ok(())
}
