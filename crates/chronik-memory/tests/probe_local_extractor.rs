//! Throwaway diagnostic: run the REAL OpenAIExtractor (V3Lite) against a live
//! LM Studio endpoint with the real answer-bearing chunk from LongMemEval
//! item e47becba, and report what survives `filter_and_convert`.
//!
//! Run: `LMS=http://192.168.1.169:1234 cargo test -p chronik-memory --test \
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
    let first_line = raw.lines().next().expect("first item");
    let item: serde_json::Value = serde_json::from_str(first_line).expect("json");

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
    match ex.extract(&chunk).await {
        Ok(extracted) => {
            eprintln!(
                "extracted {} memories in {:?}",
                extracted.len(),
                t0.elapsed()
            );
            let mut has_degree = false;
            for e in &extracted {
                if format!("{:?}", e.body).contains("Business Administration") {
                    has_degree = true;
                }
            }
            eprintln!("degree fact present: {}", has_degree);
            for e in extracted.iter().take(5) {
                eprintln!("  sample: {:?}", e.body);
            }
        }
        Err(e) => eprintln!("EXTRACTION ERROR: {e}"),
    }
}
