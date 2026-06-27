//! Provider-parity extraction eval — runs every supported LLM provider over
//! the same fixture set and prints a side-by-side comparison.
//!
//! AMS-2.3 ships three providers (Anthropic / OpenAI / Ollama). Phase 2 exit
//! criteria targets "Llama 3.1 8B NDCG@10 within 5 % of cloud providers";
//! this harness measures the *extraction-side* delta first (recall quality is
//! a separate measurement against `eval_recall.rs`).
//!
//! ## Running
//!
//! ```bash
//! ANTHROPIC_API_KEY=sk-ant-... \
//! OPENAI_API_KEY=sk-... \
//!   cargo test -p chronik-memory --test eval_provider_parity \
//!   -- --ignored --nocapture
//! ```
//!
//! Skips a provider if its API key is missing. Ollama is skipped unless
//! `OLLAMA_ENABLE=1` is set (we don't want to fail CI in environments
//! without a local Ollama running).

use chronik_memory::eval::{extraction_metrics, ExtractionMetrics, Fixture};
use chronik_memory::extractor::{Extractor, Turn};
use chronik_memory::{
    AnthropicExtractor, ChainedExtractor, OllamaExtractor, OpenAIExtractor, RuleExtractor,
};
use std::path::PathBuf;
use std::sync::Arc;

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

fn turns_for(f: &Fixture) -> Vec<Turn> {
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

async fn run_provider(
    label: &str,
    extractor: Arc<dyn Extractor>,
    fixtures: &[Fixture],
) -> ExtractionMetrics {
    println!("\n=== {label} ===");
    let mut agg = ExtractionMetrics::default();
    for f in fixtures {
        let turns = turns_for(f);
        let produced = match extractor.extract(&turns).await {
            Ok(p) => p,
            Err(e) => {
                println!("  {:30} FAIL: {e}", f.name);
                continue;
            }
        };
        let m = extraction_metrics(f, &produced, turns.len());
        println!(
            "  {:30} P={:.2} R={:.2} F1={:.2} type-acc={:.2} cite={:.2}",
            f.name,
            m.precision(),
            m.recall(),
            m.f1(),
            m.type_accuracy(),
            m.cite_accuracy()
        );
        agg.matched += m.matched;
        agg.expected_total += m.expected_total;
        agg.produced_total += m.produced_total;
        agg.type_correct += m.type_correct;
        agg.cite_in_range += m.cite_in_range;
    }
    println!(
        "  {:30} P={:.2} R={:.2} F1={:.2} type-acc={:.2} cite={:.2}",
        "TOTAL",
        agg.precision(),
        agg.recall(),
        agg.f1(),
        agg.type_accuracy(),
        agg.cite_accuracy()
    );
    agg
}

#[tokio::test]
#[ignore = "requires at least one provider API key (and optionally OLLAMA_ENABLE=1)"]
async fn provider_parity_extraction() {
    let fixtures = load_fixtures();
    println!("\nProvider-parity extraction eval — {} fixtures", fixtures.len());

    let mut summary: Vec<(String, ExtractionMetrics)> = Vec::new();

    if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
        if !k.is_empty() {
            let extractor = ChainedExtractor::new(vec![
                Arc::new(RuleExtractor::new()),
                Arc::new(AnthropicExtractor::new(k)),
            ]);
            let m = run_provider("Anthropic claude-haiku-4-5", Arc::new(extractor), &fixtures).await;
            summary.push(("Anthropic".into(), m));
        }
    }

    if let Ok(k) = std::env::var("OPENAI_API_KEY") {
        if !k.is_empty() {
            let model = std::env::var("OPENAI_MODEL")
                .unwrap_or_else(|_| "gpt-4o-mini".to_string());
            let extractor = ChainedExtractor::new(vec![
                Arc::new(RuleExtractor::new()),
                Arc::new(OpenAIExtractor::new(k).with_model(model.clone())),
            ]);
            let m = run_provider(
                &format!("OpenAI {model}"),
                Arc::new(extractor),
                &fixtures,
            )
            .await;
            summary.push(("OpenAI".into(), m));
        }
    }

    if std::env::var("OLLAMA_ENABLE").ok().as_deref() == Some("1") {
        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.1".to_string());
        let base = std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        let extractor = ChainedExtractor::new(vec![
            Arc::new(RuleExtractor::new()),
            Arc::new(OllamaExtractor::new(model.clone()).with_base_url(base)),
        ]);
        let m = run_provider(
            &format!("Ollama {model}"),
            Arc::new(extractor),
            &fixtures,
        )
        .await;
        summary.push(("Ollama".into(), m));
    }

    if summary.is_empty() {
        eprintln!("skipping: no provider API keys set");
        return;
    }

    println!("\n=== Provider parity summary ===");
    println!(
        "{:18} {:>6} {:>6} {:>6} {:>10} {:>6}",
        "provider", "P", "R", "F1", "type-acc", "cite"
    );
    for (label, m) in &summary {
        println!(
            "{:18} {:>6.2} {:>6.2} {:>6.2} {:>10.2} {:>6.2}",
            label,
            m.precision(),
            m.recall(),
            m.f1(),
            m.type_accuracy(),
            m.cite_accuracy()
        );
    }

    // Hallucination floor: every provider must clear cite-source ≥ 95 %.
    for (label, m) in &summary {
        assert!(
            m.cite_accuracy() >= 0.95,
            "{label}: cite-source accuracy {} < 0.95 — provider is hallucinating sources",
            m.cite_accuracy()
        );
    }
}
