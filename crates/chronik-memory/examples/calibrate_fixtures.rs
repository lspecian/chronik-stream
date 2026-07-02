//! Fixture calibration harness — runs the Anthropic V2 extractor against every
//! fixture in `tests/fixtures/agent-memory/` and prints the actual
//! `(subject, predicate, key)` triples it emits.
//!
//! The eval-recall integration test reports MRR ~ 0.09 not because retrieval
//! is broken, but because fixture `expected_keys` reference predicate names
//! the extractor doesn't emit (e.g. fixture says `preferred_neighborhood`,
//! prompt v2 emits `seeks_neighborhood`). This harness gives us the ground
//! truth we need to recalibrate the fixtures (Gap 1 in the agent-memory SDK
//! roadmap).
//!
//! ## Usage
//!
//! ```bash
//! ANTHROPIC_API_KEY=sk-ant-... \
//!   cargo run -p chronik-memory --example calibrate_fixtures \
//!   > calibration.json
//! ```
//!
//! Output is JSON keyed by fixture name; each value is a list of
//! `{subject, predicate, key, confidence, source_indexes, type}` records and
//! a list of suggested `expected_keys` (for compactable types only).
//!
//! ## What this tool is NOT
//!
//! - Not a fixture writer — printing only. Manual review of the JSON before
//!   editing fixtures keeps a human in the loop on whether the extractor's
//!   labels match the fixture's intent (e.g. `seeks_neighborhood` is a fine
//!   rename, but `user|max_budget_eur` → `user|budget` would lose the
//!   currency hint and we'd want to push back on the prompt instead).
//! - Not a test — we don't grade or assert; we dump.

use chronik_memory::extractor::{Extractor, Turn};
use chronik_memory::schema::Body;
use chronik_memory::AnthropicExtractor;
use chronik_memory::eval::Fixture;
use std::path::PathBuf;
use serde::Serialize;

const FIXTURES_DIR: &str = "tests/fixtures/agent-memory";

#[derive(Debug, Serialize)]
struct ProducedRecord {
    #[serde(rename = "type")]
    kind: String,
    subject: Option<String>,
    predicate: Option<String>,
    key: Option<String>,
    confidence: f32,
    source_indexes: Vec<usize>,
    text: String,
}

#[derive(Debug, Serialize)]
struct FixtureCalibration {
    name: String,
    n_turns: usize,
    produced: Vec<ProducedRecord>,
    /// All compactable-type keys actually emitted, deduplicated. These are
    /// the values the fixture's `expected_memories[].key` and
    /// `recall_queries[].expected_keys` should reference.
    suggested_expected_keys: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("set ANTHROPIC_API_KEY before running calibration");
    let extractor = AnthropicExtractor::new(api_key);

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURES_DIR);
    let mut fixtures: Vec<Fixture> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&p)?;
        let f: Fixture = serde_json::from_slice(&bytes)?;
        fixtures.push(f);
    }
    fixtures.sort_by(|a, b| a.name.cmp(&b.name));

    let mut report: Vec<FixtureCalibration> = Vec::new();
    for f in &fixtures {
        eprintln!("=> calibrating {}", f.name);
        let turns: Vec<Turn> = f
            .conversation
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
            .collect();

        let extracted = match extractor.extract(&turns).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("   FAIL: {e}");
                continue;
            }
        };

        let mut produced = Vec::with_capacity(extracted.len());
        let mut keys: Vec<String> = Vec::new();
        for ex in &extracted {
            let (kind, subject, predicate, text) = match &ex.body {
                Body::Fact(b) => (
                    "fact",
                    Some(b.subject.clone()),
                    Some(b.predicate.clone()),
                    b.text.clone(),
                ),
                Body::Event(b) => ("event", None, None, format!("{} {}", b.actor, b.verb)),
                Body::Instruction(b) => (
                    "instruction",
                    Some(b.scope.clone()),
                    Some(b.trigger.clone()),
                    b.rule.clone(),
                ),
                Body::Task(b) => (
                    "task",
                    b.owner.clone(),
                    Some(b.task_id.clone()),
                    b.title.clone(),
                ),
                Body::Concept(b) => (
                    "concept",
                    Some(b.entity_id.clone()),
                    Some(b.entity_type.clone()),
                    b.title.clone(),
                ),
            };
            if let Some(k) = &ex.key {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
            produced.push(ProducedRecord {
                kind: kind.into(),
                subject,
                predicate,
                key: ex.key.clone(),
                confidence: ex.confidence,
                source_indexes: ex.source_indexes.clone(),
                text,
            });
        }
        report.push(FixtureCalibration {
            name: f.name.clone(),
            n_turns: turns.len(),
            produced,
            suggested_expected_keys: keys,
        });
    }

    let json = serde_json::to_string_pretty(&report)?;
    println!("{json}");
    Ok(())
}
