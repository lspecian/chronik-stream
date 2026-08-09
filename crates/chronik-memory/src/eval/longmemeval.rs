//! LongMemEval adapter (AMS-2.8).
//!
//! Converts items from the [LongMemEval](https://github.com/xiaowu0162/LongMemEval)
//! dataset into the SDK's [`Fixture`](crate::eval::Fixture) format so the
//! existing eval harness can run against it. LongMemEval is a 500-question
//! recall-quality benchmark across 5 reasoning categories — primary target
//! for the SDK's recall NDCG numbers per the roadmap.
//!
//! # Dataset shape
//!
//! Each LongMemEval item has the form:
//!
//! ```jsonc
//! {
//!   "question_id": "single_session_1",
//!   "question_type": "single-session",
//!   "haystack_sessions": [
//!     [
//!       {"role": "user", "content": "..."},
//!       {"role": "assistant", "content": "..."}
//!     ],
//!     /* more sessions... */
//!   ],
//!   "question": "What is the user's preferred neighborhood?",
//!   "answer": "Lapa"
//! }
//! ```
//!
//! # Scoring model
//!
//! LongMemEval is end-to-end QA — there are no per-memory labels for an
//! NDCG/MRR computation against extracted memories. Two evaluation modes:
//!
//! 1. **`AnswerMatch`** (recommended for LongMemEval): substring or
//!    case-insensitive match between the gold `answer` and the synthesized
//!    text content of the top-k retrieved memories. Returns `1.0` for hit,
//!    `0.0` for miss.
//! 2. **`KeysMatch`**: requires the caller to populate
//!    `Fixture::recall_queries[0].expected_keys` separately (e.g. by hand-
//!    labelling the dataset). Falls back to NDCG/P@5/MRR.
//!
//! The adapter sets up the conversation and the question; the caller picks
//! the scorer.
//!
//! # Usage
//!
//! ```no_run
//! use chronik_memory::eval::longmemeval::{LongMemEvalItem, parse_jsonl};
//! use std::fs;
//!
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Download the dataset to e.g. ./longmemeval/longmemeval_s.jsonl from
//! // https://github.com/xiaowu0162/LongMemEval, then:
//! let raw = fs::read_to_string("./longmemeval/longmemeval_s.jsonl")?;
//! let items: Vec<LongMemEvalItem> = parse_jsonl(&raw)?;
//! for item in items.iter().take(10) {
//!     let fixture = item.to_fixture();
//!     println!("{} — {} turns", fixture.name, fixture.conversation.len());
//! }
//! # Ok(())
//! # }
//! ```

use crate::eval::{Fixture, FixtureTurn, RecallQuery};
use serde::{Deserialize, Deserializer, Serialize};

/// One LongMemEval question + its multi-session haystack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LongMemEvalItem {
    /// Unique id within the dataset (e.g. `single_session_1`).
    pub question_id: String,

    /// One of the dataset's 5 reasoning categories: `single-session-user`,
    /// `single-session-preference`, `multi-session`, `knowledge-update`,
    /// `temporal-reasoning`, `abstention` (and minor variants).
    #[serde(default)]
    pub question_type: String,

    /// Sessions, each a list of `(role, content)` turns. Total length across
    /// all sessions can be hundreds of turns for the long variants.
    pub haystack_sessions: Vec<Vec<RoleContent>>,

    /// The user's question to answer with retrieved memory.
    pub question: String,

    /// Gold answer — used by [`answer_match`] to grade retrieval.
    ///
    /// In LongMemEval-S the answer is sometimes a number (`"how many doctor
    /// visits did I go to?" → 2`) and sometimes a string. We deserialize as
    /// `serde_json::Value` and stringify at use time so both shapes round-
    /// trip without dataset-specific massaging.
    #[serde(deserialize_with = "deser_answer_as_string")]
    pub answer: String,

    /// Optional date string per the LongMemEval-S variant — pass-through.
    #[serde(default)]
    pub answer_session_ids: Vec<String>,

    /// Per-session date strings, parallel to `haystack_sessions`
    /// (format: `"2023/05/20 (Sat) 02:21"`). Empty on datasets without dates.
    /// WS-3.2 (ROADMAP_MEMORY_QUALITY.md): threaded into `Turn.ts` so the
    /// source excerpt carries real session dates for temporal reasoning.
    #[serde(default)]
    pub haystack_dates: Vec<String>,

    /// The date the question is asked (same format as `haystack_dates`).
    /// Temporal questions ("how many days ago...") are anchored to this
    /// date, not to the eval's wall clock.
    #[serde(default)]
    pub question_date: Option<String>,
}

