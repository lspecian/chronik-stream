//! Shared post-extraction logic across LLM providers.
//!
//! Every provider's tool-call output, once unwrapped, lands in the same shape:
//! `{ facts: [...], events: [...], instructions: [...], tasks: [...] }` (per
//! prompts/v2). This module owns the raw-struct definitions plus the
//! `filter_and_convert` pipeline that maps raw JSON → [`Extracted`] with
//! source-cite verification, calibration, and clamping applied.
//!
//! Provider-specific code (Anthropic / OpenAI / Ollama) is responsible only
//! for issuing the request, locating the tool-call output in the response, and
//! deserializing into [`RawToolInput`].

use crate::extractor::calibration::apply_calibration;
use crate::extractor::Extracted;
use crate::schema::{Body, EventBody, FactBody, InstructionBody, MemoryType, TaskBody, TaskState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Top-level shape a provider parses into. Empty arrays are valid (all four
/// fields default to `Vec::new()` so a v1 prompt that only fills `facts` and
/// `events` still parses cleanly).
#[allow(missing_docs)]
#[derive(Debug, Default, Deserialize)]
pub struct RawToolInput {
    #[serde(default)]
    pub facts: Vec<RawFact>,
    #[serde(default)]
    pub events: Vec<RawEvent>,
    #[serde(default)]
    pub instructions: Vec<RawInstruction>,
    #[serde(default)]
    pub tasks: Vec<RawTask>,
}

#[allow(missing_docs)]
#[derive(Debug, Deserialize, Serialize)]
pub struct RawFact {
    /// Subject the claim is about. Optional at the wire level so a single
    /// malformed fact (e.g. model emits `null`) doesn't fail the whole
    /// tool-call deserialization and lose every other fact in the batch.
    /// Empty / missing subjects are dropped in `filter_and_convert`.
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub subject: Option<String>,
    /// Predicate name. Same defensiveness as `subject`.
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub predicate: Option<String>,
    #[serde(default)]
    pub object: serde_json::Value,
    /// Human-readable rendering of the fact. Optional because some models
    /// (notably OpenAI gpt-4o-mini and Anthropic Claude on actor-symmetric
    /// extractions for third-party entities) emit `null` for the field.
    /// We synthesise a rendering from `subject` / `predicate` / `object`
    /// when missing, in [`filter_and_convert`].
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub text: Option<String>,
    #[serde(default)]
    pub source_indexes: Vec<usize>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

/// Deserialize a field that the schema marks as `string` but the model
/// occasionally emits as `null`. Without this, a single null in any of
/// our `String` fields causes serde to fail the whole `RawToolInput`
/// deserialization (and we lose every fact / event in the batch). Returns
/// `None` for null or missing, `Some(s)` otherwise.
fn deser_optional_string<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Null => None,
        // Non-string non-null (numbers, bools): coerce to string. Better
        // than dropping the fact entirely — `filter_and_convert` can still
        // use the (subject, predicate, object) triple.
        other => Some(other.to_string()),
    })
}

fn default_confidence() -> f32 {
    0.5
}

