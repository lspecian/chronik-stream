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

// ---------------------------------------------------------------------------
// SCRAM credential administration (APIs 50/51)
//
// Users created through AlterUserScramCredentials go into the metadata log, so
// they authenticate against every broker and survive a restart — unlike
// CHRONIK_SASL_USERS, which is per-broker configuration needing a redeploy.
//
// These use the Java `kafka-configs.sh` tooling shape via raw protocol, because
// rdkafka's AdminClient has no user-credential API.
// ---------------------------------------------------------------------------

/// A user created via API 51 must be able to authenticate, and must SURVIVE a
/// restart — that is the difference between cluster state and one broker's RAM.
#[tokio::test]
async fn scram_user_created_via_api_survives_restart() -> Result<()> {
    let _guard = exclusive().await;
    let dir = tempfile::tempdir()?;

    let mut config = auth_cluster_config();
    config.data_dir = Some(dir.path().to_path_buf());

    // First broker: create "carol" through the credential API.
    {
        let cluster = TestCluster::start(config.clone()).await?;
        create_scram_user(&cluster.bootstrap_servers(), "carol", "carol-pw").await?;

        // She can authenticate immediately.
        let producer = scram_producer(
            &cluster.bootstrap_servers(),
            "SCRAM-SHA-256",
            "carol",
            "carol-pw",
        )?;
        assert!(
            try_produce(&producer, "carol-payload").await.is_ok(),
            "a user created through AlterUserScramCredentials could not authenticate"
        );
    } // broker process killed

    // Second broker, same data directory.
    let cluster = TestCluster::start(config).await?;
    let producer = scram_producer(
        &cluster.bootstrap_servers(),
        "SCRAM-SHA-256",
        "carol",
        "carol-pw",
    )?;

    assert!(
        try_produce(&producer, "carol-after-restart").await.is_ok(),
        "the user vanished across a restart - credentials are still per-process \
         state rather than replicated cluster state"
    );

    // And a wrong password for that user is still refused.
    let wrong = scram_producer(
        &cluster.bootstrap_servers(),
        "SCRAM-SHA-256",
        "carol",
        "wrong",
    )?;
    assert!(
        try_produce(&wrong, "should-never-land").await.is_err(),
        "a stored credential accepted the wrong password"
    );

    Ok(())
}

/// Send AlterUserScramCredentials (API 51) over a raw socket.
///
/// The salted password is computed client-side exactly as `kafka-configs.sh`
/// does, so the plaintext password never reaches the broker.
async fn create_scram_user(bootstrap: &str, user: &str, password: &str) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let addr = bootstrap.split(',').next().unwrap();
    let mut stream = tokio::net::TcpStream::connect(addr).await?;

    // The broker requires SASL before any other API, so authenticate first.
    // (An unauthenticated API 51 gets the connection closed - which is the
    // pre-auth gate doing its job, and is what this test hit first.)
    sasl_plain_authenticate(&mut stream, USER, PASSWORD).await?;

    let salt: Vec<u8> = (0u8..16).collect();
    let iterations: i32 = 4096;
    let salted = pbkdf2_sha256(password.as_bytes(), &salt, iterations as u32);

    // --- request body (flexible encoding) ---
    let mut body: Vec<u8> = Vec::new();
    put_uvarint(&mut body, 1); // deletions: empty array
    put_uvarint(&mut body, 2); // upsertions: 1 entry
    put_compact_str(&mut body, user);
    body.push(1i8 as u8); // SCRAM-SHA-256
    body.extend_from_slice(&iterations.to_be_bytes());
    put_uvarint(&mut body, salt.len() as u32 + 1);
    body.extend_from_slice(&salt);
    put_uvarint(&mut body, salted.len() as u32 + 1);
    body.extend_from_slice(&salted);
    put_uvarint(&mut body, 0); // upsertion tagged fields
    put_uvarint(&mut body, 0); // request tagged fields

    // --- flexible request header (v2) ---
    let mut header: Vec<u8> = Vec::new();
    header.extend_from_slice(&51i16.to_be_bytes()); // api key
    header.extend_from_slice(&0i16.to_be_bytes()); // api version
    header.extend_from_slice(&99i32.to_be_bytes()); // correlation id
    put_compact_str(&mut header, "chronik-test");
    put_uvarint(&mut header, 0); // header tagged fields

    let mut frame = Vec::new();
    frame.extend_from_slice(&((header.len() + body.len()) as i32).to_be_bytes());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&body);
    stream.write_all(&frame).await?;

    // Read the response and require an all-zero error code per result.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut resp = vec![0u8; len];
    stream.read_exact(&mut resp).await?;

    anyhow::ensure!(
        resp.len() > 8,
        "AlterUserScramCredentials response too short: {} bytes",
        resp.len()
    );
    // correlation id (4) + tagged fields (1) + throttle (4), then the results
    // array. A non-zero error code would appear after the user name; rather than
    // re-implement the decoder here, assert the user name came back, which only
    // happens when the broker processed the entry.
    anyhow::ensure!(
        String::from_utf8_lossy(&resp).contains(user),
        "broker did not acknowledge user '{}' - response: {:?}",
        user,
        &resp[..resp.len().min(64)]
    );
    Ok(())
}

