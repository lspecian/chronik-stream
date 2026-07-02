//! LongMemEval runner — Phase 2 headline number for the SDK roadmap.
//!
//! Loads a JSONL dataset (LongMemEval format), runs each item through
//! `ingest_with_extraction` + `recall`, scores via `answer_match`, and prints
//! a per-item + aggregate report.
//!
//! ## Running
//!
//! ```bash
//! # Against the synthetic 5-item smoke set bundled in tree (no download):
//! ANTHROPIC_API_KEY=... CHRONIK_INTEGRATION=1 \
//!   CHRONIK_API=http://localhost:6094 CHRONIK_KAFKA=localhost:9094 \
//!   cargo test -p chronik-memory --test eval_longmemeval -- --ignored --nocapture
//!
//! # Against the real LongMemEval-S dataset (download manually first):
//! LONGMEMEVAL_PATH=/path/to/longmemeval_s.jsonl LONGMEMEVAL_N=20 \
//!   ANTHROPIC_API_KEY=... CHRONIK_INTEGRATION=1 \
//!   CHRONIK_API=http://localhost:6094 CHRONIK_KAFKA=localhost:9094 \
//!   cargo test -p chronik-memory --test eval_longmemeval -- --ignored --nocapture
//! ```
//!
//! The dataset is published at <https://github.com/xiaowu0162/LongMemEval>
//! (and on HuggingFace). LongMemEval-S is the smaller variant and the
//! standard target for the ≥0.70 NDCG / hit-rate Phase 2 exit criterion.
//!
//! ## What the test asserts
//!
//! Aggregated `answer_match` rate ≥ `LONGMEMEVAL_MIN_HIT_RATE` (default 0.0
//! — i.e. *no* floor, the test only checks that the pipeline runs end-to-end
//! and returns a number). The Phase 2 ≥ 0.70 target is a manual judgement
//! call after looking at the printed numbers, not a CI gate, because dataset
//! variants and N differ across runs.

use chronik_memory::eval::longmemeval::{
    answer_match, answer_match_llm, parse_jsonl, render_memory_text, LongMemEvalItem,
};
use chronik_memory::extractor::Turn;
use chronik_memory::{
    extract_subject_candidates, synthesize_concept, AnthropicExtractor, ChainedExtractor, Extractor,
    Memory, MemoryType, PromptVersion, RuleExtractor, TwoPassExtractor,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ulid::Ulid;

/// Default to the synthetic in-tree fixture so the runner is self-contained
/// for smoke tests. Override with `LONGMEMEVAL_PATH=/path/to/longmemeval_s.jsonl`
/// for the real benchmark.
const DEFAULT_DATASET: &str = "tests/fixtures/longmemeval/synthetic.jsonl";

fn dataset_path() -> PathBuf {
    if let Ok(p) = std::env::var("LONGMEMEVAL_PATH") {
        PathBuf::from(p)
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_DATASET)
    }
}

fn item_limit() -> usize {
    std::env::var("LONGMEMEVAL_N")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(5)
}

fn min_hit_rate() -> f32 {
    std::env::var("LONGMEMEVAL_MIN_HIT_RATE")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(0.0)
}

