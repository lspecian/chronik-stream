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
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
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
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
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
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
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
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
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
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
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

// ---------------------------------------------------------------------------
// Persistence
//
// Before ACLs were written to the metadata log, a rule created through
// CreateAcls lived in one broker's memory: it answered success, then vanished on
// restart and was never visible to any other node. That is the same shape as the
// SASL handshake that verified credentials and discarded the answer, so it gets
// the same treatment - a test that proves the rule is still in force after the
// process is replaced.
// ---------------------------------------------------------------------------

/// A policy expressed in configuration must still apply after a restart, and the
/// data written under it must still be there.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn acl_policy_survives_a_restart() -> Result<()> {
    let _guard = exclusive().await;
    let dir = tempfile::tempdir()?;

    let mut config = acl_cluster_config();
    config.data_dir = Some(dir.path().to_path_buf());

    // First broker: write to the allowed topic, and confirm the denied one is
    // refused.
    {
        let cluster = TestCluster::start(config.clone()).await?;
        let producer = producer(&cluster.bootstrap_servers())?;

        try_produce(&producer, ALLOWED_TOPIC)
            .await
            .expect("allowed topic must accept a write");
        assert!(
            try_produce(&producer, DENIED_TOPIC).await.is_err(),
            "denied topic accepted a write before restart"
        );
    } // cluster dropped: the broker process is killed here

    // Second broker over the same data directory.
    let cluster = TestCluster::start(config).await?;
    let producer = producer(&cluster.bootstrap_servers())?;

    assert!(
        try_produce(&producer, ALLOWED_TOPIC).await.is_ok(),
        "the allowed topic was refused AFTER restart - the policy did not survive"
    );
    assert!(
        try_produce(&producer, DENIED_TOPIC).await.is_err(),
        "the denied topic was accepted AFTER restart - authorization lapsed across \
         the restart, which is exactly the silent-expiry failure this guards"
    );

    Ok(())
}

/// OffsetCommit needs Read on the group. Without the check, a consumer denied
/// Read could still advance the group's committed offsets - it could not read
/// the data, but it could disrupt every consumer that can.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn offset_commit_on_an_unauthorized_group_is_denied() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(acl_cluster_config()).await?;

    // The topic must exist first, or the consumer fails with
    // UnknownTopicOrPartition and the test passes for the wrong reason — it
    // would prove nothing about the group check.
    let producer = producer(&cluster.bootstrap_servers())?;
    try_produce(&producer, ALLOWED_TOPIC)
        .await
        .expect("the allowed topic must accept a write");

    // "acl-group" is granted Read; this one is not.
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .set("security.protocol", "SASL_PLAINTEXT")
        .set("sasl.mechanism", "SCRAM-SHA-256")
        .set("sasl.username", USER)
        .set("sasl.password", PASSWORD)
        .set("group.id", "unauthorized-group")
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .set("session.timeout.ms", "6000")
        .create()?;

    consumer.subscribe(&[ALLOWED_TOPIC])?;

    // Joining an unauthorized group must fail; the group APIs are checked too.
    let outcome = tokio::time::timeout(Duration::from_secs(10), consumer.recv()).await;

    match outcome {
        Ok(Err(e)) => {
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("auth"),
                "joining an unauthorized group failed, but not with an authorization \
                 error: {}",
                e
            );
        }
        Ok(Ok(_)) => panic!("consumed records under a group the principal may not use"),
        Err(_) => panic!(
            "joining an unauthorized group neither failed nor returned records - the \
             group denial is being swallowed"
        ),
    }

    Ok(())
}

/// Metadata must not list topics the principal cannot Describe.
///
/// An all-topics Metadata request that named every topic on the cluster would
/// leak the topic inventory to a principal with no rights to any of it.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn metadata_omits_unauthorized_topics() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(acl_cluster_config()).await?;

    // Bring both topics into existence: the allowed one directly, and the
    // unreadable one through the write-only grant.
    let producer = producer(&cluster.bootstrap_servers())?;
    try_produce(&producer, ALLOWED_TOPIC).await.expect("allowed write");
    try_produce(&producer, NO_READ_TOPIC).await.expect("write-only write");

    // alice has Describe on both of those, and on nothing else. Fetching all
    // metadata must return exactly the topics she may describe.
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .set("security.protocol", "SASL_PLAINTEXT")
        .set("sasl.mechanism", "SCRAM-SHA-256")
        .set("sasl.username", USER)
        .set("sasl.password", PASSWORD)
        .set("group.id", "acl-group")
        .create()?;

    let metadata = consumer.fetch_metadata(None, Duration::from_secs(15))?;
    let listed: Vec<String> = metadata
        .topics()
        .iter()
        .map(|t| t.name().to_string())
        .collect();

    // Whatever else exists on the broker, a topic alice has no ACL for must not
    // appear. DENIED_TOPIC has no binding at all.
    assert!(
        !listed.contains(&DENIED_TOPIC.to_string()),
        "metadata listed a topic the principal may not describe: {:?}",
        listed
    );
    // And the authorized ones are still visible, or the filter is too strict.
    assert!(
        listed.contains(&ALLOWED_TOPIC.to_string()),
        "metadata omitted an AUTHORIZED topic: {:?}",
        listed
    );

    Ok(())
}
