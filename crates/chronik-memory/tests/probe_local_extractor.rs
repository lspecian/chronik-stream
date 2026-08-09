//! Throwaway diagnostic: run the REAL OpenAIExtractor (V3Lite) against a live
//! LM Studio endpoint with the real answer-bearing chunk from LongMemEval
//! item e47becba, and report what survives `filter_and_convert`.
//!
//! Run: `LMS=http://LMS_HOST:1234 cargo test -p chronik-memory --test \
//! probe_local_extractor -- --ignored --nocapture`

use chronik_memory::extractor::providers::openai::{OpenAIExtractor, OpenAIPromptVersion};
use chronik_memory::extractor::{Extractor, Turn};

#[tokio::test]
#[ignore = "requires live LM Studio endpoint (LMS env var)"]
async fn probe_chunk10_extraction() {
    let endpoint = match std::env::var("LMS") {
        Ok(e) => e,
        Err(_) => {
            eprintln!("set LMS=http://host:port");
            return;
        }
    };
    // Surface the extractor's tracing warns (dropped facts, invalid indexes).
    let _ = tracing_subscriber::fmt()
        .with_env_filter("chronik_memory=debug")
        .try_init();

    let path = std::env::var("LONGMEMEVAL_PATH")
        .unwrap_or_else(|_| "datasets/longmemeval_s_500.jsonl".to_string());
    let raw = std::fs::read_to_string(&path).expect("dataset");
    // Select the DOCUMENTED item e47becba, not whatever happens to be first in
    // the file — otherwise the probe reports on an unrelated conversation.
    let item: serde_json::Value = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["question_id"].as_str() == Some("e47becba"))
        .expect("item e47becba not found in dataset");

    // Flatten turns exactly like the harness does.
    let mut turns: Vec<Turn> = Vec::new();
    for session in item["haystack_sessions"].as_array().unwrap() {
        for rc in session.as_array().unwrap() {
            let role = rc["role"].as_str().unwrap_or("").trim();
            let content = rc["content"].as_str().unwrap_or("").trim();
            if role.is_empty() || content.is_empty() {
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
    let chunk: Vec<Turn> = turns[500..550.min(turns.len())].to_vec();
    eprintln!("chunk turns: {}", chunk.len());

    let model =
        std::env::var("LMS_MODEL").unwrap_or_else(|_| "qwen/qwen3-coder-30b".to_string());
    let ex = OpenAIExtractor::for_local_server(&endpoint, &model)
        .with_max_tokens(8192)
        .with_prompt_version(OpenAIPromptVersion::V3Lite);

    let t0 = std::time::Instant::now();
    // Propagate errors — a failed extraction must fail the probe, not pass it.
    let extracted = ex.extract(&chunk).await.expect("extraction failed");
    eprintln!("extracted {} memories in {:?}", extracted.len(), t0.elapsed());
    for e in extracted.iter().take(5) {
        eprintln!("  sample: {:?}", e.body);
    }
    // Verify the reported result: the answer-bearing chunk must yield the
    // degree fact. Absence is a real regression, not a diagnostic to print.
    let has_degree = extracted
        .iter()
        .any(|e| format!("{:?}", e.body).contains("Business Administration"));
    assert!(
        has_degree,
        "expected 'Business Administration' degree fact not extracted from item e47becba chunk"
    );
}