fn turns_per_batch() -> usize {
    std::env::var("LONGMEMEVAL_BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(50)
}

fn item_to_turns(item: &LongMemEvalItem) -> Vec<Turn> {
    let mut turns = Vec::new();
    for session in &item.haystack_sessions {
        for rc in session {
            // LongMemEval-S has occasional turns with empty role or content
            // (data artifacts). The SDK rejects empty content with
            // `InvalidArgument`, so filter them here — empty turns carry
            // no information for extraction or recall regardless.
            let content = rc.content.trim();
            let role = rc.role.trim();
            if content.is_empty() || role.is_empty() {
                continue;
            }
            turns.push(Turn {
                role: role.to_string(),
                content: content.to_string(),
                ts: None,
                channel: None,
                external_id: None,
            });
        }
    }
    turns
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + CHRONIK_INTEGRATION=1 + live cluster"]
async fn evaluate_longmemeval() {
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

    let path = dataset_path();
    if !path.exists() {
        eprintln!(
            "skipping: dataset not found at {:?}. Set LONGMEMEVAL_PATH or use the bundled synthetic file.",
            path
        );
        return;
    }
    let raw = std::fs::read_to_string(&path).expect("read dataset");
    let all_items = parse_jsonl(&raw).expect("parse dataset");
    let n_total = all_items.len();
    let n_run = item_limit().min(n_total);
    let items: Vec<&LongMemEvalItem> = all_items.iter().take(n_run).collect();
    println!(
        "\nLongMemEval — running {} / {} items from {:?}",
        n_run, n_total, path
    );

    let kafka =
        std::env::var("CHRONIK_KAFKA").unwrap_or_else(|_| "localhost:9092".to_string());
    let api =
        std::env::var("CHRONIK_API").unwrap_or_else(|_| "http://localhost:6092".to_string());
    let index_sleep_ms: u64 = std::env::var("LONGMEMEVAL_INDEX_SLEEP_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(45_000);
    let batch_size = turns_per_batch();

    // LLM-judge scoring: when enabled, every item is graded by BOTH the
    // substring matcher and an Anthropic-driven judge. The substring score
    // is a strict lower bound (factoid-only); the judge score is paraphrase-
    // tolerant. Side-by-side output makes the calibration gap obvious.
    let use_llm_judge = std::env::var("LONGMEMEVAL_USE_LLM_JUDGE")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    // The judge LLM is a separate `AnthropicExtractor` instance — we want
    // its `TextGenerator::complete` impl, which doesn't carry tool-use
    // overhead. Same model + temperature=0 as the extractor.
    let judge: Option<Arc<dyn chronik_memory::embeddings::TextGenerator>> = if use_llm_judge {
        Some(Arc::new(AnthropicExtractor::new(api_key.clone())))
    } else {
        None
    };
    if use_llm_judge {
        println!("LLM-judge scoring ENABLED — every item graded substring + LLM side-by-side");
    }

    // Synthesis mode (AMS-3.7 on-demand path): when enabled, after recall()
    // we ask the LLM to synthesize a single answer from the top-k memories,
    // then grade THAT synthesized answer against the gold (both substring +
    // judge). This is the on-demand half of concept-pages — instead of
    // returning atomic memories, we return a fused answer that handles
    // multi-fact arithmetic, conflict resolution by recency, and abstention.
    // Enables side-by-side comparison: raw retrieval HIT rate vs synthesized
    // answer HIT rate. The expected lift is on multi-fact / temporal /
    // abstention questions where atomic retrieval surfaces the right
    // memories but the substring/judge can't fuse them.
    let use_synth = std::env::var("LONGMEMEVAL_USE_SYNTHESIS")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    if use_synth && judge.is_none() {
        eprintln!(
            "warning: LONGMEMEVAL_USE_SYNTHESIS=1 requires a TextGenerator — \
             implicitly enabling LLM-judge mode for the synthesis prompt."
        );
    }
    let synth_gen: Option<Arc<dyn chronik_memory::embeddings::TextGenerator>> = if use_synth {
        Some(Arc::new(AnthropicExtractor::new(api_key.clone())))
    } else {
        None
    };
    if use_synth {
        println!(
            "SYNTHESIS mode ENABLED — recall results passed to LLM for fused answer; \
             both raw and synthesized hit rates reported"
        );
    }

    // Concept-pages mode (AMS-3.7 path B, pilot 5). When on, after extraction
    // we synthesize a concept page for the question's main entity (heuristic:
    // top-1 namespace-style subject candidate, plus "user" as a universal
    // fallback) and write it back via `Memory::remember_concept`. The recall
    // and synthesize calls then enable `with_concepts()` so the
    // pre-aggregated page is inlined above atomic memories. Tests the
    // hypothesis that pre-synthesis at indexing time recovers
    // multi-fact-arithmetic and dense-context items where on-demand
    // synthesis abstained because the raw values aren't surfaced together.
    let use_concepts = std::env::var("LONGMEMEVAL_USE_CONCEPTS")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    if use_concepts && synth_gen.is_none() {
        eprintln!(
            "warning: LONGMEMEVAL_USE_CONCEPTS=1 requires a TextGenerator — \
             enabling synthesis mode implicitly."
        );
    }
    if use_concepts {
        println!(
            "CONCEPT-PAGES mode ENABLED — concept pages synthesized per item before recall; \
             recall + synthesize use include_concepts(true)"
        );
    }
    let concept_indexing_sleep_ms: u64 = std::env::var("LONGMEMEVAL_CONCEPT_INDEX_SLEEP_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5_000);

    // Anthropic prompt version. Default V3 (the empirical baseline);
    // override via `LONGMEMEVAL_PROMPT_VERSION=v5` to evaluate the
    // concrete-noun additive prompt against the same dataset / cluster.
    let prompt_version = match std::env::var("LONGMEMEVAL_PROMPT_VERSION")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "v1" => PromptVersion::V1,
        "v2" => PromptVersion::V2,
        "v3" | "" => PromptVersion::V3,
        "v4" => PromptVersion::V4,
        "v5" => PromptVersion::V5,
        other => {
            eprintln!(
                "warning: LONGMEMEVAL_PROMPT_VERSION={other:?} unrecognised — falling back to V3"
            );
            PromptVersion::V3
        }
    };
    println!("EXTRACTOR PROMPT VERSION: {prompt_version:?}");

    let mut hits: usize = 0;          // primary scorer (judge if enabled, else substring)
    let mut substring_hits: usize = 0;
    let mut judge_hits: usize = 0;
    let mut synth_substring_hits: usize = 0;
    let mut synth_judge_hits: usize = 0;
    let mut synth_abstain_count: usize = 0;
    let mut total_synth_secs = 0.0_f64;
    let mut misses: usize = 0;
    let mut by_type: std::collections::HashMap<String, (usize, usize)> = Default::default();
    let mut total_extraction_secs = 0.0_f64;
    let mut total_recall_secs = 0.0_f64;
    let mut total_judge_secs = 0.0_f64;
    let runner_t0 = Instant::now();

    // AM-1.7 lever #1: chain TwoPassExtractor when LONGMEMEVAL_USE_TWO_PASS=1.
    // This wraps the base AnthropicExtractor with an entity-scan + per-entity
    // sweep pass, closing the single-session-assistant category that stays 0/3
    // when the single-pass extractor misses assistant-stated facts.
    let use_two_pass = std::env::var("LONGMEMEVAL_USE_TWO_PASS")
        .ok()
        .as_deref()
        == Some("1");
    if use_two_pass {
        println!("TWO-PASS EXTRACTOR ENABLED — Anthropic entity-scan + per-entity sweep (lever #1)");
    }

    for (i, item) in items.iter().enumerate() {
        // Override tenant via LONGMEMEVAL_TENANT so pilots can create fresh typed
        // topics when a previous run left them in a bad state (e.g. after a
        // cluster roll invalidates leader assignments).
        let tenant = std::env::var("LONGMEMEVAL_TENANT")
            .unwrap_or_else(|_| "longmemeval".to_string());
        let ns = format!("{}:{}:{}", tenant, item.question_id, Ulid::new());
        let base_pass1: Arc<dyn Extractor> = Arc::new(
            AnthropicExtractor::new(api_key.clone())
                .with_prompt_version(prompt_version),
        );
        let inner_extractor: Arc<dyn Extractor> = if use_two_pass {
            Arc::new(
                TwoPassExtractor::new(base_pass1, api_key.clone())
                    .expect("build TwoPassExtractor"),
            )
        } else {
            base_pass1
        };
        let extractor = ChainedExtractor::new(vec![
            Arc::new(RuleExtractor::new()),
            inner_extractor,
        ]);
        let mem = Memory::builder()
            .chronik_kafka(kafka.clone())
            .chronik_api(api.clone())
            .namespace(&ns)
            .extractor(extractor)
            .request_timeout(Duration::from_secs(60))
            .build()
            .await
            .expect("build memory");
        mem.init_namespace().await.expect("init namespace");

        let turns = item_to_turns(item);
        let n_turns = turns.len();

        // Chunk turns to keep individual extraction calls within model
        // context windows + provider rate limits. A single chunk's
        // extraction failure (e.g. transient Anthropic schema-validation
        // glitch where the model wraps `facts` array in a string, or
        // network blip) MUST NOT abort the whole 18-item pilot — log it
        // and continue with whatever already landed for this item.
        let extract_t0 = Instant::now();
        let mut chunk_failures = 0usize;
        for (chunk_idx, chunk) in turns.chunks(batch_size).enumerate() {
            match mem.ingest_with_extraction(chunk.to_vec()).await {
                Ok(_) => {}
                Err(e) => {
                    chunk_failures += 1;
                    eprintln!(
                        "  [warn] item {} chunk {} ingest_with_extraction failed: {} \
                         — skipping this chunk, eval continues",
                        item.question_id, chunk_idx, e
                    );
                }
            }
        }
        let extract_secs = extract_t0.elapsed().as_secs_f64();
        total_extraction_secs += extract_secs;
        if chunk_failures > 0 {
            eprintln!(
                "  [warn] item {} had {} chunk failure(s) — recall will run \
                 against a partial extraction",
                item.question_id, chunk_failures
            );
        }

        // Wait for the cold WalIndexer cycle.
        tokio::time::sleep(Duration::from_millis(index_sleep_ms)).await;

        // Concept-page pre-synthesis (path B, opt-in). For each candidate
        // entity (top-1 from question + "user" as universal fallback),
        // run `synthesize_concept` which queries the just-indexed atomic
        // memories and rolls them up into a markdown page, then writes
        // back via `remember_concept`. The next sleep below lets the
        // concept topic's Tantivy index catch up so `include_concepts`
        // can find it.
        let mut concept_count = 0usize;
        let mut concept_secs = 0.0_f64;
        if use_concepts {
            if let Some(gen) = synth_gen.as_ref() {
                let mut entities: Vec<String> = vec!["user".to_string()];
                for s in extract_subject_candidates(&item.question) {
                    if !entities.contains(&s) && entities.len() < 3 {
                        entities.push(s);
                    }
                }
                let cs_t0 = Instant::now();
                for entity_id in &entities {
                    match synthesize_concept(&mem, entity_id, gen.clone()).await {
                        Ok(body) => {
                            let _ = mem.remember_concept(body, 1.0).await.map_err(|e| {
                                eprintln!(
                                    "  [warn] item {} concept-write {} failed: {}",
                                    item.question_id, entity_id, e
                                );
                            });
                            concept_count += 1;
                        }
                        Err(e) => {
                            // No-memories / malformed — log and continue. Don't
                            // fail the eval over a per-entity hiccup.
                            eprintln!(
                                "  [warn] item {} concept-synth {} skipped: {}",
                                item.question_id, entity_id, e
                            );
                        }
                    }
                }
                concept_secs = cs_t0.elapsed().as_secs_f64();
                // Wait for the concept records to land in the typed-topic
                // Tantivy index so `include_concepts` can retrieve them.
                tokio::time::sleep(Duration::from_millis(concept_indexing_sleep_ms)).await;
            }
        }

        // Multi-channel recall: BM25 (default) + Vector (semantic) + KeyMatch
        // (rule-based subject phrase boost). Vector channel requires the cluster
        // to have an embedding provider configured (OPENAI_API_KEY) and the
        // typed-memory topics to have `vector.enabled=true` (set by
        // `init_namespace` via `TopicConfig::fact/event/instruction`). If the
        // server can't embed (no key, no provider), the channel degrades
        // silently and contributes zero to RRF — won't crash the eval.
        // When `LONGMEMEVAL_USE_CONCEPTS=1`, also inline the top concept
        // page from `mem.concept.{tenant}` above atomic memories.
        let recall_t0 = Instant::now();
        let results = mem
            .recall(&item.question)
            .types(&[MemoryType::Fact, MemoryType::Event, MemoryType::Instruction, MemoryType::Task])
            .with_vector()
            .with_key_match()
            .include_concepts(use_concepts)
            .k(10)
            .send()
            .await
            .expect("recall");
        let recall_secs = recall_t0.elapsed().as_secs_f64();
        total_recall_secs += recall_secs;

        let texts: Vec<String> = results
            .iter()
            .map(|r| render_memory_text(&r.memory.body))
            .collect();
        let substring_score = answer_match(&texts, &item.answer);
        let substring_hit = substring_score >= 0.999;
        substring_hits += if substring_hit { 1 } else { 0 };

        // If the judge is enabled and we have any retrieved memories,
        // grade with the judge too. Skip the LLM call when retrieval
        // returned 0 (no evidence to grade against — auto-miss, no cost).
        let (judge_hit, judge_secs) = if let Some(j) = judge.as_ref() {
            if texts.is_empty() {
                (false, 0.0)
            } else {
                let t0 = Instant::now();
                let s = answer_match_llm(&texts, &item.question, &item.answer, j.as_ref()).await;
                let elapsed = t0.elapsed().as_secs_f64();
                (s >= 0.999, elapsed)
            }
        } else {
            (false, 0.0)
        };
        if use_llm_judge {
            judge_hits += if judge_hit { 1 } else { 0 };
        }
        total_judge_secs += judge_secs;

        // Synthesis pass (gated). Build a *fresh* RecallBuilder against the
        // same `mem` and run `synthesize()` — it consumes the builder so we
        // can't reuse the one we already sent.
        let (synth_sub_hit, synth_judge_hit, synth_abstained, synth_secs) =
            if let Some(gen) = synth_gen.as_ref() {
                let t0 = Instant::now();
                // Same tolerance principle as extraction — a transient
                // synthesis-call failure (provider blip, schema glitch on
                // tool-use model emit, etc.) should be reported as a miss +
                // abstention, not crash the pilot.
                let synth_res = mem
                    .recall(&item.question)
                    .types(&[
                        MemoryType::Fact,
                        MemoryType::Event,
                        MemoryType::Instruction,
                        MemoryType::Task,
                    ])
                    .with_vector()
                    .with_key_match()
                    .include_concepts(use_concepts)
                    .k(10)
                    .synthesize(gen.clone())
                    .await;
                let elapsed = t0.elapsed().as_secs_f64();
                // LongMemEval-S marks abstention questions with a `_abs`
                // suffix on the question_id. For these, the gold answer is
                // "you didn't mention X" — the CORRECT behaviour is to
                // refuse rather than hallucinate. Standard
                // `answer_match_llm` ("is gold supported by retrieved
                // memories") never returns YES on `_abs` items because
                // the gold IS that the info isn't there. Credit a correct
                // synth-abstention as a HIT on those items.
                let is_abstention_question = item.question_id.ends_with("_abs");
                match synth_res {
                    Ok(synth) => {
                        let answer_text = synth.answer.clone();
                        let s_sub =
                            answer_match(&[answer_text.clone()], &item.answer) >= 0.999;
                        let s_judge = if is_abstention_question {
                            // `_abs` grading: hit iff synthesis correctly
                            // emitted the abstention literal. Skip the
                            // judge call (it would always say miss).
                            synth.abstained
                        } else if let Some(j) = judge.as_ref() {
                            answer_match_llm(
                                &[answer_text],
                                &item.question,
                                &item.answer,
                                j.as_ref(),
                            )
                            .await
                                >= 0.999
                        } else {
                            false
                        };
                        (s_sub, s_judge, synth.abstained, elapsed)
                    }
                    Err(e) => {
                        eprintln!(
                            "  [warn] item {} synthesize failed: {} — counted as miss + abstention",
                            item.question_id, e
                        );
                        (false, false, true, elapsed)
                    }
                }
            } else {
                (false, false, false, 0.0)
            };
        if use_synth {
            synth_substring_hits += if synth_sub_hit { 1 } else { 0 };
            synth_judge_hits += if synth_judge_hit { 1 } else { 0 };
            if synth_abstained {
                synth_abstain_count += 1;
            }
        }
        total_synth_secs += synth_secs;

        // Primary metric: judge if enabled, else substring.
        let hit = if use_llm_judge { judge_hit } else { substring_hit };

        let entry = by_type
            .entry(item.question_type.clone())
            .or_insert((0, 0));
        entry.0 += if hit { 1 } else { 0 };
        entry.1 += 1;

        if hit {
            hits += 1;
        } else {
            misses += 1;
        }

        let verdict = if use_llm_judge {
            // sub/judge tagging: "sub HIT  judge HIT", "sub miss judge HIT", etc.
            format!(
                "sub {:4} judge {:4}",
                if substring_hit { "HIT" } else { "miss" },
                if judge_hit { "HIT" } else { "miss" }
            )
        } else {
            (if substring_hit { "HIT " } else { "miss" }).to_string()
        };

        let synth_tag = if use_synth {
            format!(
                " synth({}{}: sub {} judge {})",
                if synth_abstained { "abstain " } else { "" },
                format_args!("{:.2}s", synth_secs),
                if synth_sub_hit { "HIT" } else { "miss" },
                if synth_judge_hit { "HIT" } else { "miss" }
            )
        } else {
            String::new()
        };

        let concept_tag = if use_concepts {
            format!(" concept(n={concept_count}, {concept_secs:.1}s)")
        } else {
            String::new()
        };

        println!(
            "  [{:>3}/{}] {:24} {:9} {:>4} turns ext={:.1}s rec={:.2}s judge={:.2}s {}{}{} (n_results={}, gold={:?})",
            i + 1,
            n_run,
            item.question_id,
            item.question_type,
            n_turns,
            extract_secs,
            recall_secs,
            judge_secs,
            verdict,
            synth_tag,
            concept_tag,
            results.len(),
            item.answer
        );
    }

    let total_runner_secs = runner_t0.elapsed().as_secs_f64();
    let n_total_runs = (hits + misses).max(1);
    let hit_rate = hits as f32 / n_total_runs as f32;

    println!();
    println!(
        "TOTAL: hit_rate={:.3}  ({}/{} hits, {} miss)  wall={:.0}s  ext_total={:.0}s  rec_total={:.0}s  judge_total={:.0}s",
        hit_rate,
        hits,
        n_total_runs,
        misses,
        total_runner_secs,
        total_extraction_secs,
        total_recall_secs,
        total_judge_secs,
    );
    if use_llm_judge {
        let sub_rate = substring_hits as f32 / n_total_runs as f32;
        let judge_rate = judge_hits as f32 / n_total_runs as f32;
        println!(
            "       substring_rate={:.3} ({}/{}) — strict factoid metric (lower bound)",
            sub_rate, substring_hits, n_total_runs
        );
        println!(
            "       judge_rate    ={:.3} ({}/{}) — paraphrase-tolerant metric (LongMemEval-aligned)",
            judge_rate, judge_hits, n_total_runs
        );
    }
    if use_synth {
        let synth_sub_rate = synth_substring_hits as f32 / n_total_runs as f32;
        let synth_judge_rate = synth_judge_hits as f32 / n_total_runs as f32;
        let abstain_rate = synth_abstain_count as f32 / n_total_runs as f32;
        println!(
            "       synth_sub_rate    ={:.3} ({}/{}) — synthesized answer, strict substring",
            synth_sub_rate, synth_substring_hits, n_total_runs
        );
        println!(
            "       synth_judge_rate  ={:.3} ({}/{}) — synthesized answer, judge-graded (concept-page on-demand metric)",
            synth_judge_rate, synth_judge_hits, n_total_runs
        );
        println!(
            "       synth_abstain_rate={:.3} ({}/{}) — model emitted \"I don't know\" — high-quality silence",
            abstain_rate, synth_abstain_count, n_total_runs
        );
        println!("       synth_total_secs={:.0}s", total_synth_secs);
    }
    println!("\nBy question_type (primary metric):");
    let mut type_keys: Vec<_> = by_type.keys().cloned().collect();
    type_keys.sort();
    for k in type_keys {
        let (h, n) = by_type[&k];
        println!("  {:30} {:>2}/{:<2}  rate={:.2}", k, h, n, h as f32 / n as f32);
    }

    // AM-1.8: write metrics JSON for the nightly baseline-diff check
    // (`tests/check_baseline.rs` reads this and compares against
    // `tests/baselines/longmemeval-s.json`).
    if let Ok(path) = std::env::var("LONGMEMEVAL_RESULTS_JSON") {
        let metrics = serde_json::json!({
            "schema_version": 1,
            "benchmark": "longmemeval-s-18-balanced",
            "n_total_runs": n_total_runs,
            "metrics": {
                "hit_rate": hit_rate,
                "substring_rate": if use_llm_judge { substring_hits as f32 / n_total_runs as f32 } else { hit_rate },
                "raw_judge_rate": if use_llm_judge { judge_hits as f32 / n_total_runs as f32 } else { hit_rate },
                "synth_substring_rate": if use_synth { synth_substring_hits as f32 / n_total_runs as f32 } else { 0.0 },
                "synth_judge_rate": if use_synth { synth_judge_hits as f32 / n_total_runs as f32 } else { 0.0 },
                "synth_abstain_rate": if use_synth { synth_abstain_count as f32 / n_total_runs as f32 } else { 0.0 }
            },
            "per_category_synth_judge": by_type.iter().map(|(k, (h, n))| {
                (k.clone(), if *n > 0 { *h as f32 / *n as f32 } else { 0.0 })
            }).collect::<std::collections::BTreeMap<_, _>>(),
            "wall_secs": total_runner_secs,
            "extraction_secs": total_extraction_secs,
            "recall_secs": total_recall_secs,
            "synth_secs": total_synth_secs,
            "judge_secs": total_judge_secs,
        });
        match std::fs::write(&path, serde_json::to_vec_pretty(&metrics).unwrap()) {
            Ok(_) => eprintln!("[eval_longmemeval] wrote results to {path}"),
            Err(e) => eprintln!("[eval_longmemeval] failed to write {path}: {e}"),
        }
    }

    let floor = min_hit_rate();
    if floor > 0.0 {
        assert!(
            hit_rate >= floor,
            "LongMemEval hit_rate {:.3} below floor {:.3}",
            hit_rate,
            floor
        );
    }
    // Sanity floor: at least one hit. If all miss, the pipeline is wedged.
    assert!(
        hits > 0,
        "all {} items missed — extraction or recall is broken (not a quality gate)",
        n_total_runs
    );
}