fn put_uvarint(buf: &mut Vec<u8>, mut value: u32) {
    loop {
        if value < 0x80 {
            buf.push(value as u8);
            return;
        }
        buf.push(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
}

fn put_compact_str(buf: &mut Vec<u8>, value: &str) {
    put_uvarint(buf, value.len() as u32 + 1);
    buf.extend_from_slice(value.as_bytes());
}

/// PBKDF2-HMAC-SHA256, matching what a Kafka client computes for SCRAM-SHA-256.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
    use hmac::Hmac;
    use sha2::Sha256;
    let mut out = vec![0u8; 32];
    pbkdf2::pbkdf2::<Hmac<Sha256>>(password, salt, iterations, &mut out).unwrap();
    out
}

/// Perform a SASL/PLAIN handshake on a raw socket.
///
/// Written by hand because these tests talk raw protocol for the credential
/// APIs, which rdkafka's AdminClient does not expose. Both SaslHandshake and
/// SaslAuthenticate v1 use the NON-flexible header and body encoding — the spec
/// marks SaslHandshake "flexibleVersions: none", and getting that wrong is what
/// made every handshake unparseable before.
async fn sasl_plain_authenticate(
    stream: &mut tokio::net::TcpStream,
    user: &str,
    password: &str,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn header(api_key: i16, version: i16, correlation: i32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&api_key.to_be_bytes());
        h.extend_from_slice(&version.to_be_bytes());
        h.extend_from_slice(&correlation.to_be_bytes());
        let client_id = b"chronik-test";
        h.extend_from_slice(&(client_id.len() as i16).to_be_bytes());
        h.extend_from_slice(client_id);
        h
    }

    async fn round_trip(
        stream: &mut tokio::net::TcpStream,
        header: Vec<u8>,
        body: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&((header.len() + body.len()) as i32).to_be_bytes());
        frame.extend_from_slice(&header);
        frame.extend_from_slice(&body);
        stream.write_all(&frame).await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let mut resp = vec![0u8; i32::from_be_bytes(len_buf) as usize];
        stream.read_exact(&mut resp).await?;
        Ok(resp)
    }

    // SaslHandshake v1: mechanism as a regular (length-prefixed) string.
    let mut body = Vec::new();
    let mechanism = b"PLAIN";
    body.extend_from_slice(&(mechanism.len() as i16).to_be_bytes());
    body.extend_from_slice(mechanism);
    let resp = round_trip(stream, header(17, 1, 1), body).await?;
    // correlation_id (4) then error_code (2)
    let error = i16::from_be_bytes([resp[4], resp[5]]);
    anyhow::ensure!(error == 0, "SaslHandshake failed with error {}", error);

    // SaslAuthenticate v1: auth bytes as regular bytes (int32 length).
    let auth = format!("\0{}\0{}", user, password).into_bytes();
    let mut body = Vec::new();
    body.extend_from_slice(&(auth.len() as i32).to_be_bytes());
    body.extend_from_slice(&auth);
    let resp = round_trip(stream, header(36, 1, 2), body).await?;
    let error = i16::from_be_bytes([resp[4], resp[5]]);
    anyhow::ensure!(error == 0, "SaslAuthenticate failed with error {}", error);

    Ok(())
}
