//! Extraction prompt v5 — v3's calibration plus **balanced third-party
//! coverage**.
//!
//! v4 attempted actor-symmetry (rebalance extraction across user / assistant /
//! agent / third-party) and **regressed** LongMemEval-S judge_rate
//! 0.389 → 0.222 on the 18-item balanced pilot. Net: +1
//! single-session-assistant, −2 single-session-user, −1 temporal-reasoning,
//! −1 knowledge-update. The "extract about all actors" framing diluted
//! user-fact density — items previously surfacing with n=10 retrieval came
//! back with n=1-5.
//!
//! v5 keeps v3's user-fact-first priority but adds explicit **concrete-noun
//! extraction** rules so the model also picks up facts that v3 was
//! systematically skipping:
//!
//! - **Physical descriptions** of third parties mentioned by the agent or
//!   assistant ("Andy was wearing an untidy, stained white shirt").
//! - **Named places, foods, brands, products** mentioned by either party
//!   (`Met Museum`, `Roscioli`, `Premier Silver`, `Lapa`).
//! - **Quantities, counts, durations** with their referent
//!   ("2-3 eggs", "32 minutes", "21 days").
//!
//! The framing is **additive, not redistributive**: user-facts remain the
//! priority, third-party facts are added *on top* without trading away
//! user-fact tokens. The instruction wording is deliberately negative on the
//! v4 trap: "do not reduce user-fact extraction to make room for
//! third-party facts."
//!
//! Schema-side, v5 inherits v4's null-tolerance on required string fields
//! (`["string", "null"]`) so a single null in any field doesn't kill the
//! whole tool_use batch. The parser still drops the malformed individual
//! record (`extractor::providers::common::filter_and_convert`).

use crate::extractor::Turn;

/// Stable extractor-version identifier for v5. Distinct from v3/v4 so
/// memories produced under v5 carry a separate calibration line.
pub const EXTRACTOR_VERSION: &str = "anthropic-v5";

/// Tool name. Same as v1-v4 — parser layer is shared.
pub const TOOL_NAME: &str = "record_memories";

