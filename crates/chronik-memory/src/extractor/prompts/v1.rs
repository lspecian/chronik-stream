//! Extraction prompt v1 — facts + events, structured output via Anthropic tool-use
//! (or OpenAI function-calling in Phase 2). Caller forces the model to invoke
//! `record_memories` with arrays of facts and events, each citing source-turn
//! indexes.
//!
//! Tool name is constant across providers so the parsing layer is shared.

use crate::extractor::Turn;

/// Stable extractor version identifier — embedded in `source.extractor` for
/// every memory produced from this prompt revision. Bump when the prompt
/// semantics or tool schema change in a way that would affect calibration.
pub const EXTRACTOR_VERSION: &str = "anthropic-v1";

/// Tool name the model must call.
pub const TOOL_NAME: &str = "record_memories";

/// System prompt — instructs the model to extract only what is explicitly
/// stated, cite indexes, and never speculate.
pub const SYSTEM_PROMPT: &str = "\
You are an extraction engine. Given a list of conversation turns, you call the \
`record_memories` tool exactly once with structured arrays of facts and events.\n\
\n\
Rules:
- Each extraction must cite the index(es) of the turns it came from in `source_indexes`.
  Indexes refer to the bracketed numbers in the user message (e.g. [0], [1]).
- Only extract claims explicitly stated. Do NOT speculate or infer.
- Subject and actor identifiers should be lowercase, namespace-style (e.g. `user:luis`,
  `agent:bot`). When the turn role is `user` and no name is given, use `user`.
- Predicate names are snake_case relations (e.g. `prefers_neighborhood`, `has_email`,
  `seeks_property_type`).
- Confidence is in [0.0, 1.0] and reflects your certainty in the extraction.
- If a turn contains nothing extractable, do not produce a memory citing it.
- If the entire batch contains nothing extractable, call the tool with empty arrays.
- Never write text outside the tool call.";

/// JSON Schema for the `record_memories` tool input.
///
/// Returned as a `serde_json::Value` so it can be embedded into provider-specific
/// request bodies. Schema is provider-neutral — same shape works for Anthropic
/// tool-use and OpenAI function-calling.
pub fn tool_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "facts": {
                "type": "array",
                "description": "Atomic supersedeable knowledge claims (subject, predicate, object).",
                "items": {
                    "type": "object",
                    "properties": {
                        "subject": {"type": "string", "description": "Entity the claim is about (lowercase namespace-style id)."},
                        "predicate": {"type": "string", "description": "Relation type (snake_case)."},
                        "object": {"description": "Value of the claim — string, number, or null."},
                        "text": {"type": "string", "description": "Free-text rendering of the claim."},
                        "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}, "description": "Indexes of source turns. Must be valid indexes into the input."},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
                    },
                    "required": ["subject", "predicate", "object", "text", "source_indexes", "confidence"]
                }
            },
            "events": {
                "type": "array",
                "description": "Time-stamped append-only occurrences.",
                "items": {
                    "type": "object",
                    "properties": {
                        "actor": {"type": "string"},
                        "verb": {"type": "string", "description": "Snake_case action name."},
                        "object": {"type": ["string", "null"], "description": "Optional target."},
                        "context": {"type": ["string", "null"], "description": "Optional free-text context."},
                        "ts": {"type": "string", "description": "RFC-3339 timestamp."},
                        "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
                    },
                    "required": ["actor", "verb", "ts", "source_indexes", "confidence"]
                }
            }
        },
        "required": ["facts", "events"]
    })
}

/// Render the user message that the model sees — bracketed turn list.
pub fn render_user_message(turns: &[Turn]) -> String {
    let mut s = String::with_capacity(turns.iter().map(|t| t.content.len() + 32).sum::<usize>());
    s.push_str("Conversation turns:\n");
    for (i, t) in turns.iter().enumerate() {
        s.push_str(&format!("[{}] {}: {}\n", i, t.role, t.content));
    }
    s.push_str("\nNow call record_memories.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(role: &str, content: &str) -> Turn {
        Turn {
            role: role.into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: None,
        }
    }

    #[test]
    fn user_message_indexes_each_turn() {
        let m = render_user_message(&[t("user", "hi"), t("assistant", "hello")]);
        assert!(m.contains("[0] user: hi"));
        assert!(m.contains("[1] assistant: hello"));
        assert!(m.ends_with("call record_memories."));
    }

    #[test]
    fn empty_batch_still_renders_marker() {
        let m = render_user_message(&[]);
        assert!(m.starts_with("Conversation turns:"));
        assert!(m.ends_with("call record_memories."));
    }

    #[test]
    fn schema_has_required_top_level_fields() {
        let s = tool_input_schema();
        let req = s["required"].as_array().unwrap();
        let names: Vec<_> = req.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"facts"));
        assert!(names.contains(&"events"));
    }
}
