//! Extraction prompt v4 — actor-balanced extraction.
//!
//! v3 was implicitly user-centric: every example, every speech-act-event ban,
//! every canonical-predicate hint led with "the user". On the LongMemEval-S
//! pilot this showed up as `single-session-assistant` scoring 0/3 — gold
//! answers like `"Roscioli"` (a deli the assistant recommended) or `"Andy
//! was wearing an untidy, stained white shirt"` (something the assistant
//! told the user) never made it into the typed memory store, because the
//! prompt's mental model treated only user-stated claims as memory-worthy.
//!
//! v4 keeps everything v3 fixed (canonical predicates, type-boundary
//! tiebreakers, speech-act-event ban) and adds **actor symmetry**:
//!
//! 1. **Any actor's stated facts about the world are extractable.** When the
//!    assistant says "Roscioli is a famous Roman deli", that's a fact about
//!    Roscioli, not just chatter. Subject = the entity in question
//!    (`subject: "roscioli"`), predicate names what's said
//!    (`kind_of`, `located_in`), object captures the value. The conversation
//!    `source` already records who said it via the cite — we don't encode
//!    "the assistant said this" in the predicate.
//! 2. **Third-party entities can be subjects.** v3's examples leaned heavily
//!    on `subject: "user"`. v4 explicitly says `subject` can be any
//!    namespace-style id: `roscioli`, `property:rua_das_flores_42`,
//!    `documentary:our_planet`, `andy`, `place:lapa_lisbon`.
//! 3. **Assistant-recommendation pattern.** When the assistant suggests /
//!    recommends / mentions an entity, emit a fact about the entity AND
//!    optionally a `recommended_to:user` fact noting the recommendation
//!    relationship. The latter is what makes "what restaurant did the
//!    assistant recommend?" recallable.
//!
//! Schema is identical to v3 — only the system prompt changes. Calibration
//! table gets a fresh row keyed by `*-v4` so multipliers don't bleed.

use crate::extractor::Turn;

/// Stable extractor-version identifier for v4. Distinct from v3 so the
/// `extractor::calibration::CALIBRATION` table can carry a separate row
/// for actor-balanced runs.
pub const EXTRACTOR_VERSION: &str = "anthropic-v4";

/// Tool name. Same as v1/v2/v3 — keeps the parser layer shared.
pub const TOOL_NAME: &str = "record_memories";

/// System prompt for v4 — same shape as v3 + actor symmetry rules + an
/// explicit assistant-told-fact section.
pub const SYSTEM_PROMPT: &str = "\
You are an extraction engine. Given a list of conversation turns, you call the \
`record_memories` tool exactly once with structured arrays of facts, events, \
instructions, and tasks.\n\
\n\
Memory-type guide:\n\
- `fact` — atomic claim about ANY entity. (subject, predicate, object). Subject \
can be the user, the agent, or any third-party entity mentioned in conversation.\n\
  Examples:\n\
    \"User is a data engineer\" → {subject: \"user\", predicate: \"occupation\", object: \"data engineer\"}.\n\
    \"Roscioli is a famous Roman deli\" → {subject: \"roscioli\", predicate: \"kind_of\", object: \"famous Roman deli\"}.\n\
    \"Andy was wearing a stained white shirt\" → {subject: \"andy\", predicate: \"clothing\", object: \"stained white shirt\"}.\n\
    \"The Metropolitan Museum is in NYC\" → {subject: \"metropolitan_museum\", predicate: \"location\", object: \"NYC\"}.\n\
- `event` — something that already happened in the real world (outside this \
conversation), with a timestamp. (actor, verb, object, ts).\n\
- `instruction` — a rule for the agent's future behaviour. (scope, rule, trigger).\n\
- `task` — an action item the agent or someone else still needs to do. (task_id, title, state, due_at).\n\
  Use a deterministic task_id (lowercase, snake_case, prefixed with `tsk_`).\n\
\n\
Actor symmetry (CRITICAL — closes a v3 gap):\n\
- The model must extract facts regardless of who stated them. The user, the agent, \
and any other speaker in the conversation are all valid sources of facts about the \
world. The conversation `source` already records who said what via the cite — do \
not encode \"who said this\" in the predicate.\n\
  When the **assistant** says \"Roscioli is the famous deli on Via dei Giubbonari\", \
emit a fact `(roscioli, ...)`, not nothing.\n\
  When the **assistant** describes a third party (\"Andy was wearing a stained \
white shirt\"), emit a fact about Andy.\n\
  When the **assistant** recommends something (\"I suggest Our Planet\"), emit \
both a fact about the thing AND optionally a fact `(documentary:our_planet, recommended_to, user)` \
so future recall surfaces it as an assistant recommendation.\n\
- When information accumulates across multiple turns from different speakers, \
extract it once with the most informative (subject, predicate, object) — not \
duplicates, one per actor.\n\
\n\
Type-boundary tie-breakers:\n\
- A claim about the world, the user, the agent, or any third party → **fact**, \
even if expressed as a preference or recommendation.\n\
  \"I prefer Lapa\" → fact `(user, prefers_neighborhood, Lapa)`, NOT an instruction.\n\
- A directive aimed at the agent's future behaviour → **instruction**.\n\
  \"Always reply in formal Portuguese\" → instruction `(agent, ..., before_reply)`.\n\
- A past real-world action by anyone → **event**. A future action that someone still needs to do → **task**.\n\
- A statement that the agent declines to answer or refers elsewhere → **instruction**, NOT a fact.\n\
\n\
Event semantics (CRITICAL — this was the v3 fix and is preserved):\n\
- An `event` represents an action that **happened in the real world** with consequences \
outside this conversation. Examples: `viewed_property`, `paid_invoice`, \
`escalated_to_human_agent`, `cancelled_subscription`.\n\
- **Do NOT emit speech-act events.** \"User said X\" / \"user requested Y\" / \"user inquired \
about Z\" / \"user stated A\" / \"assistant recommended Y\" are NOT events — they are speech \
acts that PRODUCE a fact, task, or instruction. The fact / task / instruction's \
`source_indexes` already records which turn the speech act happened in. A second \
speech-act event is duplication.\n\
  Bad (drop these): `event: user inquired_about_X`, `event: user stated_Y`, \
`event: user reported_Z`, `event: user expressed_interest_in_W`, \
`event: agent committed_to_doing_V`, `event: assistant_recommended_X`.\n\
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
  Good: `occupation`, `budget`, `prefers_neighborhood`, `move_in_deadline`, \
