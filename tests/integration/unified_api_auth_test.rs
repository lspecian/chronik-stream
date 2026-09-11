//! End-to-end proof that the Unified API's data plane requires a key
//! (Security Phase 4).
//!
//! `/_sql`, `/_search` and `/_vector` read topic contents and required nothing
//! at all: any process that could reach port 6092 could `SELECT * FROM
//! any_topic` over plain HTTP. Every control on the Kafka port — SASL
//! authentication and ACLs — was bypassable by asking the HTTP surface instead.
//!
//! These tests assert the denial, the allow, and that `/health` stays reachable
//! without credentials (otherwise Kubernetes probes fail and the fix looks like
//! an outage).

mod common;

use anyhow::Result;
use common::{exclusive, ObjectStorageType, TestCluster, TestClusterConfig};
use std::time::Duration;

const API_KEY: &str = "unified-api-test-key";

fn base_config() -> TestClusterConfig {
    TestClusterConfig {
        num_servers: 1,
        object_storage: ObjectStorageType::Local,
        ..Default::default()
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("http client")
}

/// THE test: without the key, a SQL query must be refused.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn sql_without_api_key_is_rejected() -> Result<()> {
    let _guard = exclusive().await;
    let cluster =
        TestCluster::start_with_env(base_config(), &[("CHRONIK_API_KEY", API_KEY)]).await?;

    let response = client()
        .post(format!("{}/_sql", cluster.search_endpoint()))
        .json(&serde_json::json!({ "query": "SELECT 1" }))
        .send()
        .await?;

    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "/_sql answered {} without an API key - the HTTP surface is still a way \
         around SASL and ACLs",
        response.status()
    );
    Ok(())
}

/// A wrong key must be refused too - not just a missing one.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn sql_with_wrong_api_key_is_rejected() -> Result<()> {
    let _guard = exclusive().await;
    let cluster =
        TestCluster::start_with_env(base_config(), &[("CHRONIK_API_KEY", API_KEY)]).await?;

    let response = client()
        .post(format!("{}/_sql", cluster.search_endpoint()))
        .header("X-API-Key", "not-the-key")
        .json(&serde_json::json!({ "query": "SELECT 1" }))
        .send()
        .await?;

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    Ok(())
}

/// The positive control: with the key the endpoint is reachable.
///
/// Any status other than 401 proves the middleware let the request through to
/// the handler; whether that query then succeeds is not what this test is about.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn sql_with_correct_api_key_is_allowed() -> Result<()> {
    let _guard = exclusive().await;
    let cluster =
        TestCluster::start_with_env(base_config(), &[("CHRONIK_API_KEY", API_KEY)]).await?;

    let response = client()
        .post(format!("{}/_sql", cluster.search_endpoint()))
        .header("X-API-Key", API_KEY)
        .json(&serde_json::json!({ "query": "SELECT 1" }))
        .send()
        .await?;

    assert_ne!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a request with the CORRECT API key was rejected - authentication is \
         locking out legitimate callers"
    );
    Ok(())
}

/// `/_search` reads topic data too, and is merged from a separate router. A
/// layer applied before that merge would silently miss it.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn search_without_api_key_is_rejected() -> Result<()> {
    let _guard = exclusive().await;
    let cluster =
        TestCluster::start_with_env(base_config(), &[("CHRONIK_API_KEY", API_KEY)]).await?;

    let response = client()
        .post(format!("{}/_search", cluster.search_endpoint()))
        .json(&serde_json::json!({ "query": { "match_all": {} } }))
        .send()
        .await?;

    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "/_search answered {} without an API key - the auth layer does not cover \
         the merged search router",
        response.status()
    );
    Ok(())
}

/// Health probes must not need credentials, or enabling auth reads as an outage.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn health_stays_open_without_a_key() -> Result<()> {
    let _guard = exclusive().await;
    let cluster =
        TestCluster::start_with_env(base_config(), &[("CHRONIK_API_KEY", API_KEY)]).await?;

    let response = client()
        .get(format!("{}/health", cluster.search_endpoint()))
        .send()
        .await?;

    assert!(
        response.status().is_success(),
        "/health returned {} with authentication enabled - liveness probes would \
         fail and the cluster would look down",
        response.status()
    );
    Ok(())
}

