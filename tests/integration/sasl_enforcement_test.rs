//! End-to-end proof that SASL authentication is *enforced*, not merely offered.
//!
//! These tests exist because the thing they check was previously faked. The old
//! `crates/chronik-server/tests/sasl_test_standalone.rs` asserted that an
//! unauthenticated Produce was blocked — against a `ConnectionRegistry` it
//! defined inside the test file. The server had no such component: it verified
//! credentials, answered `error_code=0`, and then let any client produce whether
//! or not it had authenticated. The test passed for years while the control did
//! not exist.
//!
//! So every test here drives a **real broker process** with a **real client**
//! (rdkafka, the same librdkafka that Java/Go/Python clients wrap), and the ones
//! that matter assert a *denial*.

mod common;

use anyhow::Result;
use common::{exclusive, ObjectStorageType, TestCluster, TestClusterConfig};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::time::Duration;

const TOPIC: &str = "sasl-enforcement-topic";
const USER: &str = "alice";
const PASSWORD: &str = "s3cret";

fn auth_cluster_config() -> TestClusterConfig {
    TestClusterConfig {
        num_servers: 1,
        object_storage: ObjectStorageType::Local,
        enable_auth: true,
        sasl_users: vec![(USER.to_string(), PASSWORD.to_string())],
        ..Default::default()
    }
}

/// A producer that does not authenticate at all (plain PLAINTEXT).
fn plaintext_producer(bootstrap: &str) -> Result<FutureProducer> {
    Ok(ClientConfig::new()
        .set("bootstrap.servers", bootstrap)
        .set("message.timeout.ms", "5000")
        .set("socket.timeout.ms", "4000")
        // Do not let librdkafka retry for the whole test duration; we want the
        // failure reported promptly.
        .set("retries", "0")
        .create()?)
}

/// A producer presenting SASL/PLAIN credentials.
fn sasl_producer(bootstrap: &str, user: &str, password: &str) -> Result<FutureProducer> {
    Ok(ClientConfig::new()
        .set("bootstrap.servers", bootstrap)
        .set("security.protocol", "SASL_PLAINTEXT")
        .set("sasl.mechanism", "PLAIN")
        .set("sasl.username", user)
        .set("sasl.password", password)
        .set("message.timeout.ms", "5000")
        .set("socket.timeout.ms", "4000")
        .set("retries", "0")
        .create()?)
}

async fn try_produce(producer: &FutureProducer, payload: &str) -> Result<(), String> {
    let record = FutureRecord::to(TOPIC).payload(payload).key("k");
    match producer
        .send(record, Duration::from_secs(8))
        .await
    {
        Ok(_) => Ok(()),
        Err((e, _)) => Err(e.to_string()),
    }
}

/// THE test: with SASL required, a client that never authenticates must not be
/// able to produce.
#[tokio::test]
async fn unauthenticated_client_cannot_produce() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let bootstrap = cluster.bootstrap_servers();

    let producer = plaintext_producer(&bootstrap)?;
    let result = try_produce(&producer, "should-never-land").await;

    assert!(
        result.is_err(),
        "produce from an UNAUTHENTICATED client succeeded against a SASL-required broker - \
         authentication is not being enforced"
    );

    Ok(())
}

/// The positive control: correct credentials do work. Without this, the test
/// above could pass because the broker is simply broken.
#[tokio::test]
async fn authenticated_client_can_produce() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let bootstrap = cluster.bootstrap_servers();

    let producer = sasl_producer(&bootstrap, USER, PASSWORD)?;
    let result = try_produce(&producer, "authenticated-payload").await;

    assert!(
        result.is_ok(),
        "produce with VALID credentials failed: {:?} - \
         enforcement has locked out legitimate clients",
        result
    );

    Ok(())
}

/// Wrong password must be refused. This is the case a stubbed verifier passes.
#[tokio::test]
async fn wrong_password_cannot_produce() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let bootstrap = cluster.bootstrap_servers();

    let producer = sasl_producer(&bootstrap, USER, "wrong-password")?;
    let result = try_produce(&producer, "should-never-land").await;

    assert!(
        result.is_err(),
        "produce with an INCORRECT password succeeded - credentials are not being verified"
    );

    Ok(())
}

/// An unknown principal must be refused even with a well-formed exchange.
#[tokio::test]
async fn unknown_user_cannot_produce() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let bootstrap = cluster.bootstrap_servers();

    let producer = sasl_producer(&bootstrap, "mallory", PASSWORD)?;
    let result = try_produce(&producer, "should-never-land").await;

    assert!(
        result.is_err(),
        "produce as an UNKNOWN user succeeded - the credential store is not being consulted"
    );

    Ok(())
}

/// SCRAM must not be offered while its proof verification is unimplemented.
///
/// Previously the broker advertised SCRAM-SHA-256/512 and accepted any
/// password on them, so a client selecting SCRAM authenticated unconditionally.
#[tokio::test]
async fn scram_is_not_offered() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let bootstrap = cluster.bootstrap_servers();

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap)
        .set("security.protocol", "SASL_PLAINTEXT")
        .set("sasl.mechanism", "SCRAM-SHA-256")
        .set("sasl.username", USER)
        .set("sasl.password", PASSWORD)
        .set("message.timeout.ms", "5000")
        .set("socket.timeout.ms", "4000")
        .set("retries", "0")
        .create()?;

    let result = try_produce(&producer, "should-never-land").await;

    assert!(
        result.is_err(),
        "SCRAM-SHA-256 authenticated successfully - the mechanism is advertised but its \
         client proof is never verified, so any password is accepted"
    );

    Ok(())
}

/// No regression for the default configuration: with SASL disabled (the default)
/// an ordinary client works exactly as before Phase 0.
#[tokio::test]
async fn sasl_disabled_by_default_does_not_break_plain_clients() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(TestClusterConfig {
        num_servers: 1,
        object_storage: ObjectStorageType::Local,
        ..Default::default()
    })
    .await?;
    let bootstrap = cluster.bootstrap_servers();

    let producer = plaintext_producer(&bootstrap)?;
    let result = try_produce(&producer, "ordinary-payload").await;

    assert!(
        result.is_ok(),
        "produce against a broker with SASL disabled failed: {:?} - \
         Phase 0 has regressed the default path",
        result
    );

    Ok(())
}
