//! AM-1.7 end-to-end smoke test.
//!
//! Drives the `/memory/v1/*` HTTP surface against a running Chronik instance.
//! Verifies the full round-trip:
//!   init-namespace → remember → recall → forget → health
//!
//! Gated by `CHRONIK_INTEGRATION=1` and a running server on
//! `CHRONIK_MEMORY_TEST_API` (default `http://localhost:6092`). Server must
//! have been started with `CHRONIK_MEMORY_KAFKA` + `CHRONIK_MEMORY_API` set.
//!
//! Run:
//! ```sh
//! # Start a server:
//! CHRONIK_MEMORY_KAFKA=localhost:9092 \
//! CHRONIK_MEMORY_API=http://127.0.0.1:6092 \
//!   target/release/chronik-server start
//!
//! # Run the smoke test:
//! CHRONIK_INTEGRATION=1 cargo test -p chronik-memory --test http_smoke -- --nocapture
//! ```

use serde_json::{json, Value};
use std::time::Duration;

fn integration_enabled() -> bool {
    std::env::var("CHRONIK_INTEGRATION").as_deref() == Ok("1")
}

fn base_url() -> String {
    std::env::var("CHRONIK_MEMORY_TEST_API")
        .unwrap_or_else(|_| "http://localhost:6092".to_string())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("http client")
}

/// One namespace per test run — ULID prefix keeps runs isolated and avoids
/// memory-cap exhaustion across repeated CI runs (per the pilot 5 incident).
fn fresh_namespace() -> String {
    // No ulid dep here — fall back to a uuid + timestamp hash.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("smoke:am17:bot:test:{}", nanos)
}

#[tokio::test]
async fn http_smoke_end_to_end() {
    if !integration_enabled() {
        eprintln!("skipping http_smoke_end_to_end — set CHRONIK_INTEGRATION=1 to enable");
        return;
    }

    let api = base_url();
    let client = http();
    let ns = fresh_namespace();
    eprintln!("[http_smoke] using namespace = {ns}");

    // ── 1. health (no auth) ──────────────────────────────────────────────
    let resp = client
        .get(format!("{api}/memory/v1/health"))
        .send()
        .await
        .expect("health: send");
    assert!(resp.status().is_success(), "health: status {}", resp.status());
    let body: Value = resp.json().await.expect("health: parse");
    eprintln!("[http_smoke] health = {body}");
    assert_eq!(
        body.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "health status must be 'ok' (server must be started with CHRONIK_MEMORY_ENABLED + KAFKA + API): {body}"
    );

    // ── 2. provision namespace ───────────────────────────────────────────
    // The smoke namespace is `smoke:am17:bot:test:{nanos}`; tenant = "smoke".
    let resp = client
        .post(format!("{api}/memory/v1/admin/init-namespace"))
        .json(&json!({
            "tenant": "smoke",
            "agent": "am17",
        }))
        .send()
        .await
        .expect("init-ns: send");
    assert!(
        resp.status().is_success(),
        "init-ns: status {} body {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // ── 3. remember (typed memory bypasses extraction) ───────────────────
    let resp = client
        .post(format!("{api}/memory/v1/remember"))
        .json(&json!({
            "namespace": ns,
            "type": "fact",
            "key": "user|budget|max",
            "body": {
                "subject": "user",
                "predicate": "budget_max",
                "object": 4000,
                "text": "max budget $4000/mo for a Williamsburg 2-br"
            },
            "confidence": 1.0
        }))
        .send()
        .await
        .expect("remember: send");
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "remember: status {status} body {body_text}"
    );
    let body: Value = serde_json::from_str(&body_text).expect("remember: parse");
    eprintln!("[http_smoke] remember = {body}");
    assert!(body.get("memory_id").is_some(), "remember: missing memory_id");
    assert!(body.get("offset").is_some(), "remember: missing offset");

    // ── 4. recall (BM25-only) with retry to absorb indexer lag ───────────
    // Hot text index is enabled by default (CHRONIK_HOT_TEXT_ENABLED=true);
    // visibility flip interval is 100ms. Six 1-second tries is generous.
    let mut results: Vec<Value> = Vec::new();
    let mut last_body = Value::Null;
    let mut last_latency: u64 = 0;
    for attempt in 0..6 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let resp = client
            .post(format!("{api}/memory/v1/recall"))
            .json(&json!({
                "namespace": ns,
                "query": "what is the user's max budget?",
                "k": 5,
                // BM25 only — vector requires an embedding provider, which
                // a baseline smoke test shouldn't require.
                "channels": ["bm25"]
            }))
            .send()
            .await
            .expect("recall: send");
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "recall: status {status} body {body_text}"
        );
        let body: Value = serde_json::from_str(&body_text).expect("recall: parse");
        last_latency = body.get("latency_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        results = body
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        last_body = body;
        if !results.is_empty() {
            eprintln!(
                "[http_smoke] recall hit on attempt {} ({} results, latency_ms={last_latency})",
                attempt + 1,
                results.len()
            );
            break;
        }
    }

    assert!(
        !results.is_empty(),
        "recall returned 0 hits after 6 retries — indexer/recall broken? body={last_body}"
    );
    let first = &results[0];
    eprintln!("[http_smoke] top hit = {first}");
    assert_eq!(
        first.get("type").and_then(|v| v.as_str()),
        Some("fact"),
        "top hit should be the fact we just wrote: {first}"
    );

    // ── 5. forget (tombstone the fact) ───────────────────────────────────
    let resp = client
        .post(format!("{api}/memory/v1/forget"))
        .json(&json!({
            "namespace": ns,
            "type": "fact",
            "key": "user|budget|max"
        }))
        .send()
        .await
        .expect("forget: send");
    assert!(resp.status().is_success(), "forget: status {}", resp.status());
    let body: Value = resp.json().await.expect("forget: parse");
    assert_eq!(
        body.get("tombstoned").and_then(|v| v.as_bool()),
        Some(true),
        "forget: tombstoned must be true: {body}"
    );

    eprintln!("[http_smoke] OK — round-trip clean");
}

#[tokio::test]
async fn http_smoke_rejects_malformed_ingest() {
    if !integration_enabled() {
        return;
    }
    let api = base_url();
    let client = http();

    // Empty turns array — handler must 400, not 500 or 202.
    let resp = client
        .post(format!("{api}/memory/v1/ingest"))
        .json(&json!({
            "namespace": "smoke:am17:bot:test:malformed",
            "turns": []
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        400,
        "empty turns should yield 400, got {}",
        resp.status()
    );
    let body: Value = resp.json().await.expect("parse");
    assert_eq!(
        body.get("error").and_then(|e| e.get("code")).and_then(|c| c.as_str()),
        Some("bad_request"),
        "error code must be 'bad_request': {body}"
    );
}

#[tokio::test]
async fn http_smoke_unknown_type_is_400() {
    if !integration_enabled() {
        return;
    }
    let api = base_url();
    let client = http();

    let resp = client
        .post(format!("{api}/memory/v1/forget"))
        .json(&json!({
            "namespace": "smoke:am17:bot:test:unknown",
            "type": "nonsense",
            "key": "k"
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status().as_u16(), 400, "unknown type → 400");
}

#[tokio::test]
async fn http_smoke_source_not_implemented() {
    if !integration_enabled() {
        return;
    }
    let api = base_url();
    let client = http();

    let resp = client
        .get(format!("{api}/memory/v1/01HXZ_test_id/source"))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        501,
        "source should be 501 (AM-2.6): got {}",
        resp.status()
    );
}