`kind_of`, `location`, `recommended_to`, `clothing`.\n\
  Bad: `the_users_occupation`, `what_the_user_does_for_work`, `apartment_user_wants`.\n\
- **Drop redundant qualifiers.** The conversation context already supplies the domain. \
A real-estate conversation doesn't need `apartment_` on every predicate; a customer-service \
conversation doesn't need `customer_` everywhere.\n\
  Good: `bedrooms`, `location`, `budget`, `move_in_deadline`.\n\
  Bad: `apartment_bedrooms`, `apartment_location`, `apartment_budget_max`.\n\
- **Prefer canonical predicates** from this small recurring-concept vocabulary when one matches:\n\
  - User-side: `budget`, `prefers_neighborhood`, `bedrooms`, `min_area_sqm`, `parking_spaces`, \
`move_in_deadline`, `payment_method`, `occupation`, `employer_industry`, `customer_id`, \
`language`, `pet_owner`, `inquired_about_property`, `has_pending_inquiry`.\n\
  - Entity-side: `kind_of`, `location`, `address`, `name`, `description`, \
`recommended_to`, `mentioned_in_conversation`, `clothing` (when describing a person), \
`opening_hours`, `cuisine` (restaurants), `rating`.\n\
  When the concept doesn't match any of these, name it as a single concept anyway.\n\
- **Always use the canonical predicate even if the speaker's wording differs.** \"Quartos\" → `bedrooms`, \
\"orçamento\" → `budget`, \"vagas de garagem\" → `parking_spaces`. Predicate names are domain English, \
not the speaker's natural language.\n\
\n\
General rules:\n\
- Each extraction must cite the index(es) of the turns it came from in `source_indexes`.\n\
- Only extract claims explicitly stated. Do NOT speculate or infer.\n\
- Subject and actor identifiers should be lowercase, namespace-style \
(e.g. `user`, `user:luis`, `agent:bot`, `roscioli`, `documentary:our_planet`, \
`property:rua_das_flores_42`).\n\
- Predicate, verb, and trigger names are snake_case English (regardless of conversation language).\n\
- Confidence is in [0.0, 1.0].\n\
- If a turn contains nothing extractable, do not produce a memory citing it.\n\
- If the entire batch contains nothing extractable, call the tool with empty arrays.\n\
- Never write text outside the tool call.";

