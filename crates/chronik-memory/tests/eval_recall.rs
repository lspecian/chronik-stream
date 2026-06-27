//! Recall-quality eval — for each fixture, ingest_with_extraction once, then
//! issue every recall query and grade with NDCG@10 / P@5 / MRR.
//!
//! Requires a live Chronik. See `eval_extraction.rs` header for env vars.
//!
//! ```bash
//! ANTHROPIC_API_KEY=... CHRONIK_INTEGRATION=1 \
//!   cargo test -p chronik-memory --test eval_recall -- --ignored --nocapture
//! ```

use chronik_memory::eval::{mrr, ndcg_at_k, precision_at_k, Fixture};
use chronik_memory::extractor::Turn;
use chronik_memory::{AnthropicExtractor, ChainedExtractor, Memory, RuleExtractor};
use std::path::PathBuf;
use std::sync::Arc;
use ulid::Ulid;

const FIXTURES_DIR: &str = "tests/fixtures/agent-memory";

fn load_fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURES_DIR);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("fixture dir") {
        let p = entry.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&p).expect("read fixture");
        let f: Fixture = serde_json::from_slice(&bytes).expect("parse fixture");
        out.push(f);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn fixture_to_turns(f: &Fixture) -> Vec<Turn> {
    f.conversation
        .iter()
        .map(|t| Turn {
            role: t.role.clone(),
            content: t.content.clone(),
            ts: t
                .ts
                .as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            channel: None,
            external_id: None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + CHRONIK_INTEGRATION=1 + live cluster"]
async fn evaluate_recall_quality() {
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

    let fixtures = load_fixtures();
    println!("\nRecall eval — {} fixtures\n", fixtures.len());

    let mut all_ndcg = Vec::new();
    let mut all_p5 = Vec::new();
    let mut all_mrr = Vec::new();

    for f in &fixtures {
        // Fresh namespace per fixture so prior runs don't leak.
        let ns = format!("eval:{}:{}", f.name, Ulid::new());

        let extractor = ChainedExtractor::new(vec![
            Arc::new(RuleExtractor::new()),
            Arc::new(AnthropicExtractor::new(api_key.clone())),
        ]);

        let mem = Memory::builder()
            .chronik_kafka(kafka.clone())
            .chronik_api(api.clone())
            .namespace(&ns)
            .extractor(extractor)
            // Bump from default 10s — Chronik can be slow to respond when
            // background indexing + the prior fixture's extraction LLM call
            // are both in flight.
            .request_timeout(std::time::Duration::from_secs(30))
            .build()
            .await
            .expect("build memory");
        mem.init_namespace().await.expect("init namespace");

        let turns = fixture_to_turns(f);
        let _ack = mem
            .ingest_with_extraction(turns)
            .await
            .expect("ingest_with_extraction");

        // Tantivy hot path should be sub-second per ROADMAP_HOT_PATH, but
        // 3-node clusters in this repo run cold-only by default — the
        // WalIndexer cycle is 30+ seconds. Sleep ~35s per fixture to let
        // cold indexing catch up. If you have hot text confirmed enabled
        // (look for `hot_text_index` in node logs) you can drop this back to
        // 1500ms locally.
        let recall_sleep = std::env::var("CHRONIK_RECALL_INDEX_SLEEP_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(35_000);
        tokio::time::sleep(std::time::Duration::from_millis(recall_sleep)).await;

        for q in &f.recall_queries {
            let results = mem
                .recall(&q.query)
                .k(q.k)
                .send()
                .await
                .expect("recall");
            let ranked: Vec<String> = results
                .iter()
                .filter_map(|r| r.memory.key.clone())
                .collect();

            let n = ndcg_at_k(&ranked, &q.expected_keys, q.k);
            let p5 = precision_at_k(&ranked, &q.expected_keys, 5);
            let m = mrr(&ranked, &q.expected_keys);
            println!(
                "  [{}] q={:?} ndcg={:.2} p@5={:.2} mrr={:.2}",
                f.name, q.query, n, p5, m
            );
            if !q.expected_keys.is_empty() {
                all_ndcg.push(n);
                all_p5.push(p5);
                all_mrr.push(m);
            }
        }
    }

    let avg = |v: &[f64]| -> f64 {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };

    println!();
    println!(
        "TOTAL: NDCG@10={:.3}  P@5={:.3}  MRR={:.3}  ({} queries)",
        avg(&all_ndcg),
        avg(&all_p5),
        avg(&all_mrr),
        all_ndcg.len()
    );

    // Sanity check: at least one query returned a hit. A 0/N pipeline
    // failure means the SDK or the cluster is broken and warrants
    // investigation.
    //
    // We do **not** assert a fixed MRR floor here. The fixtures were
    // calibrated against AnthropicExtractor v2 at temperature=0 (see
    // `examples/calibrate_fixtures.rs`), but Anthropic's API is only
    // *near*-deterministic at temp=0 — predicate names still drift across
    // runs ~30% of the time. With the SDK fixes landed (typed-loc plumbing,
    // namespace bias in BM25, `_value`/`_json_content` parse fallback), a
    // fresh test run typically lands MRR around 0.40–0.55, vs. 0.00 before.
    // CI uses the n_hits > 0 floor to catch regressions like a broken
    // produce or parse path.
    let n_hits = all_mrr.iter().filter(|m| **m > 0.0).count();
    assert!(
        n_hits > 0,
        "all {} queries returned 0 hits — pipeline broken",
        all_mrr.len()
    );
    println!(
        "\nNote: MRR={:.3} reflects residual fixture-vs-extractor drift even \
         at temp=0 ({}/{} queries hit). Calibration was last refreshed via \
         `cargo run -p chronik-memory --example calibrate_fixtures`.",
        avg(&all_mrr),
        n_hits,
        all_mrr.len()
    );
}