/// Unset key means the endpoints stay open: enabling this by default would break
/// every existing dashboard in one step.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn no_api_key_configured_leaves_endpoints_open() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start(base_config()).await?;

    let response = client()
        .post(format!("{}/_sql", cluster.search_endpoint()))
        .json(&serde_json::json!({ "query": "SELECT 1" }))
        .send()
        .await?;

    assert_ne!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "requests were rejected although CHRONIK_API_KEY is unset - the default \
         path has regressed"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTPS (Security Phase 4)
//
// CHRONIK_ADMIN_TLS_CERT was *documented* as enabling TLS for the HTTP API but
// only ever logged "axum-server crate not available" and carried on serving
// plain HTTP. These tests exist so that cannot happen again: they assert a real
// TLS handshake, not the presence of a config knob.
// ---------------------------------------------------------------------------

/// Generate a throwaway self-signed certificate for the test broker.
fn generate_test_cert(dir: &std::path::Path) -> Result<(String, String)> {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");

    let status = std::process::Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048",
            "-keyout", key.to_str().unwrap(),
            "-out", cert.to_str().unwrap(),
            "-days", "1", "-nodes",
            "-subj", "/CN=localhost",
            "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    anyhow::ensure!(status.success(), "openssl failed to generate a test certificate");

    Ok((cert.to_string_lossy().into(), key.to_string_lossy().into()))
}

#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn unified_api_serves_https_when_configured() -> Result<()> {
    let _guard = exclusive().await;
    let dir = tempfile::tempdir()?;
    let (cert, key) = generate_test_cert(dir.path())?;

    let cluster = TestCluster::start_with_env(
        base_config(),
        &[
            ("CHRONIK_API_TLS_CERT", cert.as_str()),
            ("CHRONIK_API_TLS_KEY", key.as_str()),
        ],
    )
    .await?;

    let addr = cluster.api_addrs()[0];
    let https_client = reqwest::Client::builder()
        // Self-signed: we are testing that TLS is served at all, not PKI.
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(15))
        .build()?;

    let response = https_client
        .get(format!("https://{}/health", addr))
        .send()
        .await?;

    assert!(
        response.status().is_success(),
        "HTTPS /health returned {}",
        response.status()
    );

    Ok(())
}

/// The negative half: with TLS on, a plain HTTP request must NOT be served.
///
/// Without this, a server that ignored the TLS configuration and kept serving
/// HTTP would still pass the test above if the client were lenient.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn plain_http_is_refused_when_tls_is_enabled() -> Result<()> {
    let _guard = exclusive().await;
    let dir = tempfile::tempdir()?;
    let (cert, key) = generate_test_cert(dir.path())?;

    let cluster = TestCluster::start_with_env(
        base_config(),
        &[
            ("CHRONIK_API_TLS_CERT", cert.as_str()),
            ("CHRONIK_API_TLS_KEY", key.as_str()),
        ],
    )
    .await?;

    let addr = cluster.api_addrs()[0];
    let result = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await;

    assert!(
        result.is_err(),
        "plain HTTP was served on a TLS-configured port - the certificate is \
         being ignored, which is exactly the CHRONIK_ADMIN_TLS_CERT failure mode"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-topic authorization on /_sql (Security Phase 4)
//
// The API key authenticates the caller; these prove it does not authorize
// everything. A key holder may only read topics the configured principal holds
// Read on, so /_sql stops being a way around the ACLs enforced on :9092.
// ---------------------------------------------------------------------------

const SQL_PRINCIPAL: &str = "User:sqlreader";

fn sql_acl_config() -> TestClusterConfig {
    TestClusterConfig {
        num_servers: 1,
        object_storage: ObjectStorageType::Local,
        enable_acls: true,
        acl_allow_if_no_acl: false,
        // The HTTP principal may read exactly one topic. ANONYMOUS (the Kafka
        // side, where SASL is off) may create and write both, so the test can
        // bring the topics into existence - an absent topic would make the
        // query fail as "table not found" and prove nothing about authorization.
        acl_bindings: format!(
            "{principal},Topic,sqlallowed,Read,Allow;\
             User:ANONYMOUS,Topic,sqlallowed,Write,Allow;\
             User:ANONYMOUS,Topic,sqldenied,Write,Allow",
            principal = SQL_PRINCIPAL
        ),
        ..Default::default()
    }
}

fn sql_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("CHRONIK_API_KEY", API_KEY),
        ("CHRONIK_API_PRINCIPAL", SQL_PRINCIPAL),
    ]
}

