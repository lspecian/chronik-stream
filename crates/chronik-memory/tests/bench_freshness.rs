//! T0→T1 freshness bench — measures the time between producing a raw turn and
//! the produced typed memory becoming recallable. N=100 probes by default.
//!
//! ```bash
//! ANTHROPIC_API_KEY=... CHRONIK_INTEGRATION=1 \
//!   cargo test -p chronik-memory --test bench_freshness -- --ignored --nocapture
//! ```

use chronik_memory::extractor::Turn;
use chronik_memory::{
    AnthropicExtractor, ChainedExtractor, Memory, MemoryType, RuleExtractor,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ulid::Ulid;

const PROBES: usize = 50;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const POLL_TIMEOUT: Duration = Duration::from_secs(60);

fn percentile(mut samples: Vec<u128>, p: f64) -> u128 {
    samples.sort_unstable();
    if samples.is_empty() {
        return 0;
    }
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx]
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + CHRONIK_INTEGRATION=1 + live cluster"]
async fn t0_t1_freshness_distribution() {
    if std::env::var("CHRONIK_INTEGRATION").ok().as_deref() != Some("1") {
        eprintln!("skipping: set CHRONIK_INTEGRATION=1 to run");
        return;
    }
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("skipping: ANTHROPIC_API_KEY not set");
            return;
        }
    };

    let kafka =
        std::env::var("CHRONIK_KAFKA").unwrap_or_else(|_| "localhost:9092".to_string());
    let api =
        std::env::var("CHRONIK_API").unwrap_or_else(|_| "http://localhost:6092".to_string());
    let ns = format!("bench-freshness:{}", Ulid::new());

    let extractor = ChainedExtractor::new(vec![
        Arc::new(RuleExtractor::new()),
        Arc::new(AnthropicExtractor::new(api_key)),
    ]);

    let mem = Memory::builder()
        .chronik_kafka(kafka)
        .chronik_api(api)
        .namespace(&ns)
        .extractor(extractor)
        .build()
        .await
        .expect("build memory");
    mem.init_namespace().await.expect("init namespace");

    let mut samples = Vec::with_capacity(PROBES);
    let mut timeouts = 0;

    for i in 0..PROBES {
        let marker = format!("FRESHNESS_PROBE_{i}_{}", Ulid::new());
        let turn = Turn {
            role: "user".into(),
            content: format!(
                "I prefer {marker}. Please remember this fact about me."
            ),
            ts: None,
            channel: None,
            external_id: None,
        };

        let t0 = Instant::now();
        let _ack = mem
            .ingest_with_extraction(vec![turn])
            .await
            .expect("ingest_with_extraction");

        let mut visible = false;
        let deadline = t0 + POLL_TIMEOUT;
        while Instant::now() < deadline {
            let results = mem
                .recall(&marker)
                .types(&[MemoryType::Fact])
                .k(5)
                .send()
                .await
                .expect("recall");
            if results.iter().any(|r| match &r.memory.body {
                chronik_memory::Body::Fact(f) => {
                    f.text.contains(&marker)
                        || serde_json::to_string(&f.object)
                            .unwrap_or_default()
                            .contains(&marker)
                }
                _ => false,
            }) {
                visible = true;
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        let elapsed = t0.elapsed().as_millis();
        if visible {
            samples.push(elapsed);
        } else {
            timeouts += 1;
            eprintln!("  probe {i} timed out after {elapsed}ms");
        }
    }

    if samples.is_empty() {
        panic!("all {} probes timed out — extractor or recall is broken", PROBES);
    }

    let min = *samples.iter().min().unwrap();
    let max = *samples.iter().max().unwrap();
    let p50 = percentile(samples.clone(), 0.50);
    let p95 = percentile(samples.clone(), 0.95);
    let p99 = percentile(samples.clone(), 0.99);

    println!();
    println!("T0 → T1 freshness over {} probes ({} timeouts)", samples.len(), timeouts);
    println!("  min   = {min} ms");
    println!("  p50   = {p50} ms");
    println!("  p95   = {p95} ms");
    println!("  p99   = {p99} ms");
    println!("  max   = {max} ms");

    // Phase 1 lag budget: p99 < 30s (per AMS-1.5 spec).
    assert!(
        p99 < 30_000,
        "p99 freshness {p99} ms exceeds 30s budget"
    );
}
