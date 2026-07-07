//! Extraction prompt v3 — same four memory types as v2, with **predicate
//! canonicalization** baked into the system prompt.
//!
//! The Phase 1 / 2 dogfood numbers showed a clear failure mode: even at
//! `temperature=0`, Claude Haiku and gpt-4o-mini produce semantically
//! equivalent but lexically different predicates across runs:
//!
//! - `apartment_orientation_preference` vs `apartment_orientation` vs
//!   `orientation_preference`
//! - `budget` vs `budget_max` vs `apartment_budget_max` vs `max_budget_eur`
//! - `customer_id` vs `customer_account_id`
//!
//! Each variant is a different `(namespace, key)` from log compaction's
//! perspective, which means recall against an expected key misses, top-3 hit
//! rate slumps to 58 %, and downstream LongMemEval suffers the same fate.
//!
//! v3's system prompt enforces three rules:
//! 1. **Predicates are concept names**, not sentences. Single noun-or-verb,
//!    snake_case, no English connectors. (`prefers_neighborhood` is OK,
//!    `the_neighborhood_the_user_prefers` is not.)
//! 2. **Drop redundant context.** The topic already supplies the domain —
//!    real-estate conversations don't need `apartment_` on every predicate;
//!    customer-service conversations don't need `customer_` everywhere.
//!    Prefer the shortest unambiguous name.
//! 3. **Prefer the canonical name** from a small recommended vocabulary
//!    when the concept matches one of them. The vocabulary covers concepts
//!    that recur across domains (budget / location / bedrooms / role /
//!    occupation / customer_id / language / preferences), not domain-specific
//!    things (which the model still names as it sees fit).
//!
//! Plus stronger type-boundary examples: when a sentence is *both* a self-
//! fact and an instruction (`I always prefer X` → instruction or fact?), v3
//! says **fact** if the claim is about the world / user, **instruction** if
//! it's a directive aimed at the agent.

use crate::extractor::Turn;

/// Stable extractor-version identifier for v3. Distinct from v2 so memories
/// produced under v3 carry a separate calibration line in the
/// `extractor::calibration::CALIBRATION` table.
pub const EXTRACTOR_VERSION: &str = "anthropic-v3";

/// Tool name. Same as v1/v2 — keeps the parser layer shared.
pub const TOOL_NAME: &str = "record_memories";

/// System prompt for v3 — extracts all four memory types with predicate
/// canonicalization, type-boundary tie-breakers, and a speech-act-event ban.
/// See module-level docs for the rationale behind each rule block.
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
Speaker attribution (CRITICAL — drives \"what did you tell me\" recall):\n\
- Every fact carries a `speaker` field: `\"user\"` when the user stated it, `\"assistant\"` \
when the assistant stated it.\n\
- **Extract assistant-stated facts.** When the assistant names, recommends, quotes, \
designates, or quantifies something concrete — an app (\"I recommend Memrise\"), a place, \
a website, a designation (\"your jumpsuit is 'LIV'\"), a definition, a quoted passage — \
and the user engages with it rather than rejecting it, emit a fact with \
speaker=\"assistant\". Subject is the entity the claim is about \
(e.g. `(user, recommended_language_app, Memrise)` with speaker=\"assistant\").\n\
- Do NOT skip a fact merely because the user never restated it. Users later ask \
\"what did you tell me about X?\" — the assistant's statement IS the memory.\n\
- Generic assistant filler (\"great choice!\", \"let me know if you need anything\") is \
NOT a fact. Extract only concrete, referable content.\n\
\n\
Type-boundary tie-breakers:\n\
- A claim about the world or about the user → **fact**, even if expressed as a preference.\n\
  \"I prefer Lapa\" → fact `(user, prefers_neighborhood, Lapa)`, NOT an instruction.\n\
- A directive aimed at the agent's future behaviour → **instruction**.\n\
  \"Always reply in formal Portuguese\" → instruction `(agent, ..., before_reply)`.\n\
- A past real-world action by anyone → **event**. A future action that someone still needs to do → **task**.\n\
- A statement that the agent declines to answer or refers elsewhere → **instruction**, NOT a fact.\n\
\n\
Event semantics (CRITICAL — this is the most common extraction error):\n\
- An `event` represents an action that **happened in the real world** with consequences \
outside this conversation. Examples: `viewed_property`, `paid_invoice`, \
`escalated_to_human_agent`, `cancelled_subscription`.\n\
- **Do NOT emit speech-act events.** \"User said X\" / \"user requested Y\" / \"user inquired \
about Z\" / \"user stated A\" are NOT events — they are speech acts that PRODUCE a fact, \
task, or instruction. The fact / task / instruction's `source_indexes` already records \
which turn the speech act happened in. A second speech-act event is duplication.\n\
  Bad (drop these): `event: user inquired_about_X`, `event: user stated_Y`, \
`event: user reported_Z`, `event: user expressed_interest_in_W`, \
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
- **Numeric objects are JSON numbers.** When a fact's object is a quantity — a count, \
a price, a duration in days, an area — emit it as a bare JSON number (750000, 3, 21), \
NOT a string (\"$750K\", \"three\", \"21 days\"). Put the unit and original wording in `text`. \
This drives numeric-aware ranking downstream.\n\
- Subject and actor identifiers should be lowercase, namespace-style (e.g. `user`, `user:luis`, `agent:bot`).\n\
- Predicate, verb, and trigger names are snake_case English (regardless of conversation language).\n\
- Confidence is in [0.0, 1.0].\n\
- If a turn contains nothing extractable, do not produce a memory citing it.\n\
- If the entire batch contains nothing extractable, call the tool with empty arrays.\n\
- Never write text outside the tool call.";

/// JSON Schema for the v3 `record_memories` tool input.
///
/// Schema shape is identical to v2 — the `RawToolInput` parser is shared
/// across all prompt versions so the only difference between v2 and v3 is
/// the system prompt's guidance, not the wire format.
pub fn tool_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "facts": {
                "type": "array",
                "description": "Atomic supersedeable knowledge claims (subject, predicate, object). Predicates are short snake_case concept names — see system prompt for canonicalization rules.",
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
            "subject": {"type": "string", "description": "Lowercase namespace-style subject id (e.g. `user`, `user:luis`, `agent:bot`)."},
            "predicate": {"type": "string", "description": "Short snake_case concept name. Drop redundant domain qualifiers (e.g. `bedrooms`, not `apartment_bedrooms`). Prefer canonical names when one applies."},
            "object": {"description": "The fact's value. Quantities (counts, prices, durations, areas) MUST be JSON numbers (750000, 3, 21) — never strings like \"$750K\" or \"three\". Units and original wording go in `text`."},
            "text": {"type": "string"},
            "speaker": {"type": "string", "enum": ["user", "assistant"], "description": "Who stated the claim in the conversation. \"assistant\" for facts the assistant introduced (recommendations, names, quotes, designations)."},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["subject", "predicate", "object", "text", "speaker", "source_indexes", "confidence"]
    })
}