/// Parse a LongMemEval date string (`"2023/05/20 (Sat) 02:21"`) into a UTC
/// timestamp. The weekday token is redundant — strip it before parsing.
/// Returns `None` on any malformed input (defensive: dataset artifacts).
pub fn parse_longmemeval_date(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveDateTime, TimeZone, Utc};
    // "2023/05/20 (Sat) 02:21" → "2023/05/20 02:21"
    let cleaned: String = {
        let mut out = String::with_capacity(s.len());
        let mut in_paren = false;
        for c in s.chars() {
            match c {
                '(' => in_paren = true,
                ')' => in_paren = false,
                _ if !in_paren => out.push(c),
                _ => {}
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let naive = NaiveDateTime::parse_from_str(&cleaned, "%Y/%m/%d %H:%M").ok()?;
    Some(Utc.from_utc_datetime(&naive))
}

/// Plain `(role, content)` pair as it appears in LongMemEval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleContent {
    /// Speaker role — `"user"` or `"assistant"`.
    pub role: String,
    /// Message content.
    pub content: String,
}

impl LongMemEvalItem {
    /// Convert to a [`Fixture`]. Conversation flattens all sessions in order;
    /// `recall_queries` contains one entry with the question and an empty
    /// `expected_keys` list — caller scores via [`answer_match`] instead of
    /// the SDK's default NDCG path.
    pub fn to_fixture(&self) -> Fixture {
        let mut conversation: Vec<FixtureTurn> = Vec::new();
        for session in &self.haystack_sessions {
            for rc in session {
                conversation.push(FixtureTurn {
                    role: rc.role.clone(),
                    content: rc.content.clone(),
                    ts: None,
                });
            }
        }
        Fixture {
            name: format!("longmemeval-{}", self.question_id),
            description: format!(
                "LongMemEval [{}] — {} sessions, {} turns",
                self.question_type,
                self.haystack_sessions.len(),
                conversation.len()
            ),
            conversation,
            expected_memories: vec![],
            recall_queries: vec![RecallQuery {
                query: self.question.clone(),
                expected_keys: vec![],
                k: 10,
            }],
            negative_assertions: None,
        }
    }
}

/// Custom deserializer for the `answer` field — accepts string or number
/// (LongMemEval-S has both shapes), renders to a string for [`answer_match`].
fn deser_answer_as_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        // Arrays / objects: stringify whole. LongMemEval doesn't ship these
        // shapes today but tolerate them so we don't fail on a future variant.
        other => other.to_string(),
    })
}

/// Parse a JSONL (newline-delimited JSON) file's contents into a Vec of items.
/// Empty lines and lines starting with `#` are skipped.
pub fn parse_jsonl(src: &str) -> Result<Vec<LongMemEvalItem>, serde_json::Error> {
    let mut out = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        out.push(serde_json::from_str(trimmed)?);
    }
    Ok(out)
}

/// Substring-match scorer for end-to-end QA.
///
/// Returns `1.0` if the gold `answer` is a case-insensitive substring of any
/// of the retrieved text strings, else `0.0`. Used when the caller wants
/// LongMemEval's native QA-match metric rather than NDCG@k against labeled
/// keys.
///
/// `retrieved_texts` is typically `result.memory.body` rendered to text — a
/// helper [`render_memory_text`] is provided for the common case.
pub fn answer_match(retrieved_texts: &[String], gold_answer: &str) -> f32 {
    let needle = gold_answer.to_lowercase();
    if needle.trim().is_empty() {
        return 0.0;
    }
    for hay in retrieved_texts {
        if hay.to_lowercase().contains(&needle) {
            return 1.0;
        }
    }
    0.0
}

