# Fixture authoring guide (AM-1.5.b)

This guide is for humans expanding the custom-fixture set at [`crates/chronik-memory/tests/fixtures/agent-memory/`](../crates/chronik-memory/tests/fixtures/agent-memory/). The infrastructure to *consume* fixtures — the `eval::Fixture` schema, deserializer, and the eval harness — is fully in place. Adding more fixtures is data-generation work, not code work.

The target is 100 fixtures. As of 2026-07-02 the repo has 10, covering the five verticals declared in the roadmap (real-estate, customer-support, personal-assistant, technical-help, multi-session).

## Fixture format

Every fixture is a single JSON file. The schema is `chronik_memory::eval::Fixture`:

```json
{
  "name": "kebab-case-slug",
  "description": "One-line summary of the scenario + which extractor + version calibrated against.",
  "conversation": [
    {
      "role": "user" | "assistant",
      "content": "the utterance",
      "ts": "RFC3339 timestamp"
    }
  ],
  "expected_memories": [
    {
      "type": "fact" | "event" | "instruction" | "task",
      "key": "optional stable key",
      "subject_predicate": ["subject", "predicate"],
      "source_indexes": [0, 1],
      "min_confidence": 0.7
    }
  ],
  "recall_queries": [
    {
      "query": "natural-language question",
      "expected_keys": ["fact:key"],
      "k": 5
    }
  ]
}
```

Key rules:
- `name` must be unique across the fixtures dir (used as a test identifier).
- Every entry in `expected_memories` must be citable via `source_indexes` — the extractor's cite-source check rejects any memory that doesn't cite at least one input turn, so the fixtures must not encode phantom memories.
- `recall_queries.expected_keys` should be non-empty for at least half the queries — an empty expected set turns the test into an abstention check rather than a hit check.

## Recipe: 10-fixture batch in one sitting

The most efficient authoring flow (5-15 min per fixture):

1. **Pick a vertical + scenario shape**. Coverage matrix as of 2026-07-02:
    - Real-estate: 3 fixtures (Lapa, budget-supersession, viewing-event)
    - Customer-support: 2 fixtures (billing, refusal-schools)
    - Personal-assistant: 2 fixtures (basic assistant, PT-BR)
    - Technical / multi-turn: 2 fixtures (lead-capture, frustration-escalation)
    - Task lifecycle: 1 fixture

    Where the count is < 20 per vertical, add there first.

2. **Write the conversation manually or with LLM assistance.**
    - 10-30 turns per conversation.
    - Include at least one implicit fact (something the user says without explicit statement, e.g. "let me check with my wife" → `user|marital_status=married`).
    - Include at least one time-anchored event (property viewing, task completion) with a specific timestamp.
    - Include at least one instruction (a rule the agent should follow going forward).

3. **Extract the gold set by hand.** For each turn, list every fact / event / instruction / task the ideal extractor would produce. Use `subject|predicate` for the `key` field on facts (e.g. `user:luis|budget_ceiling`).

4. **Add 2-4 recall queries.** At least one query should:
    - Match a specific `expected_key` (hit test).
    - Be phrased differently from the source turn (paraphrase test).
    - Reference implicit state (temporal or arithmetic — "which properties has the user actually visited?").

5. **Save as `{scenario-slug}.json` in `crates/chronik-memory/tests/fixtures/agent-memory/`.**

6. **Verify it loads.** Run:
    ```bash
    cargo test -p chronik-memory --test eval_extraction fixtures_load -- --nocapture
    ```
    The output lists all fixtures + their expected memory count. Confirm your new fixture appears.

## LLM-assisted authoring

Claude Sonnet 4.6 (or later) can generate high-quality fixtures given the schema + 2-3 in-repo examples. Prompt template:

```
Here is the fixture format for our extractor eval:
[paste one existing fixture JSON]

Author a new fixture for the vertical: <vertical>. Scenario: <description>.
- 15 turns, mix of user/assistant.
- Include a fact-supersession event (user changes their mind about budget/preference/etc).
- Include a Task with due_at.
- 4 recall queries, 2 with expected_keys and 2 abstention checks.
Output only the JSON, no commentary.
```

Every LLM-generated fixture must be reviewed by a human before commit — cite-source accuracy in particular is easy to get wrong (LLM invents an offset that doesn't cite the utterance).

## Batching towards 100

10 fixtures / week for 9 weeks reaches 100. Each fixture is independent — no cross-fixture dependencies, no shared state. A single reviewer can validate a 10-fixture batch in ~1 hour.

The eval harness picks up new fixtures automatically on the next `cargo test` — no registration step needed.
