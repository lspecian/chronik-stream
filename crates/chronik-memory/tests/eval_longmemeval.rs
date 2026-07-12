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
//!
//! # Full 500-item pass split across 10 parallel processes (each runs 50):
//! LONGMEMEVAL_PATH=/path/to/longmemeval_s.jsonl \
//!   LONGMEMEVAL_N=50 LONGMEMEVAL_SHARDS=10 LONGMEMEVAL_SHARD_INDEX=0 \
//!   ANTHROPIC_API_KEY=... CHRONIK_INTEGRATION=1 \
//!   cargo test -p chronik-memory --test eval_longmemeval -- --ignored --nocapture
//! # ... repeat with SHARD_INDEX=1,2,...,9 in parallel; aggregate per-shard reports.
//!
//! # Just the temporal category (reproducing a regression):
//! LONGMEMEVAL_CATEGORIES=temporal,temporal-reasoning \
//!   LONGMEMEVAL_PATH=/path/to/longmemeval_s.jsonl \
//!   ANTHROPIC_API_KEY=... CHRONIK_INTEGRATION=1 \
//!   cargo test -p chronik-memory --test eval_longmemeval -- --ignored --nocapture
//!
//! # Skip specific known-broken items while everything else runs:
//! LONGMEMEVAL_SKIP_ITEMS=q0042,q0057 \
//!   LONGMEMEVAL_PATH=/path/to/longmemeval_s.jsonl \
//!   ANTHROPIC_API_KEY=... CHRONIK_INTEGRATION=1 \
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
    Memory, MemoryType, OpenAIExtractor, PromptVersion, RuleExtractor, TwoPassExtractor,
};

/// Returns the LLM factory for this run. When
/// `LONGMEMEVAL_LLM_PROVIDER=local` and `LONGMEMEVAL_LLM_ENDPOINT` +
/// `LONGMEMEVAL_LLM_MODEL` are set, uses an OpenAI-compat local server
/// (LM Studio, vLLM, llama.cpp, etc.). Otherwise falls back to Anthropic.
fn llm_provider_choice() -> LlmProvider {
    provider_from_var("LONGMEMEVAL_LLM_PROVIDER")
}

/// Provider used for EXTRACTION specifically. Defaults to
/// `LONGMEMEVAL_EXTRACTION_PROVIDER` when set, else falls back to the main
/// `LONGMEMEVAL_LLM_PROVIDER`. Decoupling matters when synth/judge run on a
/// local model (free, unlimited) but extraction should stay on the cached
/// cloud extractor: the extraction cache is keyed by the extractor id
/// (`chain[rules@v1+openai-v3]`), so keeping extraction on `openai` replays
/// the paid gpt-4o-mini facts from disk with ZERO API calls, while
/// `LONGMEMEVAL_LLM_PROVIDER=local` sends only synth+judge to Ollama.
fn extraction_provider_choice() -> LlmProvider {
    if std::env::var("LONGMEMEVAL_EXTRACTION_PROVIDER")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        provider_from_var("LONGMEMEVAL_EXTRACTION_PROVIDER")
    } else {
        llm_provider_choice()
    }
}

fn provider_from_var(var: &str) -> LlmProvider {
    let provider = std::env::var(var).unwrap_or_default();
    match provider.as_str() {
        "local" => {
            let endpoint = std::env::var("LONGMEMEVAL_LLM_ENDPOINT")
                .expect("LONGMEMEVAL_LLM_PROVIDER=local requires LONGMEMEVAL_LLM_ENDPOINT");
            let model = std::env::var("LONGMEMEVAL_LLM_MODEL")
                .expect("LONGMEMEVAL_LLM_PROVIDER=local requires LONGMEMEVAL_LLM_MODEL");
            eprintln!("LLM provider: local (endpoint={}, model={})", endpoint, model);
            LlmProvider::Local { endpoint, model }
        }
        "openai" => {
            let model = std::env::var("LONGMEMEVAL_LLM_MODEL")
                .unwrap_or_else(|_| "gpt-4o-mini".to_string());
            eprintln!("LLM provider: openai (model={})", model);
            LlmProvider::OpenAI { model }
        }
        _ => {
            eprintln!("LLM provider: anthropic (default)");
            LlmProvider::Anthropic
        }
    }
}