/// System prompt for v5 — v3's calibration plus balanced third-party
/// coverage. See module-level docs for the v4 → v5 lesson.
pub const SYSTEM_PROMPT: &str = "\
You are an extraction engine. Given a list of conversation turns, you call the \
`record_memories` tool exactly once with structured arrays of facts, events, \
instructions, and tasks.\n\
\n\
Memory-type guide:\n\
- `fact` — atomic claim about an entity. (subject, predicate, object).\n\
  Example: user is a data engineer → {subject: \"user\", predicate: \"occupation\", object: \"data engineer\"}.\n\
- `event` — something that already happened, with a timestamp. (actor, verb, object, ts).\n\
  Example: user said they viewed a property at 18:00 → {actor: \"user\", verb: \"viewed_property\", ts: \"...\"}.\n\
- `instruction` — a rule for the agent's future behaviour. (scope, rule, trigger).\n\
  Example: \"always reply in Portuguese\" → {scope: \"agent\", rule: \"...\", trigger: \"before_reply\"}.\n\
- `task` — an action item the agent or someone else still needs to do. (task_id, title, state, due_at).\n\
  Use a deterministic task_id derived from the title (lowercase, snake_case, prefixed with `tsk_`).\n\
\n\
Extraction priority (CRITICAL — v5 calibration):\n\
1. **User-facts are the priority.** Anything the user states about themselves \
(preferences, history, occupation, plans, health, budget, location) is a high-\
priority fact. Capture EVERY user-fact you can — under-extracting user-facts \
hurts retrieval more than missing any other category.\n\
2. **Concrete third-party facts are ADDITIVE — do NOT reduce user-fact \
extraction to make room for them.** When the user OR the agent OR the \
assistant mentions a specific:\n\
  - named place (e.g. `Met Museum`, `Lapa`, `Roscioli restaurant`)\n\
  - named food / product / brand (e.g. `Premier Silver`, `eggs`, `nike`)\n\
  - quantity / count / measurement (e.g. `2-3 eggs`, `32 minutes`, `400 sqm`)\n\
  - physical description of a person or object (e.g. \"Andy was wearing an \
untidy, stained white shirt\", \"the property has a private elevator\")\n\
  - temporal anchor for an event (e.g. \"three weeks ago\", \"last Tuesday\")\n\
  …extract it as a fact. Subject is whatever the description is ABOUT (the \
named entity), not who stated it.\n\
  Examples (extract ALL of these):\n\
    - \"Andy was wearing an untidy, stained white shirt.\" → \
fact `(andy, wearing, untidy stained white shirt)`\n\
    - \"Try the carbonara at Roscioli — it's amazing.\" → \
fact `(roscioli, serves, carbonara)` + fact `(roscioli, quality, amazing)`\n\
    - \"You'd want 2-3 eggs for that recipe.\" → \
fact `(recipe, eggs_needed, 2-3)` (subject is the dish if named, else use \
the obvious concept noun)\n\
    - \"The Met Museum is open until 9pm on Fridays.\" → \
fact `(met_museum, closing_time_friday, 9pm)`\n\
3. **Never skip a fact because it feels too small or too narrative.** If a \
named entity has a property attached, that's a fact worth keeping. The \
recall layer will rank it appropriately.\n\
\n\
Type-boundary tie-breakers:\n\
- A claim about the world or about the user → **fact**, even if expressed as a preference.\n\
  \"I prefer Lapa\" → fact `(user, prefers_neighborhood, Lapa)`, NOT an instruction.\n\
- A directive aimed at the agent's future behaviour → **instruction**.\n\
  \"Always reply in formal Portuguese\" → instruction `(agent, ..., before_reply)`.\n\
- A past real-world action by anyone → **event**. A future action that someone still needs to do → **task**.\n\
- A statement that the agent declines to answer or refers elsewhere → **instruction**, NOT a fact.\n\
- An assistant utterance describing or recommending something → extract \
the underlying **facts** about the described entity (NOT a speech-act event \
about the assistant).\n\
\n\
Event semantics (CRITICAL — this is the most common extraction error):\n\
- An `event` represents an action that **happened in the real world** with consequences \
outside this conversation. Examples: `viewed_property`, `paid_invoice`, \
`escalated_to_human_agent`, `cancelled_subscription`.\n\
- **Do NOT emit speech-act events.** \"User said X\" / \"user requested Y\" / \"user inquired \
about Z\" / \"user stated A\" / \"assistant recommended W\" are NOT events — they are \
speech acts that PRODUCE a fact, task, or instruction. The fact / task / \
instruction's `source_indexes` already records which turn the speech act \
happened in. A second speech-act event is duplication.\n\
  Bad (drop these): `event: user inquired_about_X`, `event: user stated_Y`, \
`event: user reported_Z`, `event: assistant recommended_Q`, \
`event: agent committed_to_doing_V`.\n\
  Good (keep these): `event: agent escalated_to_human_agent` (real action with \
consequences), `event: user viewed_property` (the actual viewing, if it happened), \
`event: billing_system charged_duplicate` (the underlying real-world incident).\n\
- When the agent says \"I'll do X\" or \"let me check X for you\", that's a **task** \
for the agent — NOT an event of the agent committing.\n\
- When deciding whether to emit an event, ask: did this action happen outside this \
conversation, with real consequences? If no → it is a speech act and the fact / task \
captures it.\n\
\n\
Predicate naming rules (CRITICAL — these drive deduplication):\n\
- Predicates are concept names. **One concept per predicate.** Single noun or short \
verb-phrase, snake_case, no English connectors.\n\
  Good: `occupation`, `budget`, `prefers_neighborhood`, `move_in_deadline`.\n\
  Bad: `the_users_occupation`, `what_the_user_does_for_work`, `apartment_user_wants`.\n\
- **Drop redundant qualifiers.** The conversation context already supplies the domain. \
A real-estate conversation doesn't need `apartment_` on every predicate; a customer-service \
conversation doesn't need `customer_` everywhere. Prefer the shortest unambiguous form.\n\
  Good: `bedrooms`, `location`, `budget`, `move_in_deadline`.\n\
  Bad: `apartment_bedrooms`, `apartment_location`, `apartment_budget_max`, `desired_move_in_month`.\n\
- **Prefer canonical predicates** from this small recurring-concept vocabulary when one matches:\n\
  - `budget` (numeric budget cap, currency goes in `object` if needed)\n\
  - `location`, `prefers_neighborhood`\n\
  - `bedrooms`, `min_area_sqm`, `parking_spaces`\n\
  - `move_in_deadline`, `payment_method`\n\
  - `occupation`, `employer_industry`\n\
  - `customer_id`, `language`, `pet_owner`\n\
  - `inquired_about_property`, `has_pending_inquiry`\n\
  When the concept doesn't match any of these, name it as a single concept anyway.\n\
- **Always use the canonical predicate even if the user's wording differs.** \"Quartos\" → `bedrooms`, \
\"orçamento\" → `budget`, \"vagas de garagem\" → `parking_spaces`. Predicate names are domain English, \
not the user's natural language.\n\
\n\
General rules:\n\
- Each extraction must cite the index(es) of the turns it came from in `source_indexes`.\n\
- Only extract claims explicitly stated. Do NOT speculate or infer.\n\
- Subject and actor identifiers should be lowercase, namespace-style (e.g. `user`, `user:luis`, `agent:bot`, `andy`, `met_museum`, `roscioli`). Bare lowercase ids are fine for third-party entities without an obvious namespace prefix.\n\
- Predicate, verb, and trigger names are snake_case English (regardless of conversation language).\n\
- Confidence is in [0.0, 1.0].\n\
- If a turn contains nothing extractable, do not produce a memory citing it.\n\
- If the entire batch contains nothing extractable, call the tool with empty arrays.\n\
- Never write text outside the tool call.";