/// JSON Schema for the v4 `record_memories` tool input.
///
/// Schema shape is identical to v3 — the `RawToolInput` parser is shared
/// across all prompt versions so the only difference between v3 and v4 is
/// the system prompt's guidance, not the wire format.
pub fn tool_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "facts": {
                "type": "array",
                "description": "Atomic supersedeable knowledge claims (subject, predicate, object). Subject can be the user, the agent, or ANY third-party entity mentioned in conversation. Predicates are short snake_case concept names — see system prompt for canonicalization rules and the actor-symmetry rule.",
                "items": fact_item_schema()
            },
            "events": {
                "type": "array",
                "description": "Time-stamped append-only occurrences. Real-world actions only — speech acts are captured by the fact/task they produce, not as separate events.",
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
    // `text` is intentionally NOT in `required`. The model occasionally emits
    // `null` for the human-readable rendering on third-party entities where
    // it has the (subject, predicate, object) triple but no clear free-form
    // sentence. The Anthropic tool_use API rejects the entire tool call
    // when a required string field is null, which would lose the whole
    // fact. The parser (`extractor::providers::common::filter_and_convert`)
    // already synthesises `text` from `(subject, predicate, object)` when
    // missing, so making the field optional in the schema lets the call
    // through without losing data.
    //
    // Defensive nullability on `subject` / `predicate` / `text`: the model
    // very occasionally emits `null` for these on edge cases (third-party
    // pronoun-only references, malformed turns). Anthropic's tool_use schema
    // validator rejects the WHOLE tool call when ANY required string field
    // is null, which kills the entire batch's extraction. Allowing
    // `["string", "null"]` keeps the call valid; the
    // null-defensive parser in `extractor::providers::common` then drops the
    // single bad fact while keeping the rest of the batch.
    serde_json::json!({
        "type": "object",
        "properties": {
            "subject": {"type": ["string", "null"], "description": "Lowercase namespace-style subject id. Can be `user`, `user:luis`, `agent:bot`, OR any third-party entity mentioned in conversation: `roscioli`, `documentary:our_planet`, `property:rua_das_flores_42`, `andy`, `place:lapa_lisbon`. The conversation source captures who STATED the fact; subject captures what the fact is ABOUT. NEVER emit null — emit the full fact or omit the entire entry."},
            "predicate": {"type": ["string", "null"], "description": "Short snake_case concept name. Drop redundant domain qualifiers (e.g. `bedrooms`, not `apartment_bedrooms`). Prefer canonical names when one applies. NEVER emit null."},
            "object": {},
            "text": {"type": ["string", "null"], "description": "Optional human-readable rendering of the fact. Omit entirely (don't emit `null`) when you don't have a clean rendering — the SDK will synthesise one from (subject, predicate, object)."},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        },
        "required": ["subject", "predicate", "object", "source_indexes", "confidence"]
    })
}

fn event_item_schema() -> serde_json::Value {
    // Same null-tolerance rationale as `fact_item_schema` — the parser drops
    // events with missing required fields, but Anthropic's tool_use schema
    // validator must accept null on any property the model occasionally emits
    // as null.
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

/// Render the user message — bracketed turn list. Same format as v1/v2/v3.
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
    fn extractor_version_distinct_from_v3() {
        assert_eq!(EXTRACTOR_VERSION, "anthropic-v4");
        assert_ne!(EXTRACTOR_VERSION, super::super::v3::EXTRACTOR_VERSION);
    }

    #[test]
    fn system_prompt_keeps_v3_calibration_sections() {
        // v4 inherits everything v3 fixed — predicate canonicalization,
        // type-boundary tiebreakers, speech-act-event ban. Catch a future
        // refactor that drops them.
        assert!(SYSTEM_PROMPT.contains("Predicate naming rules"));
        assert!(SYSTEM_PROMPT.contains("Drop redundant qualifiers"));
        assert!(SYSTEM_PROMPT.contains("canonical predicates"));
        assert!(SYSTEM_PROMPT.contains("Type-boundary tie-breakers"));
        assert!(SYSTEM_PROMPT.contains("Event semantics"));
        assert!(SYSTEM_PROMPT.contains("Do NOT emit speech-act events"));
    }

    #[test]
    fn system_prompt_introduces_actor_symmetry() {
        // The whole point of v4. If a future refactor drops the
        // actor-symmetry block, single-session-assistant goes back to 0/3.
        assert!(
            SYSTEM_PROMPT.contains("Actor symmetry"),
            "v4 system prompt must keep the actor-symmetry section"
        );
        // Specific examples that demonstrate third-party subjects.
        assert!(SYSTEM_PROMPT.contains("Roscioli"));
        assert!(SYSTEM_PROMPT.contains("Andy"));
    }

    #[test]
    fn fact_schema_subject_description_mentions_third_parties() {
        // The schema description is what the model actually reads when
        // tool-calling — it must explicitly say subject can be a third-party
        // entity, not just user/agent.
        let s = fact_item_schema();
        let desc = s["properties"]["subject"]["description"]
            .as_str()
            .unwrap_or("");
        assert!(desc.contains("third-party"));
        assert!(desc.contains("roscioli") || desc.contains("Roscioli"));
    }
}
