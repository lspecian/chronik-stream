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

// NOTE: "an unimplemented mechanism must be refused" is asserted at the unit
// level instead (`sasl::tests` and `connection::tests`), not here. The two
// unimplemented variants are GSSAPI and OAUTHBEARER, and neither can drive this
// assertion end-to-end: this librdkafka is built without a GSSAPI provider, so
// the *client* refuses to construct ("No provider for SASL mechanism GSSAPI")
// and the broker is never contacted. A test that fails in the client tells us
// nothing about the server.
//
// The server-side guarantee is structural: `ENABLED_MECHANISMS` is the single
// source of advertised mechanisms, the handshake only completes for a mechanism
// in it, and `handle_authenticate` hard-refuses anything else.

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

// ---------------------------------------------------------------------------
// Phase 1: SCRAM
//
// SCRAM was withdrawn in Phase 0 because its verification was a stub that
// accepted any password. These tests exist to prove the replacement is real:
// the positive cases must authenticate against librdkafka's own SCRAM client
// (which verifies the server signature, so a fabricated one fails), and the
// negative cases must be refused.
// ---------------------------------------------------------------------------

fn scram_producer(
    bootstrap: &str,
    mechanism: &str,
    user: &str,
    password: &str,
) -> Result<FutureProducer> {
    Ok(ClientConfig::new()
        .set("bootstrap.servers", bootstrap)
        .set("security.protocol", "SASL_PLAINTEXT")
        .set("sasl.mechanism", mechanism)
        .set("sasl.username", user)
        .set("sasl.password", password)
        .set("message.timeout.ms", "8000")
        .set("socket.timeout.ms", "6000")
        .set("retries", "0")
        .create()?)
}

#[tokio::test]
async fn scram_sha256_authenticates_with_correct_password() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let producer = scram_producer(
        &cluster.bootstrap_servers(),
        "SCRAM-SHA-256",
        USER,
        PASSWORD,
    )?;

    let result = try_produce(&producer, "scram256-payload").await;
    assert!(
        result.is_ok(),
        "SCRAM-SHA-256 with valid credentials failed: {:?}. librdkafka verifies the \
         server signature, so this also proves the signature is computed correctly",
        result
    );
    Ok(())
}

#[tokio::test]
async fn scram_sha512_authenticates_with_correct_password() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let producer = scram_producer(
        &cluster.bootstrap_servers(),
        "SCRAM-SHA-512",
        USER,
        PASSWORD,
    )?;

    let result = try_produce(&producer, "scram512-payload").await;
    assert!(
        result.is_ok(),
        "SCRAM-SHA-512 with valid credentials failed: {:?}",
        result
    );
    Ok(())
}

/// THE regression test for the old stub, which accepted any client-final message.
#[tokio::test]
async fn scram_sha256_rejects_wrong_password() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let producer = scram_producer(
        &cluster.bootstrap_servers(),
        "SCRAM-SHA-256",
        USER,
        "wrong-password",
    )?;

    let result = try_produce(&producer, "should-never-land").await;
    assert!(
        result.is_err(),
        "SCRAM-SHA-256 accepted an INCORRECT password - the client proof is not \
         being verified, which is exactly the stub this replaced"
    );
    Ok(())
}

#[tokio::test]
async fn scram_sha512_rejects_wrong_password() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let producer = scram_producer(
        &cluster.bootstrap_servers(),
        "SCRAM-SHA-512",
        USER,
        "wrong-password",
    )?;

    let result = try_produce(&producer, "should-never-land").await;
    assert!(
        result.is_err(),
        "SCRAM-SHA-512 accepted an INCORRECT password"
    );
    Ok(())
}

#[tokio::test]
async fn scram_rejects_unknown_user() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(auth_cluster_config()).await?;
    let producer = scram_producer(
        &cluster.bootstrap_servers(),
        "SCRAM-SHA-256",
        "mallory",
        PASSWORD,
    )?;

    let result = try_produce(&producer, "should-never-land").await;
    assert!(result.is_err(), "SCRAM accepted an UNKNOWN user");
    Ok(())
}

// ---------------------------------------------------------------------------
// Migration mode (staged rollout)
//
// With a single Kafka port, going straight from "no authentication" to
// "authentication required" cuts off every client that has not been
// reconfigured, at the same instant. CHRONIK_SASL_ENABLED=optional is the step
// in between: credentials are verified when offered, unauthenticated clients
// are still served and logged, so an operator can watch the logs until the
// warnings stop and only then require it.
// ---------------------------------------------------------------------------

fn optional_auth_config() -> TestClusterConfig {
    TestClusterConfig {
        num_servers: 1,
        object_storage: ObjectStorageType::Local,
        // enable_auth drives CHRONIK_SASL_ENABLED=true; override it to
        // "optional" through the env escape hatch below.
        enable_auth: true,
        sasl_users: vec![(USER.to_string(), PASSWORD.to_string())],
        ..Default::default()
    }
}

#[tokio::test]
async fn optional_mode_serves_unauthenticated_clients() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start_with_env(
        optional_auth_config(),
        &[("CHRONIK_SASL_ENABLED", "optional")],
    )
    .await?;

    let producer = plaintext_producer(&cluster.bootstrap_servers())?;
    let result = try_produce(&producer, "migration-payload").await;

    assert!(
        result.is_ok(),
        "an unauthenticated client was refused in OPTIONAL mode: {:?} - the staged \
         rollout is not usable, so operators have no way to enable auth without an \
         outage",
        result
    );
    Ok(())
}

#[tokio::test]
async fn optional_mode_still_accepts_valid_credentials() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start_with_env(
        optional_auth_config(),
        &[("CHRONIK_SASL_ENABLED", "optional")],
    )
    .await?;

    let producer = sasl_producer(&cluster.bootstrap_servers(), USER, PASSWORD)?;
    let result = try_produce(&producer, "migration-authenticated").await;

    assert!(
        result.is_ok(),
        "an AUTHENTICATED client failed in OPTIONAL mode: {:?}",
        result
    );
    Ok(())
}

/// Optional must not degrade into "any password works" - it is a migration
/// step, not a weakening. A wrong password still fails the exchange.
#[tokio::test]
async fn optional_mode_still_rejects_wrong_credentials() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start_with_env(
        optional_auth_config(),
        &[("CHRONIK_SASL_ENABLED", "optional")],
    )
    .await?;

    let producer = sasl_producer(&cluster.bootstrap_servers(), USER, "wrong-password")?;
    let result = try_produce(&producer, "should-never-land").await;

    assert!(
        result.is_err(),
        "a WRONG password was accepted in OPTIONAL mode - optional applies to \
         whether credentials are required, not to whether they are checked"
    );
    Ok(())
}
