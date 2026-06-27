//! Deterministic rule-pass extractor — regex over conversation turns for cheap,
//! high-precision extractions.
//!
//! Currently emits:
//! - **Email** → `fact{predicate=has_email}` keyed by `subject:user|has_email`.
//! - **Phone (E.164-ish)** → `fact{predicate=has_phone}`.
//! - **URL** → `fact{predicate=mentioned_url}`.
//! - **Date (ISO-8601)** → `event{verb=mentioned_date, ts=...}`.
//!
//! All rule extractions get a baseline confidence of 0.6 — the LLM-pass can
//! produce higher-confidence facts that supersede via the same key.
//!
//! Subjects are derived from the turn's `role` (e.g. `user:role` becomes
//! `user`). For events, the actor is also derived from the role.

use crate::error::Result;
use crate::extractor::{Extracted, Extractor, Turn};
use crate::schema::{Body, EventBody, FactBody};
use async_trait::async_trait;
use chrono::Utc;
use regex::Regex;
use std::sync::OnceLock;

/// Baseline confidence for rule-pass extractions.
const RULE_CONFIDENCE: f32 = 0.6;

/// Stateless deterministic extractor.
#[derive(Debug, Clone, Default)]
pub struct RuleExtractor;

impl RuleExtractor {
    /// Construct a new extractor.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for RuleExtractor {
    fn id(&self) -> &str {
        "rules@v1"
    }

    async fn extract(&self, turns: &[Turn]) -> Result<Vec<Extracted>> {
        let mut out = Vec::new();
        for (idx, t) in turns.iter().enumerate() {
            let subject = role_to_subject(&t.role);

            for m in email_re().find_iter(&t.content) {
                out.push(make_fact(
                    &subject,
                    "has_email",
                    serde_json::json!(m.as_str()),
                    format!("{} has email {}", subject, m.as_str()),
                    idx,
                ));
            }

            for m in phone_re().find_iter(&t.content) {
                out.push(make_fact(
                    &subject,
                    "has_phone",
                    serde_json::json!(m.as_str()),
                    format!("{} has phone {}", subject, m.as_str()),
                    idx,
                ));
            }

            for m in url_re().find_iter(&t.content) {
                out.push(make_fact(
                    &subject,
                    "mentioned_url",
                    serde_json::json!(m.as_str()),
                    format!("{} mentioned {}", subject, m.as_str()),
                    idx,
                ));
            }

            for m in iso_date_re().find_iter(&t.content) {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(m.as_str()) {
                    out.push(Extracted {
                        body: Body::Event(EventBody {
                            actor: subject.clone(),
                            verb: "mentioned_date".into(),
                            object: Some(m.as_str().to_string()),
                            channel: t.channel.clone(),
                            context: Some(truncate(&t.content, 200)),
                            ts: dt.with_timezone(&Utc),
                        }),
                        key: None,
                        confidence: RULE_CONFIDENCE,
                        source_indexes: vec![idx],
                    });
                }
            }
        }
        Ok(out)
    }
}

/// Derive a subject identifier from a role string.
///
/// Convention: roles like `user`, `assistant`, `system` map to themselves.
/// Anything else passes through. Callers wanting fully-qualified subjects
/// (e.g. `user:luis`) should provide them via the LLM-pass extractor — the
/// rule-pass doesn't have enough context to disambiguate between users.
fn role_to_subject(role: &str) -> String {
    role.to_string()
}

fn make_fact(
    subject: &str,
    predicate: &str,
    object: serde_json::Value,
    text: String,
    src_idx: usize,
) -> Extracted {
    Extracted {
        body: Body::Fact(FactBody {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object,
            polarity: "asserted".into(),
            text,
        }),
        key: Some(format!("{}|{}", subject, predicate)),
        confidence: RULE_CONFIDENCE,
        source_indexes: vec![src_idx],
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t = s.chars().take(max).collect::<String>();
        t.push('…');
        t
    }
}

// ---- compiled regexes ----

fn email_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
            .expect("email regex")
    })
}

fn phone_re() -> &'static Regex {
    // E.164-ish: optional +, 7-15 digits with optional separators (spaces, dashes, parens).
    // Conservative — we want precision, not recall, in the rule pass.
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?:^|\s|\()(\+?\d{1,3}[\s\-]?)?\(?\d{2,4}\)?[\s\-]?\d{3,4}[\s\-]?\d{3,4}\b")
            .expect("phone regex")
    })
}

fn url_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"https?://[^\s<>'"]+"#).expect("url regex"))
}

fn iso_date_re() -> &'static Regex {
    // ISO-8601 / RFC-3339 dates with optional time. Tolerant but not exhaustive.
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})\b",
        )
        .expect("iso date regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: &str, content: &str) -> Turn {
        Turn {
            role: role.into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: None,
        }
    }

    #[tokio::test]
    async fn extracts_email() {
        let e = RuleExtractor::new();
        let out = e
            .extract(&[turn("user", "ping me at luis@example.com later")])
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        match &out[0].body {
            Body::Fact(f) => {
                assert_eq!(f.predicate, "has_email");
                assert_eq!(f.object, serde_json::json!("luis@example.com"));
            }
            _ => panic!("wrong body"),
        }
        assert_eq!(out[0].source_indexes, vec![0]);
        assert!((out[0].confidence - RULE_CONFIDENCE).abs() < 1e-6);
    }

    #[tokio::test]
    async fn extracts_url_and_iso_date() {
        let e = RuleExtractor::new();
        let out = e
            .extract(&[turn(
                "user",
                "see https://example.com and 2026-04-25T18:00:00Z",
            )])
            .await
            .unwrap();
        // Should find one URL fact and one date event.
        let n_facts = out.iter().filter(|x| matches!(x.body, Body::Fact(_))).count();
        let n_events = out.iter().filter(|x| matches!(x.body, Body::Event(_))).count();
        assert_eq!(n_facts, 1, "exactly one URL fact");
        assert_eq!(n_events, 1, "exactly one date event");
    }

    #[tokio::test]
    async fn ignores_non_matching_text() {
        let e = RuleExtractor::new();
        let out = e
            .extract(&[turn("user", "I prefer south-facing apartments.")])
            .await
            .unwrap();
        assert!(
            out.is_empty(),
            "rule extractor should not match free-form text: {:?}",
            out
        );
    }

    #[tokio::test]
    async fn cite_indexes_track_input_position() {
        let e = RuleExtractor::new();
        let turns = vec![
            turn("user", "hello"),
            turn("user", "see https://x.com"),
            turn("user", "world"),
        ];
        let out = e.extract(&turns).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source_indexes, vec![1]);
    }

    #[tokio::test]
    async fn id_is_stable() {
        let e = RuleExtractor::new();
        assert_eq!(e.id(), "rules@v1");
    }

    #[test]
    fn truncate_helper() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello…");
    }
}