/// JSON Schema for the v5 `record_memories` tool input.
///
/// Same shape as v3 with v4's null-tolerance on required string fields.
/// The parser drops individual malformed records.
pub fn tool_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "facts": {
                "type": "array",
                "description": "Atomic supersedeable knowledge claims (subject, predicate, object). User-facts are priority; concrete third-party facts (named places, foods, products, quantities, physical descriptions) are ADDITIVE — see system prompt.",
                "items": fact_item_schema()
            },
            "events": {
                "type": "array",
                "description": "Time-stamped append-only occurrences. No speech-act events.",
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
            "subject": {"type": ["string", "null"], "description": "Lowercase namespace-style subject id. For user-facts: `user`, `user:luis`. For third-party facts: the named entity in lowercase (`andy`, `met_museum`, `roscioli`, `premier_silver`). NEVER emit null — emit the full fact or omit the entry."},
            "predicate": {"type": ["string", "null"], "description": "Short snake_case concept name. Drop redundant domain qualifiers (e.g. `bedrooms`, not `apartment_bedrooms`). Prefer canonical names when one applies. NEVER emit null."},
            "object": {},
            "text": {"type": ["string", "null"], "description": "Optional human-readable rendering. Omit entirely (don't emit `null`) when you don't have a clean rendering — the SDK synthesises one from (subject, predicate, object)."},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["subject", "predicate", "object", "source_indexes", "confidence"]
    })
}

fn event_item_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "actor": {"type": ["string", "null"]},
            "verb": {"type": ["string", "null"], "description": "Short snake_case verb phrase (e.g. `viewed_property`, `requested_refund`)."},
            "object": {"type": ["string", "null"]},
            "context": {"type": ["string", "null"]},
            "ts": {"type": ["string", "null"]},
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
            "scope": {"type": ["string", "null"], "description": "Where the rule applies (e.g. `agent`, `agent:bot`)."},
            "rule": {"type": ["string", "null"], "description": "The rule text in natural language."},
            "trigger": {"type": ["string", "null"], "description": "When the rule fires — typically one of `before_reply`, `on_match`, `always`."},
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
            "task_id": {"type": ["string", "null"], "description": "Deterministic id derived from the title (e.g. `tsk_schedule_viewing`). Lowercase snake_case prefixed with `tsk_`."},
            "title": {"type": ["string", "null"]},
            "state": {"type": ["string", "null"], "enum": ["open", "in_progress", "done", "cancelled", null]},
            "due_at": {"type": ["string", "null"], "description": "Optional RFC-3339 timestamp."},
            "owner": {"type": ["string", "null"]},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["task_id", "title", "state", "source_indexes", "confidence"]
    })
}

/// Render the user message — bracketed turn list. Same format as v1/v2/v3/v4.
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
    fn extractor_version_is_v5() {
        assert_eq!(EXTRACTOR_VERSION, "anthropic-v5");
    }

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
    fn fact_schema_keeps_required_keys_but_makes_strings_nullable() {
        let s = fact_item_schema();
        let req = s["required"].as_array().unwrap();
        let names: Vec<_> = req.iter().filter_map(|v| v.as_str()).collect();
        // `text` is NOT required (model occasionally omits / nulls it).
        assert!(!names.contains(&"text"));
        // `subject` / `predicate` ARE required but nullable in the type.
        assert!(names.contains(&"subject"));
        assert!(names.contains(&"predicate"));
        let subject_type = s["properties"]["subject"]["type"].as_array().unwrap();
        let subject_type_strs: Vec<_> =
            subject_type.iter().filter_map(|v| v.as_str()).collect();
        assert!(subject_type_strs.contains(&"string"));
        assert!(subject_type_strs.contains(&"null"));
    }

    #[test]
    fn prompt_keeps_user_fact_priority() {
        let p = SYSTEM_PROMPT;
        // The lesson from v4: don't suggest equal token allocation.
        assert!(p.contains("User-facts are the priority"));
        assert!(p.contains("ADDITIVE"));
        assert!(p.contains("do NOT reduce user-fact extraction"));
    }

    #[test]
    fn prompt_includes_concrete_noun_examples() {
        let p = SYSTEM_PROMPT;
        // Targeted extraction-gap items from pilots 3-5:
        assert!(p.contains("Andy was wearing")); // item 2
        assert!(p.contains("Roscioli")); // item 8
        assert!(p.contains("Met Museum")); // item 14
        assert!(p.contains("2-3 eggs")); // item 18
    }

    #[test]
    fn render_user_message_brackets_turns() {
        let turns = vec![
            Turn {
                role: "user".into(),
                content: "I want a 3BR in Lapa".into(),
                ts: None,
                channel: None,
                external_id: None,
            },
            Turn {
                role: "assistant".into(),
                content: "Got it".into(),
                ts: None,
                channel: None,
                external_id: None,
            },
        ];
        let msg = render_user_message(&turns);
        assert!(msg.contains("[0] user: I want a 3BR in Lapa"));
        assert!(msg.contains("[1] assistant: Got it"));
        assert!(msg.contains("Now call record_memories"));
    }
}
