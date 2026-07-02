//! Extraction prompt v2 — emits **all four memory types** (fact, event,
//! instruction, task) in a single tool call. Default for Phase 2 extraction.
//!
//! Differences from v1:
//! - Adds `instructions` and `tasks` arrays to the tool input schema.
//! - System prompt teaches the model when to pick instruction vs fact (rules
//!   for the agent → instruction; claims about the world → fact) and when to
//!   pick task vs event (an action item the agent needs to do → task; an
//!   action that already happened → event).
//! - Tool name unchanged (`record_memories`) so the parser layer stays shared.

use crate::extractor::Turn;

/// Stable extractor version for v2 prompt. Bump when schema or semantics
/// change in a way that invalidates calibration.
pub const EXTRACTOR_VERSION: &str = "anthropic-v2";

/// Tool name. Same as v1.
pub const TOOL_NAME: &str = "record_memories";

/// System prompt — instructs the model to extract all four memory types,
/// cite sources, and never speculate.
pub const SYSTEM_PROMPT: &str = "\
You are an extraction engine. Given a list of conversation turns, you call the \
`record_memories` tool exactly once with structured arrays of facts, events, \
instructions, and tasks.\n\
\n\
Memory-type guide:\n\
- `fact` — atomic supersedeable claim about an entity. (subject, predicate, object).\n\
  Example: user is a data engineer → {subject: \"user\", predicate: \"occupation\", object: \"data engineer\"}.\n\
- `event` — something that already happened, with a timestamp. (actor, verb, object, ts).\n\
  Example: user said they viewed a property at 18:00 → {actor: \"user\", verb: \"viewed_property\", ts: \"...\"}.\n\
- `instruction` — a rule for the agent to follow in the future. (scope, rule, trigger).\n\
  Example: \"always reply in Portuguese\" → {scope: \"agent\", rule: \"...\", trigger: \"before_reply\"}.\n\
- `task` — an action item the agent or someone else still needs to do. (task_id, title, state, due_at).\n\
  Use a deterministic task_id derived from the title (lowercase, snake_case, prefixed with `tsk_`).\n\
\n\
Rules:\n\
- Each extraction must cite the index(es) of the turns it came from in `source_indexes`.\n\
- Only extract claims explicitly stated. Do NOT speculate or infer.\n\
- Subject and actor identifiers should be lowercase, namespace-style (e.g. `user`, `user:luis`, `agent:bot`).\n\
- Predicate, verb, and trigger names are snake_case.\n\
- Confidence is in [0.0, 1.0].\n\
- If a turn contains nothing extractable, do not produce a memory citing it.\n\
- If the entire batch contains nothing extractable, call the tool with empty arrays.\n\
- Never write text outside the tool call.";

/// JSON Schema for the v2 `record_memories` tool input.
pub fn tool_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "facts": {
                "type": "array",
                "description": "Atomic supersedeable knowledge claims (subject, predicate, object).",
                "items": fact_item_schema()
            },
            "events": {
                "type": "array",
                "description": "Time-stamped append-only occurrences.",
                "items": event_item_schema()
            },
            "instructions": {
                "type": "array",
                "description": "Rules for the agent (scope, rule, trigger).",
                "items": instruction_item_schema()
            },
            "tasks": {
                "type": "array",
                "description": "Action items the agent or someone else still needs to do.",
                "items": task_item_schema()
            }
        },
        "required": ["facts", "events", "instructions", "tasks"]
    })
}

fn fact_item_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "subject": {"type": "string"},
            "predicate": {"type": "string"},
            "object": {},
            "text": {"type": "string"},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["subject", "predicate", "object", "text", "source_indexes", "confidence"]
    })
}

fn event_item_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "actor": {"type": "string"},
            "verb": {"type": "string"},
            "object": {"type": ["string", "null"]},
            "context": {"type": ["string", "null"]},
            "ts": {"type": "string"},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["actor", "verb", "ts", "source_indexes", "confidence"]
    })
}

fn instruction_item_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "scope": {"type": "string", "description": "Where the rule applies (e.g. `agent`, `agent:bot`)."},
            "rule": {"type": "string", "description": "The rule text in natural language."},
            "trigger": {"type": "string", "description": "When the rule fires (e.g. `before_reply`, `on_match`, `always`)."},
            "priority": {"type": "integer", "default": 0},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["scope", "rule", "trigger", "source_indexes", "confidence"]
    })
}

fn task_item_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "string", "description": "Deterministic id derived from the title (e.g. `tsk_schedule_viewing`)."},
            "title": {"type": "string"},
            "state": {"type": "string", "enum": ["open", "in_progress", "done", "cancelled"]},
            "due_at": {"type": ["string", "null"], "description": "Optional RFC-3339 timestamp."},
            "owner": {"type": ["string", "null"]},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["task_id", "title", "state", "source_indexes", "confidence"]
    })
}

/// Render the user message — bracketed turn list. Same format as v1.
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

    #[test]
    fn schema_has_all_four_arrays() {
        let s = tool_input_schema();
        let req = s["required"].as_array().unwrap();
        let names: Vec<_> = req.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"facts"));
        assert!(names.contains(&"events"));
        assert!(names.contains(&"instructions"));
        assert!(names.contains(&"tasks"));
    }

    #[test]
    fn task_state_enum_constrained() {
        let s = task_item_schema();
        let states: Vec<_> = s["properties"]["state"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(states, vec!["open", "in_progress", "done", "cancelled"]);
    }

    #[test]
    fn extractor_version_distinct_from_v1() {
        assert_eq!(EXTRACTOR_VERSION, "anthropic-v2");
        assert_ne!(EXTRACTOR_VERSION, super::super::v1::EXTRACTOR_VERSION);
    }
}