#[allow(missing_docs)]
#[derive(Debug, Deserialize, Serialize)]
pub struct RawEvent {
    /// Same null-defensive deserialization as `RawFact` — see
    /// [`deser_optional_string`]. Filter logic drops events with missing
    /// required fields rather than failing the whole batch.
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub actor: Option<String>,
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub verb: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub ts: Option<String>,
    #[serde(default)]
    pub source_indexes: Vec<usize>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

#[allow(missing_docs)]
#[derive(Debug, Deserialize, Serialize)]
pub struct RawInstruction {
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub scope: Option<String>,
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub rule: Option<String>,
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub trigger: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub source_indexes: Vec<usize>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

#[allow(missing_docs)]
#[derive(Debug, Deserialize, Serialize)]
pub struct RawTask {
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub task_id: Option<String>,
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "deser_optional_string")]
    pub state: Option<String>,
    #[serde(default)]
    pub due_at: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub source_indexes: Vec<usize>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

/// Map raw extractions to [`Extracted`], dropping any whose source indexes are
/// out of range. Confidence is calibrated and clamped to [0, 1].
///
/// Provider-agnostic — same logic for Anthropic, OpenAI, vLLM, Ollama.
pub fn filter_and_convert(
    raw: RawToolInput,
    n_turns: usize,
    extractor_version: &str,
) -> Vec<Extracted> {
    let mut out = Vec::with_capacity(
        raw.facts.len() + raw.events.len() + raw.instructions.len() + raw.tasks.len(),
    );

    for f in raw.facts {
        // Drop facts missing subject or predicate — without those, we can't
        // construct a key or even render the fact meaningfully. The wire-
        // level Optional<String> deserialization (above) means a null from
        // the model lands here as None instead of failing the whole batch.
        let (subject, predicate) = match (f.subject.as_ref(), f.predicate.as_ref()) {
            (Some(s), Some(p)) if !s.is_empty() && !p.is_empty() => (s.clone(), p.clone()),
            _ => {
                warn!(
                    subject = ?f.subject,
                    predicate = ?f.predicate,
                    "fact missing subject or predicate — dropping"
                );
                continue;
            }
        };
        if !valid_indexes(&f.source_indexes, n_turns) {
            warn!(
                ?f.source_indexes,
                n_turns,
                "fact cites out-of-range index — dropping"
            );
            continue;
        }
        // Synthesise `text` from `(subject predicate object)` when the model
        // didn't emit one. Strips JSON quoting on string-shaped objects so
        // the rendering reads as English, not as a JSON literal.
        let text = f.text.unwrap_or_else(|| {
            let obj_str = match &f.object {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            if obj_str.is_empty() {
                format!("{} {}", subject, predicate)
            } else {
                format!("{} {} {}", subject, predicate, obj_str)
            }
        });
        out.push(Extracted {
            body: Body::Fact(FactBody {
                subject: subject.clone(),
                predicate: predicate.clone(),
                object: f.object,
                polarity: "asserted".into(),
                text,
            }),
            key: Some(format!("{}|{}", subject, predicate)),
            confidence: apply_calibration(f.confidence, extractor_version, MemoryType::Fact),
            source_indexes: f.source_indexes,
        });
    }

    for ev in raw.events {
        if !valid_indexes(&ev.source_indexes, n_turns) {
            warn!(
                ?ev.source_indexes,
                n_turns,
                "event cites out-of-range index — dropping"
            );
            continue;
        }
        // Drop events with missing actor / verb / ts — same defensive pattern
        // as facts. Without these the event is structurally meaningless.
        let (actor, verb, ts_str) = match (ev.actor.as_ref(), ev.verb.as_ref(), ev.ts.as_ref()) {
            (Some(a), Some(v), Some(t))
                if !a.is_empty() && !v.is_empty() && !t.is_empty() =>
            {
                (a.clone(), v.clone(), t.clone())
            }
            _ => {
                warn!(
                    actor = ?ev.actor,
                    verb = ?ev.verb,
                    ts = ?ev.ts,
                    "event missing actor / verb / ts — dropping"
                );
                continue;
            }
        };
        let ts: DateTime<Utc> = match parse_ts(&ts_str) {
            Some(t) => t,
            None => {
                warn!(ts = %ts_str, "event has unparseable ts — dropping");
                continue;
            }
        };
        out.push(Extracted {
            body: Body::Event(EventBody {
                actor,
                verb,
                object: ev.object,
                channel: None,
                context: ev.context,
                ts,
            }),
            key: None,
            confidence: apply_calibration(ev.confidence, extractor_version, MemoryType::Event),
            source_indexes: ev.source_indexes,
        });
    }

    for inst in raw.instructions {
        if !valid_indexes(&inst.source_indexes, n_turns) {
            warn!(
                ?inst.source_indexes,
                n_turns,
                "instruction cites out-of-range index — dropping"
            );
            continue;
        }
        let (scope, rule, trigger) = match (
            inst.scope.as_ref(),
            inst.rule.as_ref(),
            inst.trigger.as_ref(),
        ) {
            (Some(s), Some(r), Some(t)) if !s.is_empty() && !r.is_empty() && !t.is_empty() => {
                (s.clone(), r.clone(), t.clone())
            }
            _ => {
                warn!(
                    scope = ?inst.scope,
                    rule = ?inst.rule,
                    trigger = ?inst.trigger,
                    "instruction missing scope / rule / trigger — dropping"
                );
                continue;
            }
        };
        let rule_hash = short_hash(&rule);
        let key = format!("{}|{}|{}", scope, trigger, rule_hash);
        out.push(Extracted {
            body: Body::Instruction(InstructionBody {
                scope,
                rule,
                trigger,
                priority: inst.priority,
            }),
            key: Some(key),
            confidence: apply_calibration(
                inst.confidence,
                extractor_version,
                MemoryType::Instruction,
            ),
            source_indexes: inst.source_indexes,
        });
    }

    for task in raw.tasks {
        if !valid_indexes(&task.source_indexes, n_turns) {
            warn!(
                ?task.source_indexes,
                n_turns,
                "task cites out-of-range index — dropping"
            );
            continue;
        }
        let (task_id, title, state_str) = match (
            task.task_id.as_ref(),
            task.title.as_ref(),
            task.state.as_ref(),
        ) {
            (Some(id), Some(t), Some(s)) if !id.is_empty() && !t.is_empty() && !s.is_empty() => {
                (id.clone(), t.clone(), s.clone())
            }
            _ => {
                warn!(
                    task_id = ?task.task_id,
                    title = ?task.title,
                    state = ?task.state,
                    "task missing task_id / title / state — dropping"
                );
                continue;
            }
        };
        let state = match parse_task_state(&state_str) {
            Some(s) => s,
            None => {
                warn!(state = %state_str, "task has unknown state — dropping");
                continue;
            }
        };
        let due_at = task.due_at.as_deref().and_then(parse_ts);
        out.push(Extracted {
            body: Body::Task(TaskBody {
                task_id: task_id.clone(),
                title,
                state,
                due_at,
                owner: task.owner,
                depends_on: vec![],
            }),
            key: Some(task_id),
            confidence: apply_calibration(task.confidence, extractor_version, MemoryType::Task),
            source_indexes: task.source_indexes,
        });
    }

    out
}

/// True when every cited index is in `[0, n_turns)` AND the citation list is non-empty.
pub fn valid_indexes(indexes: &[usize], n_turns: usize) -> bool {
    !indexes.is_empty() && indexes.iter().all(|&i| i < n_turns)
}

/// Parse an RFC-3339 timestamp string into a UTC `DateTime`. Returns `None`
/// for malformed input (the extractor pipeline drops the affected memory).
pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Map a JSON `state` string to a [`TaskState`]. Returns `None` for unknown
/// states (the extractor pipeline drops the affected task).
pub fn parse_task_state(s: &str) -> Option<TaskState> {
    match s {
        "open" => Some(TaskState::Open),
        "in_progress" => Some(TaskState::InProgress),
        "done" => Some(TaskState::Done),
        "cancelled" => Some(TaskState::Cancelled),
        _ => None,
    }
}

/// 12-hex-char SHA-256 prefix — short enough for keying, long enough that
/// collisions are vanishingly rare for the instruction-rule keying use case.
/// 12-hex-char SHA-256 prefix used for keying instructions on
/// `(scope, trigger, hash(rule))`.
pub fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    hex::encode(&digest[..6])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> RawToolInput {
        RawToolInput::default()
    }

    #[test]
    fn filter_drops_out_of_range_indexes() {
        let raw = RawToolInput {
            facts: vec![
                RawFact {
                    subject: Some("user".into()),
                    predicate: Some("p".into()),
                    object: serde_json::json!("o"),
                    text: Some("t".into()),
                    source_indexes: vec![0, 5],
                    confidence: 0.9,
                },
                RawFact {
                    subject: Some("user".into()),
                    predicate: Some("q".into()),
                    object: serde_json::json!("o"),
                    text: Some("t".into()),
                    source_indexes: vec![0],
                    confidence: 0.9,
                },
            ],
            ..empty()
        };
        let out = filter_and_convert(raw, 2, "any-v2");
        assert_eq!(out.len(), 1);
        match &out[0].body {
            Body::Fact(f) => assert_eq!(f.predicate, "q"),
            _ => panic!("wrong"),
        }
    }

    #[test]
    fn task_state_table() {
        assert_eq!(parse_task_state("open"), Some(TaskState::Open));
        assert_eq!(parse_task_state("in_progress"), Some(TaskState::InProgress));
        assert_eq!(parse_task_state("done"), Some(TaskState::Done));
        assert_eq!(parse_task_state("cancelled"), Some(TaskState::Cancelled));
        assert_eq!(parse_task_state("foo"), None);
    }

    #[test]
    fn short_hash_deterministic() {
        let a = short_hash("rule");
        let b = short_hash("rule");
        let c = short_hash("other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 12);
    }

    #[test]
    fn filter_drops_unparseable_event_ts() {
        let raw = RawToolInput {
            events: vec![RawEvent {
                actor: Some("a".into()),
                verb: Some("v".into()),
                object: None,
                context: None,
                ts: Some("not a date".into()),
                source_indexes: vec![0],
                confidence: 0.9,
            }],
            ..empty()
        };
        assert!(filter_and_convert(raw, 1, "any-v2").is_empty());
    }

    #[test]
    fn filter_drops_event_with_null_actor_or_verb() {
        // Anthropic v4 schema permits null on these fields; the parser must
        // drop the malformed record rather than emit a bogus event.
        let raw = RawToolInput {
            events: vec![
                RawEvent {
                    actor: None,
                    verb: Some("v".into()),
                    object: None,
                    context: None,
                    ts: Some("2026-04-25T10:00:00Z".into()),
                    source_indexes: vec![0],
                    confidence: 0.9,
                },
                RawEvent {
                    actor: Some("a".into()),
                    verb: None,
                    object: None,
                    context: None,
                    ts: Some("2026-04-25T10:00:00Z".into()),
                    source_indexes: vec![0],
                    confidence: 0.9,
                },
            ],
            ..empty()
        };
        assert!(filter_and_convert(raw, 1, "any-v2").is_empty());
    }

    #[test]
    fn filter_drops_empty_indexes() {
        let raw = RawToolInput {
            facts: vec![RawFact {
                subject: Some("user".into()),
                predicate: Some("p".into()),
                object: serde_json::json!("o"),
                text: Some("t".into()),
                source_indexes: vec![],
                confidence: 0.9,
            }],
            ..empty()
        };
        assert!(filter_and_convert(raw, 5, "any-v2").is_empty());
    }

    #[test]
    fn filter_clamps_confidence_above_one() {
        let raw = RawToolInput {
            facts: vec![RawFact {
                subject: Some("user".into()),
                predicate: Some("p".into()),
                object: serde_json::json!("o"),
                text: Some("t".into()),
                source_indexes: vec![0],
                confidence: 1.7, // out of range
            }],
            ..empty()
        };
        let out = filter_and_convert(raw, 1, "any-v2");
        assert_eq!(out.len(), 1);
        assert!((out[0].confidence - 1.0).abs() < 1e-6);
    }

    #[test]
    fn filter_synthesises_text_when_provider_omits_it() {
        // OpenAI gpt-4o-mini occasionally returns a fact without `text`
        // even though the JSON schema marks it required. We synthesise from
        // (subject predicate object) so the fact still lands.
        let raw = RawToolInput {
            facts: vec![RawFact {
                subject: Some("user".into()),
                predicate: Some("customer_id".into()),
                object: serde_json::json!("C-9182"),
                text: None,
                source_indexes: vec![0],
                confidence: 0.9,
            }],
            ..empty()
        };
        let out = filter_and_convert(raw, 1, "any-v2");
        assert_eq!(out.len(), 1);
        match &out[0].body {
            Body::Fact(f) => {
                assert_eq!(f.text, "user customer_id C-9182");
                assert_eq!(f.predicate, "customer_id");
            }
            _ => panic!("wrong body type"),
        }
    }

    #[test]
    fn filter_synthesises_text_strips_json_quoting_on_string_objects() {
        // Numeric / structural objects pass through their JSON literal,
        // string objects come through plain so the rendering reads as English.
        let raw = RawToolInput {
            facts: vec![
                RawFact {
                    subject: Some("user".into()),
                    predicate: Some("budget".into()),
                    object: serde_json::json!(800_000),
                    text: None,
                    source_indexes: vec![0],
                    confidence: 0.9,
                },
                RawFact {
                    subject: Some("user".into()),
                    predicate: Some("neighborhood".into()),
                    object: serde_json::json!("Lapa"),
                    text: None,
                    source_indexes: vec![0],
                    confidence: 0.9,
                },
            ],
            ..empty()
        };
        let out = filter_and_convert(raw, 1, "any-v2");
        assert_eq!(out.len(), 2);
        match &out[0].body {
            Body::Fact(f) => assert_eq!(f.text, "user budget 800000"),
            _ => panic!("wrong body type"),
        }
        match &out[1].body {
            Body::Fact(f) => assert_eq!(f.text, "user neighborhood Lapa"),
            _ => panic!("wrong body type"),
        }
    }

    #[test]
    fn filter_converts_instructions() {
        let raw = RawToolInput {
            instructions: vec![RawInstruction {
                scope: Some("agent".into()),
                rule: Some("Always reply in Portuguese.".into()),
                trigger: Some("before_reply".into()),
                priority: 5,
                source_indexes: vec![0],
                confidence: 0.9,
            }],
            ..empty()
        };
        let out = filter_and_convert(raw, 1, "any-v2");
        assert_eq!(out.len(), 1);
        match &out[0].body {
            Body::Instruction(i) => {
                assert_eq!(i.scope, "agent");
                assert_eq!(i.trigger, "before_reply");
                assert_eq!(i.priority, 5);
            }
            _ => panic!("wrong body variant"),
        }
        let key = out[0].key.as_ref().unwrap();
        assert!(key.starts_with("agent|before_reply|"));
        assert_eq!(key.len(), "agent|before_reply|".len() + 12);
    }

    #[test]
    fn filter_converts_tasks() {
        let raw = RawToolInput {
            tasks: vec![RawTask {
                task_id: Some("tsk_schedule_viewing".into()),
                title: Some("Schedule viewing".into()),
                state: Some("open".into()),
                due_at: Some("2026-04-26T09:00:00Z".into()),
                owner: Some("agent:bot".into()),
                source_indexes: vec![0],
                confidence: 0.85,
            }],
            ..empty()
        };
        let out = filter_and_convert(raw, 1, "any-v2");
        assert_eq!(out.len(), 1);
        match &out[0].body {
            Body::Task(t) => {
                assert_eq!(t.task_id, "tsk_schedule_viewing");
                assert_eq!(t.state, TaskState::Open);
                assert!(t.due_at.is_some());
            }
            _ => panic!("wrong body variant"),
        }
        assert_eq!(out[0].key.as_deref(), Some("tsk_schedule_viewing"));
    }

    #[test]
    fn filter_drops_task_with_unknown_state() {
        let raw = RawToolInput {
            tasks: vec![RawTask {
                task_id: Some("tsk_x".into()),
                title: Some("x".into()),
                state: Some("pending_review".into()),
                due_at: None,
                owner: None,
                source_indexes: vec![0],
                confidence: 0.9,
            }],
            ..empty()
        };
        assert!(filter_and_convert(raw, 1, "any-v2").is_empty());
    }
}
