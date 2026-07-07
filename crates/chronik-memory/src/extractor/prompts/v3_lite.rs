//! v3-lite — condensed V3 for small local models (LM Studio / vLLM).
//!
//! # Why this exists
//!
//! The full V3 prompt is ~13K chars of predicate-canonicalization rules,
//! type-boundary tie-breakers, and speech-act bans. gpt-4o-mini follows it;
//! a locally-served 30B (qwen3-coder-30b, 2026-07-05 probes) is overwhelmed
//! by it and under-extracts — the same conversation chunk yielded **0 facts
//! under full V3 and 7 facts under a ~600-char prompt**, including the
//! answer-bearing fact. Rule count traded recall for canonicalization that a
//! 30B can't deliver anyway.
//!
//! v3-lite keeps the load-bearing instructions — the four types, speaker
//! attribution (WS-2), JSON-number objects (WS-4.3), citation indexes,
//! confidence — and drops the long canonicalization vocabulary and rule
//! blocks. Tool schema is shared with V3 (same wire shape, same parser).

use super::v3;
use crate::extractor::Turn;

/// Version id — embedded in `source.extractor` and in the extraction-cache
/// key, so switching between v3 and v3-lite never reuses the other's cache.
pub const EXTRACTOR_VERSION: &str = "openai-v3lite";

/// Condensed system prompt (~1.4K chars vs V3's ~13K).
pub const SYSTEM_PROMPT: &str = "\
You are an extraction engine. Given conversation turns, call the `record_memories` \
tool exactly once with arrays of facts, events, instructions, and tasks.\n\
\n\
- `fact` — atomic claim about an entity: (subject, predicate, object). Extract ALL \
facts stated about the user: occupation, education, family, possessions, preferences, \
budgets, plans, experiences. Example: {subject: \"user\", predicate: \"degree\", \
object: \"Business Administration\", speaker: \"user\"}.\n\
- Every fact has `speaker`: \"user\" if the user stated it, \"assistant\" if the \
assistant stated it. DO extract concrete assistant statements the user engaged with — \
recommendations, names, places, quotes, designations (e.g. {subject: \"user\", \
predicate: \"recommended_app\", object: \"Memrise\", speaker: \"assistant\"}). Skip \
generic assistant filler.\n\
- `event` — something that happened in the real world, with a timestamp: (actor, verb, \
object, ts). Not speech acts.\n\
- `instruction` — a rule for the agent's future behaviour: (scope, rule, trigger).\n\
- `task` — an open action item: (task_id, title, state, due_at). task_id = tsk_ + \
snake_case title.\n\
\n\
Rules:\n\
- Predicates are short snake_case concept names (`budget`, `bedrooms`, `occupation`).\n\
- Numeric values are JSON numbers (750000, 3, 21), never strings.\n\
- Cite the turn index(es) each memory came from in `source_indexes`.\n\
- Only extract what is explicitly stated. No speculation.\n\
- Be selective: at most 25 facts per call — the most consequential, no near-duplicates.\n\
- Keep every value SHORT. Do not restate turn content.\n\
- Empty batch → call the tool with empty arrays. Never write text outside the tool call.";

/// Tool name — shared with all versions.
pub const TOOL_NAME: &str = v3::TOOL_NAME;

/// Slim tool schema for output-budget-constrained local models.
///
/// V3's fact schema requires `text` (a sentence per fact) and `confidence` —
/// on a 30B that inflates the tool JSON past any reasonable max_tokens
/// (finish_reason=length at 8192 on real chunks, 2026-07-05). The shared
/// parser already synthesizes `text` from (subject, predicate, object) and
/// defaults `confidence`, so V3-lite simply doesn't ask for them. Events /
/// instructions / tasks keep V3's shapes (they are rare).
pub fn tool_input_schema() -> serde_json::Value {
    let mut schema = v3::tool_input_schema();
    schema["properties"]["facts"]["items"] = serde_json::json!({
        "type": "object",
        "properties": {
            "subject": {"type": "string", "description": "Lowercase namespace-style subject id (e.g. `user`)."},
            "predicate": {"type": "string", "description": "Short snake_case concept name."},
            "object": {"description": "The fact's value. Quantities MUST be JSON numbers."},
            "speaker": {"type": "string", "enum": ["user", "assistant"], "description": "Who stated the claim."},
            "source_indexes": {"type": "array", "items": {"type": "integer", "minimum": 0}}
        },
        "required": ["subject", "predicate", "object", "speaker", "source_indexes"]
    });
    schema
}

/// User-message rendering — identical to V3.
pub fn render_user_message(turns: &[Turn]) -> String {
    v3::render_user_message(turns)
}