#[derive(Clone)]
enum LlmProvider {
    Anthropic,
    Local { endpoint: String, model: String },
    OpenAI { model: String },
}

impl LlmProvider {
    /// Build a TextGenerator (for judge / synth) — uses `complete` only.
    fn build_generator(
        &self,
        api_key: &str,
    ) -> Arc<dyn chronik_memory::embeddings::TextGenerator> {
        match self {
            LlmProvider::Anthropic => Arc::new(AnthropicExtractor::new(api_key.to_string())),
            LlmProvider::Local { endpoint, model } => Arc::new(
                OpenAIExtractor::for_local_server(endpoint, model).with_max_tokens(1024),
            ),
            LlmProvider::OpenAI { model } => Arc::new(
                OpenAIExtractor::new(api_key.to_string()).with_model(model),
            ),
        }
    }

    /// Build an Extractor (for ingest_with_extraction).
    ///
    /// Local models get the condensed V3-lite prompt: the full V3's ~13K
    /// chars of canonicalization rules make 30B-class models under-extract
    /// (probed 2026-07-05: 0 facts under V3, 7 under a short prompt on the
    /// same chunk). Cloud models keep full V3.
    fn build_extractor(
        &self,
        api_key: &str,
        prompt_version: PromptVersion,
    ) -> Arc<dyn Extractor> {
        match self {
            LlmProvider::Anthropic => Arc::new(
                AnthropicExtractor::new(api_key.to_string())
                    .with_prompt_version(prompt_version),
            ),
            LlmProvider::Local { endpoint, model } => Arc::new(
                OpenAIExtractor::for_local_server(endpoint, model)
                    .with_max_tokens(8192)
                    .with_prompt_version(
                        chronik_memory::extractor::providers::openai::OpenAIPromptVersion::V3Lite,
                    ),
            ),
            LlmProvider::OpenAI { model } => Arc::new(
                OpenAIExtractor::new(api_key.to_string()).with_model(model),
            ),
        }
    }
}
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

/// Sharding for parallel execution: split the dataset into
/// `LONGMEMEVAL_SHARDS` disjoint slices and run only shard
/// `LONGMEMEVAL_SHARD_INDEX` in this process (0-indexed). Items are
/// assigned round-robin so every shard sees a comparable category mix.
/// Defaults `(1, 0)` → the whole dataset, no sharding.
fn shard_config() -> (usize, usize) {
    let shards = std::env::var("LONGMEMEVAL_SHARDS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1);
    let index = std::env::var("LONGMEMEVAL_SHARD_INDEX")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    (shards, index.min(shards.saturating_sub(1)))
}

/// Optional comma-separated allow-list of `question_type` values to
/// keep — everything else is filtered out. Useful for reproducing a
/// regression in one category without paying for the full 500-item pass.
/// Empty / unset = no filter.
fn category_filter() -> Option<Vec<String>> {
    let raw = std::env::var("LONGMEMEVAL_CATEGORIES").ok()?;
    let cats: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cats.is_empty() {
        None
    } else {
        Some(cats)
    }
}

/// Optional comma-separated skip-list of `question_id` values —
/// e.g. `LONGMEMEVAL_SKIP_ITEMS=q0042,q0057` to work around known-broken
/// items while the rest of the dataset runs. Empty / unset = no skips.
fn skip_list() -> std::collections::HashSet<String> {
    std::env::var("LONGMEMEVAL_SKIP_ITEMS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Apply category filter, skip list, and shard slicing to the raw
/// dataset. Returns the subset the caller should evaluate.
///
/// Ordering: (a) categories filtered → (b) skip-list applied → (c)
/// shard slicing (round-robin) → (d) `LONGMEMEVAL_N` cap. This means
/// `LONGMEMEVAL_N=10` under a 3-shard split gives 10 *per shard*, so
/// each parallel process runs the same fixed amount of work.
///
/// Non-shard callers pass `(1, 0)` and get the whole (filtered) set
/// with the `LONGMEMEVAL_N` cap applied — identical to the pre-refactor
/// behaviour when no shard / filter env vars are set.
fn select_items<'a>(
    all: &'a [LongMemEvalItem],
    shards: usize,
    shard_index: usize,
    n_cap: usize,
    categories: Option<&[String]>,
    skips: &std::collections::HashSet<String>,
) -> Vec<&'a LongMemEvalItem> {
    let mut filtered: Vec<&LongMemEvalItem> = all
        .iter()
        .filter(|item| {
            if let Some(cats) = categories {
                if !cats.iter().any(|c| c == &item.question_type) {
                    return false;
                }
            }
            if skips.contains(&item.question_id) {
                return false;
            }
            true
        })
        .collect();
    if shards > 1 {
        filtered = filtered
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % shards == shard_index)
            .map(|(_, item)| item)
            .collect();
    }
    filtered.into_iter().take(n_cap).collect()
}