/// LLM-judge scorer for end-to-end QA — paraphrase-tolerant alternative
/// to the strict substring [`answer_match`].
///
/// The substring matcher works for factoid answers (`"800000"`, `"C-9182"`)
/// but is structurally wrong for LongMemEval categories where the gold
/// answer is a paraphrased synthesis (e.g. single-session-preference's
/// "The user would prefer documentaries similar to ..."). Those categories
/// score 0 under substring matching even when the right facts were
/// retrieved. LongMemEval's official metric uses an LLM judge for exactly
/// this reason — this is the SDK's port of that protocol.
///
/// Returns `1.0` if the judge says any retrieved text answers the
/// question correctly given the gold answer; `0.0` otherwise. The judge
/// is the caller-supplied [`TextGenerator`] (typically the same Anthropic
/// or OpenAI extractor used elsewhere in the eval; pass via
/// `with_text_generator` or build a dedicated one).
///
/// **Cost**: one LLM call per item per eval run. At Claude Haiku 4.5
/// pricing (~$0.001 per call for typical prompt sizes), a 500-item run
/// adds ~$0.50 over the substring path.
pub async fn answer_match_llm(
    retrieved_texts: &[String],
    question: &str,
    gold_answer: &str,
    judge: &dyn crate::embeddings::TextGenerator,
) -> f32 {
    if gold_answer.trim().is_empty() {
        return 0.0;
    }
    if retrieved_texts.is_empty() {
        // No memories to judge against — no need to pay for an LLM call.
        return 0.0;
    }
    let prompt = build_judge_prompt(retrieved_texts, question, gold_answer);
    let raw = match judge.complete(&prompt).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "answer_match_llm: judge LLM failed; treating as miss");
            return 0.0;
        }
    };
    parse_judge_score(&raw)
}

/// Build the judge prompt. Format chosen to keep tokens small and parsing
/// trivial: ask for a one-token verdict, accept any of `1` / `0` / `yes` /
/// `no` / `correct` / `incorrect` (case-insensitive) at the start of the
/// response.
fn build_judge_prompt(
    retrieved_texts: &[String],
    question: &str,
    gold_answer: &str,
) -> String {
    // Cap each retrieved snippet to keep the prompt under ~3 KB; the judge
    // doesn't need full bodies, just enough to grade against the question.
    const MAX_SNIPPET_CHARS: usize = 280;
    const MAX_SNIPPETS: usize = 10;
    let mut bullets = String::new();
    for (i, t) in retrieved_texts.iter().take(MAX_SNIPPETS).enumerate() {
        let trimmed: String = t.chars().take(MAX_SNIPPET_CHARS).collect();
        bullets.push_str(&format!("[{}] {}\n", i + 1, trimmed));
    }
    format!(
        "You are grading whether a retrieval system surfaced the answer to a question.\n\
\n\
Question: {question}\n\
\n\
Gold answer (correct): {gold_answer}\n\
\n\
Retrieved memories (any of these is a hit if it lets a reader answer the question correctly given the gold):\n\
{bullets}\n\
Reply with exactly one token: `1` if the gold answer is supported by any retrieved memory, `0` if not. \
Treat paraphrases and synonyms as equivalent — exact wording is not required, only the underlying \
fact / preference / event / instruction.\n\
\n\
Your verdict:"
    )
}

/// Parse the judge's first-token verdict. Tolerant of common variations.
fn parse_judge_score(raw: &str) -> f32 {
    let trimmed = raw.trim().to_lowercase();
    // Strip leading punctuation / quotes the model sometimes adds.
    let cleaned: String = trimmed
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_string();
    let first = cleaned.split_whitespace().next().unwrap_or("");
    let first = first.trim_end_matches(|c: char| !c.is_alphanumeric());
    match first {
        "1" | "yes" | "y" | "true" | "correct" | "hit" | "supported" => 1.0,
        "0" | "no" | "n" | "false" | "incorrect" | "miss" | "unsupported" => 0.0,
        _ => {
            tracing::warn!(verdict = %trimmed, "answer_match_llm: judge returned unparseable verdict; treating as miss");
            0.0
        }
    }
}

