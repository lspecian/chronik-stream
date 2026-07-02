//! Extraction quality eval — runs the configured extractor against each
//! fixture's conversation, computes precision / recall / type-classification /
//! cite-source accuracy, and prints a summary table.
//!
//! # Running
//!
//! These tests require:
//! - A live Chronik cluster reachable at `$CHRONIK_KAFKA` (default `localhost:9092`)
//!   and `$CHRONIK_API` (default `http://localhost:6092`).
//! - `$ANTHROPIC_API_KEY` for the LLM-pass extractor.
//!
//! They're behind `#[ignore]` so `cargo test` doesn't fail in environments
//! without those. Run explicitly:
//!
//! ```bash
//! ANTHROPIC_API_KEY=... cargo test -p chronik-memory --test eval_extraction \
//!     -- --ignored --nocapture
//! ```

use chronik_memory::eval::{
    extraction_metrics, negative_assertion_violations, ExtractionMetrics, Fixture,
};
use chronik_memory::extractor::{Extractor, Turn};
use chronik_memory::{AnthropicExtractor, ChainedExtractor, RuleExtractor};
use std::path::PathBuf;
use std::sync::Arc;

const FIXTURES_DIR: &str = "tests/fixtures/agent-memory";

fn load_fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURES_DIR);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
    {
        let p = entry.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
        let f: Fixture = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("parse {p:?}: {e}"));
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

fn print_row(name: &str, m: &ExtractionMetrics) {
    println!(
        "  {:<28} P={:.2}  R={:.2}  F1={:.2}  type-acc={:.2}  cite-acc={:.2}  ({}/{} matched)",
        name,
        m.precision(),
        m.recall(),
        m.f1(),
        m.type_accuracy(),
        m.cite_accuracy(),
        m.matched,
        m.expected_total,
    );
}

#[test]
fn fixtures_load() {
    let fs = load_fixtures();
    assert!(
        !fs.is_empty(),
        "no fixtures found in {FIXTURES_DIR}"
    );
    println!("loaded {} fixtures", fs.len());
    for f in &fs {
        println!("  {} — {} turns, {} expected memories",
            f.name, f.conversation.len(), f.expected_memories.len());
    }
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and a live Chronik (set CHRONIK_INTEGRATION=1)"]
async fn evaluate_extraction_quality() {
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("skipping: ANTHROPIC_API_KEY not set");
            return;
        }
    };

    let model = std::env::var("ANTHROPIC_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5".to_string());

    let extractor = ChainedExtractor::new(vec![
        Arc::new(RuleExtractor::new()),
        Arc::new(AnthropicExtractor::new(api_key).with_model(model)),
    ]);

    let fixtures = load_fixtures();
    println!("\nExtraction eval — {} fixtures\n", fixtures.len());

    let mut agg = ExtractionMetrics::default();
    let mut all_violations: Vec<(String, Vec<String>)> = Vec::new();
    for f in &fixtures {
        let turns = fixture_to_turns(f);
        let produced = extractor
            .extract(&turns)
            .await
            .unwrap_or_else(|e| panic!("extract({}): {e}", f.name));
        let m = extraction_metrics(f, &produced, turns.len());
        print_row(&f.name, &m);
        let v = negative_assertion_violations(f, &produced);
        if !v.is_empty() {
            println!(
                "    NEGATIVE-SPACE VIOLATIONS in {}: {:?}",
                f.name, v
            );
            all_violations.push((f.name.clone(), v));
        }
        agg.matched += m.matched;
        agg.expected_total += m.expected_total;
        agg.produced_total += m.produced_total;
        agg.type_correct += m.type_correct;
        agg.cite_in_range += m.cite_in_range;
    }

    println!();
    print_row("TOTAL", &agg);

    // Phase 1 exit gates (loose — calibration happens during dogfooding):
    assert!(
        agg.cite_accuracy() >= 0.95,
        "cite-source accuracy {} < 0.95 — extractor is hallucinating sources",
        agg.cite_accuracy()
    );
    assert!(
        all_violations.is_empty(),
        "negative-space violations: {:?}",
        all_violations
    );
}