fn event_item_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "actor": {"type": "string"},
            "verb": {"type": "string", "description": "Short snake_case verb phrase (e.g. `viewed_property`, `requested_refund`)."},
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
            "trigger": {"type": "string", "description": "When the rule fires — typically one of `before_reply`, `on_match`, `always`."},
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
            "task_id": {"type": "string", "description": "Deterministic id derived from the title (e.g. `tsk_schedule_viewing`). Lowercase snake_case prefixed with `tsk_`."},
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

/// Render the user message — bracketed turn list. Same format as v1/v2.
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
    fn extractor_version_distinct_from_v1_and_v2() {
        assert_eq!(EXTRACTOR_VERSION, "anthropic-v3");
        assert_ne!(EXTRACTOR_VERSION, super::super::v1::EXTRACTOR_VERSION);
        assert_ne!(EXTRACTOR_VERSION, super::super::v2::EXTRACTOR_VERSION);
    }

    #[test]
    fn system_prompt_mentions_predicate_canonicalization() {
        // Sanity: the canonicalization-rules section is the whole point of v3.
        // If someone refactors the prompt in a way that drops it, the
        // recall calibration target evaporates.
        assert!(
            SYSTEM_PROMPT.contains("Predicate naming rules"),
            "v3 system prompt must keep the predicate canonicalization section"
        );
        assert!(
            SYSTEM_PROMPT.contains("Drop redundant qualifiers"),
            "v3 system prompt must keep the redundant-qualifier rule"
        );
        assert!(
            SYSTEM_PROMPT.contains("canonical predicates"),
            "v3 system prompt must keep the canonical-vocabulary rule"
        );
    }

    #[test]
    fn system_prompt_mentions_type_boundary_tiebreakers() {
        assert!(
            SYSTEM_PROMPT.contains("Type-boundary tie-breakers"),
            "v3 system prompt must keep the type-classification tie-breaker section"
        );
    }

    #[test]
    fn system_prompt_bans_speech_act_events() {
        // The "speech-act event" ban is what closed the type-classification
        // gap from 74 % to ~90 %+. If a future refactor drops it, the
        // regression returns; the assertion is here to catch it loudly.
        assert!(
            SYSTEM_PROMPT.contains("Event semantics"),
            "v3 system prompt must keep the event-semantics section"
        );
        assert!(
            SYSTEM_PROMPT.contains("Do NOT emit speech-act events"),
            "v3 system prompt must explicitly ban speech-act events"
        );
    }
}