/// Render a memory body into a single text string suitable for QA-style
/// substring matching. Concatenates the most informative fields per type.
pub fn render_memory_text(body: &crate::schema::Body) -> String {
    use crate::schema::Body::*;
    match body {
        Fact(f) => format!(
            "{}: {} {} {}",
            f.text,
            f.subject,
            f.predicate,
            serde_json::to_string(&f.object).unwrap_or_default()
        ),
        Event(e) => format!(
            "{} {} {} {}",
            e.actor,
            e.verb,
            e.object.as_deref().unwrap_or(""),
            e.context.as_deref().unwrap_or("")
        ),
        Instruction(i) => format!("{} {} {}", i.scope, i.trigger, i.rule),
        Task(t) => format!("{} {:?} {}", t.title, t.state, t.task_id),
        Concept(c) => format!("{} {} {}", c.title, c.entity_type, c.markdown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, EventBody, FactBody, InstructionBody, TaskBody, TaskState};
    use chrono::Utc;

    #[test]
    fn parses_longmemeval_date_format() {
        let ts = parse_longmemeval_date("2023/05/20 (Sat) 02:21").expect("parse");
        assert_eq!(ts.to_rfc3339(), "2023-05-20T02:21:00+00:00");
    }

    #[test]
    fn longmemeval_date_rejects_garbage() {
        assert!(parse_longmemeval_date("").is_none());
        assert!(parse_longmemeval_date("not a date").is_none());
        assert!(parse_longmemeval_date("2023-05-20T02:21:00Z").is_none());
    }

    fn synthetic_item() -> LongMemEvalItem {
        LongMemEvalItem {
            question_id: "test_q1".into(),
            haystack_dates: vec![],
            question_date: None,
            question_type: "single-session-preference".into(),
            haystack_sessions: vec![
                vec![
                    RoleContent {
                        role: "user".into(),
                        content: "I want a 3BR in Lapa, max 800k euros.".into(),
                    },
                    RoleContent {
                        role: "assistant".into(),
                        content: "Got it.".into(),
                    },
                ],
                vec![RoleContent {
                    role: "user".into(),
                    content: "Actually, prefer south-facing.".into(),
                }],
            ],
            question: "What is the user's max budget?".into(),
            answer: "800000".into(),
            answer_session_ids: vec![],
        }
    }

    #[test]
    fn item_to_fixture_flattens_sessions() {
        let item = synthetic_item();
        let f = item.to_fixture();
        assert_eq!(f.name, "longmemeval-test_q1");
        // 2 turns from session 0 + 1 from session 1 = 3 turns
        assert_eq!(f.conversation.len(), 3);
        assert_eq!(f.recall_queries.len(), 1);
        assert_eq!(f.recall_queries[0].query, "What is the user's max budget?");
        // No labeled keys — caller scores via answer_match
        assert!(f.recall_queries[0].expected_keys.is_empty());
        assert!(f.expected_memories.is_empty());
    }

    #[test]
    fn parse_jsonl_skips_blanks_and_comments() {
        let src = "\n# this is a comment\n# another\n\n";
        let items = parse_jsonl(src).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn parse_jsonl_round_trips_real_item() {
        let item = synthetic_item();
        let line = serde_json::to_string(&item).unwrap();
        let parsed = parse_jsonl(&line).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].question_id, "test_q1");
        assert_eq!(parsed[0].haystack_sessions.len(), 2);
    }

    #[test]
    fn parse_handles_numeric_answer() {
        // Real LongMemEval-S items occasionally ship `"answer": 2` for
        // counting questions ("how many doctor visits in March?"). The
        // adapter must accept both string and numeric answers.
        let src = r#"{"question_id":"q","question_type":"multi-session","haystack_sessions":[],"question":"how many?","answer":2,"answer_session_ids":[]}"#;
        let items = parse_jsonl(src).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].answer, "2");
    }

    #[test]
    fn parse_handles_string_answer() {
        let src = r#"{"question_id":"q","question_type":"single-session-user","haystack_sessions":[],"question":"what?","answer":"Lapa","answer_session_ids":[]}"#;
        let items = parse_jsonl(src).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].answer, "Lapa");
    }

    #[test]
    fn parse_jsonl_handles_multiple_lines() {
        let item = synthetic_item();
        let one = serde_json::to_string(&item).unwrap();
        let multi = format!("{one}\n{one}\n# comment\n{one}");
        let parsed = parse_jsonl(&multi).unwrap();
        assert_eq!(parsed.len(), 3);
    }

    #[test]
    fn answer_match_finds_substring() {
        let retrieved = vec![
            "Luis prefers Lapa".to_string(),
            "Max budget is 800000 euros".to_string(),
        ];
        assert_eq!(answer_match(&retrieved, "800000"), 1.0);
        assert_eq!(answer_match(&retrieved, "Lapa"), 1.0);
    }

    #[test]
    fn answer_match_case_insensitive() {
        let retrieved = vec!["Luis prefers LAPA".to_string()];
        assert_eq!(answer_match(&retrieved, "lapa"), 1.0);
    }

    #[test]
    fn answer_match_misses_when_absent() {
        let retrieved = vec!["totally different content".to_string()];
        assert_eq!(answer_match(&retrieved, "Lapa"), 0.0);
    }

    #[test]
    fn answer_match_empty_gold_returns_zero() {
        let retrieved = vec!["something".to_string()];
        assert_eq!(answer_match(&retrieved, ""), 0.0);
        assert_eq!(answer_match(&retrieved, "   "), 0.0);
    }

    #[test]
    fn render_fact_includes_subject_and_object() {
        let body = Body::Fact(FactBody {
            subject: "user".into(),
            predicate: "max_budget_eur".into(),
            object: serde_json::json!(800000),
            polarity: "asserted".into(),
            text: "User has max budget 800k EUR".into(),
                speaker: "user".into(),
        });
        let s = render_memory_text(&body);
        assert!(s.contains("max_budget_eur"));
        assert!(s.contains("800000"));
    }

    #[test]
    fn render_event_includes_actor_and_verb() {
        let body = Body::Event(EventBody {
            actor: "user:luis".into(),
            verb: "stated_budget".into(),
            object: Some("800000 EUR".into()),
            channel: None,
            context: Some("max budget".into()),
            ts: Utc::now(),
        });
        let s = render_memory_text(&body);
        assert!(s.contains("user:luis"));
        assert!(s.contains("stated_budget"));
        assert!(s.contains("800000"));
    }

    #[test]
    fn render_instruction_renders_rule() {
        let body = Body::Instruction(InstructionBody {
            scope: "agent:bot".into(),
            rule: "Reply in Portuguese".into(),
            trigger: "before_reply".into(),
            priority: 0,
        });
        let s = render_memory_text(&body);
        assert!(s.contains("Reply in Portuguese"));
        assert!(s.contains("before_reply"));
    }

    #[test]
    fn render_task_includes_title() {
        let body = Body::Task(TaskBody {
            task_id: "tsk_x".into(),
            title: "Schedule viewing".into(),
            state: TaskState::Open,
            due_at: None,
            owner: None,
            depends_on: vec![],
        });
        let s = render_memory_text(&body);
        assert!(s.contains("Schedule viewing"));
    }

    // ---- LLM-judge unit tests (judge LLM not invoked — just the parser) ----

    #[test]
    fn parse_judge_score_accepts_one_token_yes() {
        assert_eq!(parse_judge_score("1"), 1.0);
        assert_eq!(parse_judge_score("yes"), 1.0);
        assert_eq!(parse_judge_score("Yes"), 1.0);
        assert_eq!(parse_judge_score("correct"), 1.0);
        assert_eq!(parse_judge_score("hit"), 1.0);
    }

    #[test]
    fn parse_judge_score_accepts_one_token_no() {
        assert_eq!(parse_judge_score("0"), 0.0);
        assert_eq!(parse_judge_score("no"), 0.0);
        assert_eq!(parse_judge_score("incorrect"), 0.0);
        assert_eq!(parse_judge_score("miss"), 0.0);
    }

    #[test]
    fn parse_judge_score_strips_leading_punctuation() {
        // Models often wrap the verdict in markdown / quotes.
        assert_eq!(parse_judge_score("**1**"), 1.0);
        assert_eq!(parse_judge_score("`yes`"), 1.0);
        assert_eq!(parse_judge_score("\"0\""), 0.0);
    }

    #[test]
    fn parse_judge_score_takes_first_token_only() {
        // If the model writes "1. Because the retrieved fact says ...",
        // we accept the leading 1.
        assert_eq!(parse_judge_score("1. The retrieved fact matches."), 1.0);
        assert_eq!(parse_judge_score("yes — supported by snippet [3]"), 1.0);
    }

    #[test]
    fn parse_judge_score_unparseable_is_miss() {
        // Defensive: when the model rambles without a clear yes/no,
        // we treat as miss (false negatives are safer than false positives
        // in eval reporting).
        assert_eq!(parse_judge_score("hmm, not sure honestly"), 0.0);
        assert_eq!(parse_judge_score(""), 0.0);
    }

    #[test]
    fn build_judge_prompt_caps_snippets_and_chars() {
        let long_text = "x".repeat(1000);
        let retrieved = vec![long_text.clone(); 20];
        let prompt = build_judge_prompt(&retrieved, "what?", "answer");
        // 280-char cap × 10 snippets means each snippet contributes <= 290
        // chars (incl. bracketed index). Total is far less than the
        // unconstrained 20×1000.
        assert!(
            prompt.len() < 6000,
            "prompt is {} chars; should cap snippets to keep token cost bounded",
            prompt.len()
        );
        // Question + gold must always be in the prompt.
        assert!(prompt.contains("what?"));
        assert!(prompt.contains("answer"));
    }
}