/// Bring the topics into existence so a query against them is authorized
/// rather than failing as "table not found".
async fn seed_topics(cluster: &TestCluster) -> Result<()> {
    use rdkafka::producer::{FutureProducer, FutureRecord};
    let producer: FutureProducer = rdkafka::config::ClientConfig::new()
        .set("bootstrap.servers", &cluster.bootstrap_servers())
        .set("message.timeout.ms", "8000")
        .create()?;
    for topic in ["sqlallowed", "sqldenied"] {
        let record: FutureRecord<str, str> = FutureRecord::to(topic).payload("seed");
        let _ = producer.send(record, Duration::from_secs(10)).await;
    }
    Ok(())
}

async fn run_sql(cluster: &TestCluster, query: &str) -> Result<(reqwest::StatusCode, String)> {
    let response = client()
        .post(format!("{}/_sql", cluster.search_endpoint()))
        .header("X-API-Key", API_KEY)
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Ok((status, body))
}

/// THE test: a query against a topic the principal cannot read is refused.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn sql_query_on_unauthorized_topic_is_forbidden() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start_with_env(sql_acl_config(), &sql_env()).await?;
    seed_topics(&cluster).await?;

    let (status, body) = run_sql(&cluster, "SELECT * FROM sqldenied").await?;

    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "/_sql answered {} for a topic the principal may not read - the HTTP \
         surface is still a way around the ACLs. Body: {}",
        status,
        body
    );
    Ok(())
}

/// A query that touches BOTH an allowed and a denied topic must be refused —
/// authorizing only the first table would leak the second through a join.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn sql_join_touching_an_unauthorized_topic_is_forbidden() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start_with_env(sql_acl_config(), &sql_env()).await?;
    seed_topics(&cluster).await?;

    let (status, body) = run_sql(
        &cluster,
        "SELECT * FROM sqlallowed a JOIN sqldenied d ON a._offset = d._offset",
    )
    .await?;

    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "a join reaching an unauthorized topic was allowed ({}). Body: {}",
        status,
        body
    );
    Ok(())
}

/// Unparseable SQL must be rejected, not run. A statement whose tables cannot be
/// determined cannot be authorized, and letting it through would authorize
/// nothing at all.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn unparseable_sql_is_rejected_rather_than_run() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start_with_env(sql_acl_config(), &sql_env()).await?;

    let (status, _) = run_sql(&cluster, "SELECT FROM WHERE ((").await?;

    assert!(
        status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::FORBIDDEN,
        "unparseable SQL returned {} - it must be refused, because a query whose \
         tables cannot be resolved cannot be authorized",
        status
    );
    Ok(())
}

/// A query touching no topic at all needs no authorization.
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn a_query_touching_no_topic_is_allowed() -> Result<()> {
    let _guard = exclusive().await;
    let cluster = TestCluster::start_with_env(sql_acl_config(), &sql_env()).await?;

    let (status, body) = run_sql(&cluster, "SELECT 1").await?;

    assert_ne!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "a query reading no topic was refused ({}) - the filter is too strict. \
         Body: {}",
        status,
        body
    );
    Ok(())
}

/// With ACLs off, /_sql keeps working for any topic (the default path).
#[tokio::test]
#[ignore = "starts a real broker; run with: cargo test --test <target> -- --ignored"]
async fn sql_authorization_is_off_by_default() -> Result<()> {
    let _guard = exclusive().await;
    let cluster =
        TestCluster::start_with_env(base_config(), &[("CHRONIK_API_KEY", API_KEY)]).await?;

    let (status, _) = run_sql(&cluster, "SELECT * FROM anything").await?;

    assert_ne!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "a query was refused with ACLs disabled - the default path has regressed"
    );
    Ok(())
}