fn item_to_turns(item: &LongMemEvalItem) -> Vec<Turn> {
    use chronik_memory::eval::longmemeval::parse_longmemeval_date;
    let mut turns = Vec::new();
    for (si, session) in item.haystack_sessions.iter().enumerate() {
        // WS-3.2: thread the session date into every turn of the session so
        // the WS-0 source excerpt carries a real date and synthesis can do
        // temporal arithmetic. NOTE: intentionally NOT written to the
        // memory's `valid_from` — 2023-dated valid_from would push events
        // through ~36 decay half-lives and destroy their ranking (bi-temporal
        // ranking is Sprint 3).
        let session_ts = item
            .haystack_dates
            .get(si)
            .and_then(|d| parse_longmemeval_date(d));
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
                ts: session_ts,
                channel: None,
                external_id: None,
            });
        }
    }
    turns
}

/// Poll Chronik's `/_search` until this item's typed facts are visible in
/// the fact topic's index, so recall measures retrieval quality rather than
/// indexing lag. Readiness = hits for this item's unique namespace reach
/// `max(1, total_typed/3)` (a third is plenty — recall's k is far smaller).
/// `floor_ms` is always waited (cold-index cycle floor); polling then runs
/// every 5s up to `timeout_ms`. Items that extracted nothing skip polling.
async fn wait_until_indexed(
    api: &str,
    tenant: &str,
    namespace: &str,
    total_typed: usize,
    floor_ms: u64,
    timeout_ms: u64,
    question_id: &str,
    wait_vector: bool,
) {
    tokio::time::sleep(Duration::from_millis(floor_ms)).await;
    if total_typed == 0 {
        return;
    }
    // Readiness bar: at least a third of the extracted facts, capped at 10 —
    // recall's k is far smaller than 10 per channel anyway. CRITICAL: Chronik's
    // ES-compat `hits.total` reflects the RETURNED hits array, not the matched
    // count, so `size: 0` always reads 0 (this bug burned the first sprint-3c
    // launch). Request `size: need` and count the actual hits array.
    let need = std::cmp::min(std::cmp::max(1, total_typed / 3), 10) as u64;
    let client = reqwest::Client::new();
    let url = format!("{}/_search", api.trim_end_matches('/'));
    // Match on the namespace's unique trailing ULID only. A match query on
    // the full namespace would OR its tokens and count hits from every item
    // sharing the tenant prefix, making readiness trivially (and wrongly)
    // true. The ULID is a single unique token.
    let unique_token = namespace.rsplit(':').next().unwrap_or(namespace);
    let body = serde_json::json!({
        "index": format!("mem.fact.{}", tenant),
        "query": {"match": {"_all": unique_token}},
        "size": need
    });
    let t0 = Instant::now();
    // Per-node visibility: the cluster serves /_search from whichever pod the
    // ClusterIP picks, and hot-index visibility differs per node until the
    // cold indexer catches up everywhere. One positive probe only proves ONE
    // node is ready (the sprint-3c litmus failed exactly this way: readiness
    // passed via the leader, recall hit a stale peer and got 0). Require
    // three consecutive positive probes, 5s apart, so with 3 server pods the
    // odds of recall landing on a stale node are negligible.
    let mut consecutive = 0u32;
    loop {
        let hits: u64 = match client.post(&url).json(&body).send().await {
            Ok(resp) => resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v["hits"]["hits"].as_array().map(|a| a.len() as u64))
                .unwrap_or(0),
            Err(_) => 0,
        };
        if hits >= need {
            consecutive += 1;
            if consecutive >= 3 {
                break;
            }
        } else {
            consecutive = 0;
        }
        if t0.elapsed().as_millis() as u64 + floor_ms >= timeout_ms {
            eprintln!(
                "  [warn] item {} index-readiness timed out: {}/{} facts visible \
                 after {}s — recall may under-measure",
                question_id,
                hits,
                need,
                (t0.elapsed().as_millis() as u64 + floor_ms) / 1000
            );
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // Sprint 5: vector-liveness gate. Text (STEP 2) and embeddings (STEP 3)
    // run in the SAME WalIndexer `index_segment` pass, so once this item's
    // text is 3×-confirmed above the segment's embeddings have almost
    // certainly completed too. What the text gate CANNOT catch is the whole
    // channel silently dead — s4c ran 500 items with `total_vectors:0` and
    // nobody noticed because recall degraded to text-only. This guard makes
    // that failure loud: require the fact topic's HNSW index to be non-empty
    // before recall runs.
    //
    // CRITICAL: poll `/_vector/.../search`, NOT `/_vector/.../stats`. The stats
    // endpoint is LOCAL-only (never fans out), so behind the round-robin
    // ClusterIP with per-node vector indexes it reports total_vectors=0 on the
    // 2/3 of nodes that aren't the partition leader — which stalls the gate
    // 300s/item even though search works fine. `/search` runs the same fan-out
    // as real recall (needs_fan_out → fan_out_post → merge), so its
    // total_vectors is the cluster-wide count and matches what recall sees.
    if wait_vector {
        let search_url = format!(
            "{}/_vector/mem.fact.{}/search",
            api.trim_end_matches('/'),
            tenant
        );
        // Any query works — the gate only reads total_vectors from the response,
        // which the handler fills from the fanned-out cluster-wide count. Reuse
        // the item's unique namespace token so we don't embed anything exotic.
        let probe = serde_json::json!({ "query": unique_token, "k": 1 });
        let tv0 = Instant::now();
        loop {
            let total: u64 = match client.post(&search_url).json(&probe).send().await {
                Ok(resp) => resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["total_vectors"].as_u64())
                    .unwrap_or(0),
                Err(_) => 0,
            };
            if total > 0 {
                return;
            }
            if tv0.elapsed().as_millis() as u64 >= timeout_ms {
                eprintln!(
                    "  [warn] item {} vector-index liveness timed out: total_vectors=0 \
                     after {}s — vector channel may be dead, recall degrading to text-only",
                    question_id,
                    tv0.elapsed().as_millis() as u64 / 1000
                );
                return;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

/// WS-3.2: anchor temporal questions to the question's own date instead of
/// the eval wall clock. LongMemEval golds like "7 days ago" are computed
/// relative to `question_date`; without the anchor the synthesizer has no
/// "today" to subtract from.
fn anchored_question(item: &LongMemEvalItem) -> String {
    match &item.question_date {
        Some(d) if !d.trim().is_empty() => {
            format!("(Today is {}.) {}", d, item.question)
        }
        _ => item.question.clone(),
    }
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + CHRONIK_INTEGRATION=1 + live cluster"]
async fn evaluate_longmemeval() {
    if std::env::var("CHRONIK_INTEGRATION").ok().as_deref() != Some("1") {
        eprintln!("skipping: set CHRONIK_INTEGRATION=1 to run");
        return;
    }
    // Provider-specific credential lookup:
    // - "local"  → LM Studio / vLLM (placeholder api_key "local")
    // - "openai" → OpenAI (reads OPENAI_API_KEY)
    // - default  → Anthropic (reads ANTHROPIC_API_KEY)
    let provider_env = std::env::var("LONGMEMEVAL_LLM_PROVIDER").unwrap_or_default();
    let api_key = match provider_env.as_str() {
        "local" => std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| "local".to_string()),
        "openai" => match std::env::var("OPENAI_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => {
                eprintln!("skipping: OPENAI_API_KEY not set");
                return;
            }
        },
        _ => match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => {
                eprintln!("skipping: ANTHROPIC_API_KEY not set");
                return;
            }
        },
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
    let n_cap = item_limit();
    let (shards, shard_index) = shard_config();
    let categories = category_filter();
    let skips = skip_list();
    let items: Vec<&LongMemEvalItem> = select_items(
        &all_items,
        shards,
        shard_index,
        n_cap,
        categories.as_deref(),
        &skips,
    );
    let n_run = items.len();
    println!(
        "\nLongMemEval — running {} / {} items from {:?}",
        n_run, n_total, path
    );
    if shards > 1 {
        println!(
            "  shard {}/{} (LONGMEMEVAL_SHARDS={}, LONGMEMEVAL_SHARD_INDEX={})",
            shard_index + 1,
            shards,
            shards,
            shard_index
        );
    }
    if let Some(cats) = &categories {
        println!("  category filter: {}", cats.join(","));
    }
    if !skips.is_empty() {
        println!("  skip list ({}): {:?}", skips.len(), skips);
    }

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
    let llm_provider = llm_provider_choice();
    // Extraction provider is decoupled: with `LONGMEMEVAL_LLM_PROVIDER=local`
    // for synth/judge, set `LONGMEMEVAL_EXTRACTION_PROVIDER=openai` to keep
    // extraction on the cached gpt-4o-mini path (id `openai-v3`, model-
    // independent) — cache hits replay facts from disk with zero API calls.
    let extraction_provider = extraction_provider_choice();

    let use_llm_judge = std::env::var("LONGMEMEVAL_USE_LLM_JUDGE")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    // The judge LLM uses the same provider as the extractor by default —
    // `TextGenerator::complete` for judge-graded scoring.
    let judge: Option<Arc<dyn chronik_memory::embeddings::TextGenerator>> = if use_llm_judge {
        Some(llm_provider.build_generator(&api_key))
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
    // The SYNTH model may be overridden independently of the judge via
    // `LONGMEMEVAL_SYNTH_MODEL` (local provider only). This isolates the
    // synth-model variable: hold the judge at the baseline model (e.g.
    // gemma3:4b, keeping synth_judge_rate comparable to the E4 0.546 anchor)
    // while swapping ONLY the answer-generating model. Without this split,
    // changing LONGMEMEVAL_LLM_MODEL would move the judge too and confound the
    // result (the E5 mistake). Defaults to `llm_provider` when unset.
    let synth_provider = match &llm_provider {
        LlmProvider::Local { endpoint, .. } => {
            match std::env::var("LONGMEMEVAL_SYNTH_MODEL") {
                Ok(m) if !m.trim().is_empty() => {
                    eprintln!("SYNTH model override: local (endpoint={endpoint}, model={m}) — judge stays on baseline model");
                    LlmProvider::Local { endpoint: endpoint.clone(), model: m }
                }
                _ => llm_provider.clone(),
            }
        }
        other => other.clone(),
    };
    let synth_gen: Option<Arc<dyn chronik_memory::embeddings::TextGenerator>> = if use_synth {
        Some(synth_provider.build_generator(&api_key))
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
    // Vector recall channel — OFF by default. When enabled, recall requests the
    // server's vector channel, which embeds the query. If the cluster's
    // embedding provider is unreachable/quota-dead, that embed call retries for
    // ~16s PER ITEM before degrading to BM25 — pure waste when no HNSW index
    // exists (vector.enabled=false). Vector was also shown not to be the lever
    // (s12=0.262 < 0.336). So gate it: opt in via LONGMEMEVAL_USE_VECTOR=1.
    let use_vector = std::env::var("LONGMEMEVAL_USE_VECTOR")
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
        let base_pass1: Arc<dyn Extractor> = extraction_provider.build_extractor(&api_key, prompt_version);
        let inner_extractor: Arc<dyn Extractor> = if use_two_pass {
            Arc::new(
                TwoPassExtractor::new(base_pass1, api_key.clone())
                    .expect("build TwoPassExtractor"),
            )
        } else {
            base_pass1
        };
        // Extraction cache (cost control): LONGMEMEVAL_EXTRACTION_CACHE=<dir>
        // replays previously-computed extractions instead of calling the LLM.
        // Wraps the whole chain (rules + LLM) so a hit costs zero API calls.
        // The cache key embeds the chain id (provider + prompt version), so
        // extractor/prompt changes re-extract automatically. ~$12-13 of a
        // ~$15 fleet run is extraction — cached re-runs cost ~$1.50.
        let chain: Arc<dyn Extractor> = Arc::new(ChainedExtractor::new(vec![
            Arc::new(RuleExtractor::new()),
            inner_extractor,
        ]));
        let extractor: Arc<dyn Extractor> = match std::env::var("LONGMEMEVAL_EXTRACTION_CACHE") {
            Ok(dir) if !dir.trim().is_empty() => Arc::new(
                chronik_memory::extractor::cached::CachedExtractor::new(chain, dir)
                    .expect("extraction cache dir"),
            ),
            _ => chain,
        };
        let mem = Memory::builder()
            .chronik_kafka(kafka.clone())
            .chronik_api(api.clone())
            .namespace(&ns)
            .extractor_arc(extractor)
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
        let mut total_typed_acks = 0usize;
        for (chunk_idx, chunk) in turns.chunks(batch_size).enumerate() {
            match mem.ingest_with_extraction(chunk.to_vec()).await {
                Ok(ack) => {
                    total_typed_acks += ack.typed_acks.len();
                    eprintln!(
                        "  [dbg] item {} chunk {} OK: raw_acks={} typed_acks={}",
                        item.question_id, chunk_idx,
                        ack.raw_acks.len(), ack.typed_acks.len()
                    );
                }
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

        // Index-readiness wait. The sprint-3 fleet (2026-07-04) proved a
        // fixed sleep is a race: with ~2× fact volume the cluster's indexing
        // lag exceeded 45s halfway through the run and empty recalls went
        // 17/250 → 172/250 between the first and second half — measuring
        // indexing lag instead of recall quality. Poll until this item's own
        // facts are actually searchable (search the fact topic for this
        // item's unique namespace), with `index_sleep_ms` acting as a floor
        // and `LONGMEMEVAL_INDEX_TIMEOUT_MS` (default 300s) as the cap.
        let index_timeout_ms: u64 = std::env::var("LONGMEMEVAL_INDEX_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300_000);
        // Sprint 5: when vector search is enabled, also gate on the HNSW index
        // being non-empty so a dead embedding channel fails loud rather than
        // silently degrading recall to text-only (the s4c blind-run trap).
        let wait_vector = matches!(
            std::env::var("LONGMEMEVAL_WAIT_VECTOR").as_deref(),
            Ok("1") | Ok("true") | Ok("on")
        );
        wait_until_indexed(
            &api,
            mem.tenant(),
            mem.namespace(),
            total_typed_acks,
            index_sleep_ms,
            index_timeout_ms,
            &item.question_id,
            wait_vector,
        )
        .await;

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
        let mut rb = mem
            .recall(&item.question)
            .types(&[MemoryType::Fact, MemoryType::Event, MemoryType::Instruction, MemoryType::Task]);
        if use_vector {
            rb = rb.with_vector();
        }
        let results = rb
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
                // WS-3.2: recall with the plain question (retrieval keys are
                // date-agnostic), but the anchored question ("Today is X. ...")
                // reaches the synthesis prompt via the builder's query, giving
                // the model a "today" to compute "N days ago" against.
                let mut srb = mem
                    .recall(anchored_question(item))
                    .types(&[
                        MemoryType::Fact,
                        MemoryType::Event,
                        MemoryType::Instruction,
                        MemoryType::Task,
                    ]);
                if use_vector {
                    srb = srb.with_vector();
                }
                let synth_res = srb
                    .with_key_match()
                    .include_concepts(use_concepts)
                    // Sprint 3: k=15 (was 10). Multi-session questions lost 34
                    // raw hits at k=10 — the answer-bearing memories from the
                    // other sessions ranked 11-15. Excerpts keep the prompt
                    // well within gpt-4o-mini's context at k=15.
                    .k(15)
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

// ─────────────────────────────────────────────────────────────
// Unit tests for the selector — these run under `cargo test` even
// without `--ignored`, so shard/filter logic is protected by CI.
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod selector_tests {
    use super::*;
    use chronik_memory::eval::longmemeval::LongMemEvalItem;
    use std::collections::HashSet;

    fn make_item(id: &str, qtype: &str) -> LongMemEvalItem {
        LongMemEvalItem {
            question_id: id.into(),
            question_type: qtype.into(),
            question: format!("q for {id}"),
            answer: "gold".into(),
            haystack_sessions: vec![],
            answer_session_ids: Vec::new(),
            haystack_dates: Vec::new(),
            question_date: None,
        }
    }

    fn corpus() -> Vec<LongMemEvalItem> {
        vec![
            make_item("q0", "single-session-user"),
            make_item("q1", "single-session-assistant"),
            make_item("q2", "temporal"),
            make_item("q3", "knowledge-update"),
            make_item("q4", "single-session-user"),
            make_item("q5", "temporal"),
            make_item("q6", "multi-session"),
            make_item("q7", "temporal-abs"),
            make_item("q8", "single-session-user"),
        ]
    }

    #[test]
    fn no_filter_no_shard_returns_full_dataset_up_to_cap() {
        let items = corpus();
        let skips = HashSet::new();
        let picked = select_items(&items, 1, 0, usize::MAX, None, &skips);
        assert_eq!(picked.len(), items.len());
    }

    #[test]
    fn n_cap_takes_first_n() {
        let items = corpus();
        let skips = HashSet::new();
        let picked = select_items(&items, 1, 0, 3, None, &skips);
        assert_eq!(picked.len(), 3);
        assert_eq!(picked[0].question_id, "q0");
        assert_eq!(picked[2].question_id, "q2");
    }

    #[test]
    fn category_filter_keeps_only_matching_types() {
        let items = corpus();
        let skips = HashSet::new();
        let cats = vec!["temporal".to_string()];
        let picked = select_items(&items, 1, 0, usize::MAX, Some(&cats), &skips);
        assert_eq!(picked.len(), 2);
        assert!(picked.iter().all(|i| i.question_type == "temporal"));
    }

    #[test]
    fn multi_category_filter_is_union() {
        let items = corpus();
        let skips = HashSet::new();
        let cats = vec!["temporal".to_string(), "multi-session".to_string()];
        let picked = select_items(&items, 1, 0, usize::MAX, Some(&cats), &skips);
        assert_eq!(picked.len(), 3, "2 temporal + 1 multi-session");
    }

    #[test]
    fn skip_list_removes_named_items() {
        let items = corpus();
        let skips: HashSet<String> = ["q0", "q4"].iter().map(|s| s.to_string()).collect();
        let picked = select_items(&items, 1, 0, usize::MAX, None, &skips);
        assert_eq!(picked.len(), 7);
        assert!(picked.iter().all(|i| i.question_id != "q0"
            && i.question_id != "q4"));
    }

    #[test]
    fn shard_slices_round_robin_and_is_deterministic() {
        let items = corpus();
        let skips = HashSet::new();
        let shard0 = select_items(&items, 3, 0, usize::MAX, None, &skips);
        let shard1 = select_items(&items, 3, 1, usize::MAX, None, &skips);
        let shard2 = select_items(&items, 3, 2, usize::MAX, None, &skips);
        assert_eq!(shard0.len(), 3, "9 items / 3 shards = 3 each");
        assert_eq!(shard1.len(), 3);
        assert_eq!(shard2.len(), 3);
        // Round-robin: shard k gets indexes {k, k+shards, k+2*shards, ...}
        assert_eq!(shard0[0].question_id, "q0");
        assert_eq!(shard0[1].question_id, "q3");
        assert_eq!(shard0[2].question_id, "q6");
        assert_eq!(shard1[0].question_id, "q1");
        assert_eq!(shard2[0].question_id, "q2");
        // All shards union = full dataset, no overlap.
        let mut all: Vec<String> = shard0
            .iter()
            .chain(shard1.iter())
            .chain(shard2.iter())
            .map(|i| i.question_id.clone())
            .collect();
        all.sort();
        let mut expected: Vec<String> = corpus().iter().map(|i| i.question_id.clone()).collect();
        expected.sort();
        assert_eq!(all, expected);
    }

    #[test]
    fn shard_with_n_cap_applies_per_shard() {
        let items = corpus();
        let skips = HashSet::new();
        // 9 items, 3 shards, cap 2 → each shard runs at most 2.
        let shard0 = select_items(&items, 3, 0, 2, None, &skips);
        assert_eq!(shard0.len(), 2, "N cap applies AFTER shard slicing");
    }

    #[test]
    fn category_filter_and_shard_compose() {
        let items = corpus();
        let skips = HashSet::new();
        let cats = vec!["single-session-user".to_string()];
        // 3 items match, split across 2 shards.
        let shard0 = select_items(&items, 2, 0, usize::MAX, Some(&cats), &skips);
        let shard1 = select_items(&items, 2, 1, usize::MAX, Some(&cats), &skips);
        assert_eq!(shard0.len() + shard1.len(), 3);
        // The union of shards under the filter equals the filter output.
        let mut combined: Vec<String> = shard0
            .iter()
            .chain(shard1.iter())
            .map(|i| i.question_id.clone())
            .collect();
        combined.sort();
        assert_eq!(combined, vec!["q0", "q4", "q8"]);
    }

    #[test]
    fn skip_list_and_category_and_shard_compose() {
        let items = corpus();
        let skips: HashSet<String> = ["q0"].iter().map(|s| s.to_string()).collect();
        let cats = vec!["single-session-user".to_string()];
        // Match "single-session-user" AND not in skips → q4, q8.
        let picked = select_items(&items, 1, 0, usize::MAX, Some(&cats), &skips);
        let ids: Vec<String> = picked.iter().map(|i| i.question_id.clone()).collect();
        assert_eq!(ids, vec!["q4".to_string(), "q8".to_string()]);
    }

    #[test]
    fn empty_dataset_returns_empty_regardless_of_filters() {
        let items: Vec<LongMemEvalItem> = Vec::new();
        let skips = HashSet::new();
        assert!(select_items(&items, 1, 0, 100, None, &skips).is_empty());
        assert!(select_items(&items, 4, 2, 100, Some(&["x".into()]), &skips).is_empty());
    }

    #[test]
    fn shard_index_clamped_by_config_helper() {
        // shard_config() clamps: passing SHARD_INDEX >= SHARDS should
        // return the last valid shard.
        std::env::set_var("LONGMEMEVAL_SHARDS", "3");
        std::env::set_var("LONGMEMEVAL_SHARD_INDEX", "5");
        let (shards, index) = shard_config();
        assert_eq!(shards, 3);
        assert_eq!(index, 2, "out-of-range index clamps to last shard");
        std::env::remove_var("LONGMEMEVAL_SHARDS");
        std::env::remove_var("LONGMEMEVAL_SHARD_INDEX");
    }
}
