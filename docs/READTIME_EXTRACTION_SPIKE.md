# Read-Time Extraction Spike

**Status:** experimental, env-gated, default-off. Branch `feat/memory-hybrid-infra-and-quality`.

## Why

The local LongMemEval loop concluded the wall is **extraction coverage**, not
retrieval or synthesis: `raw_judge ≈ 0.056` means the answer fact usually never
becomes a typed memory at write time, so no retrieval/synthesis tweak can
recover it (`synth_judge ≈ 0.278` baseline). See
`memory/local-eval-stack.md` and `docs/LONGMEMEVAL_RESEARCH_CONCLUSION.md`.

Write-time extraction has to *guess*, at ingest, which turns will matter — the
local 30B under-guesses. Read-time (a.k.a. "proactive"/"active") extraction
inverts the timing: retain raw turns losslessly, and extract the answer at query
time, *conditioned on the question*. The answer-bearing turn is always
physically retained; the reader only has to find it in a small retrieved set
knowing what it's looking for. This plays to Chronik's durable-retention thesis
(WAL + segments already hold every turn) rather than throwing it away.

## What the spike does

A minimal, faithful A/B against the write-time baseline. Only the **evidence
source** changes; the reader model, judge, and answer rules are held constant.

```
baseline  (synthesize):          retrieve TYPED memories (mem.fact/event/...) → read → answer
read-time (synthesize_readtime): retrieve RAW turns (mem.raw.{ns})            → read → answer
```

- `synthesize_readtime` retrieves the top-`k` raw turns from `mem.raw.{ns}` by
  BM25 (`run_raw_search`), then feeds them to the same reader with a prompt
  (`build_readtime_prompt`) that mirrors the v2 synthesis answer rules but lists
  verbatim `(date) role: content` excerpts instead of extracted memory bullets.
- `k` (via `.k()`, i.e. `LONGMEMEVAL_SYNTH_K`) sets how many raw turns are
  retrieved and read.

## Components (all on this branch)

| File | Change |
|------|--------|
| `crates/chronik-memory/src/topics.rs` | `raw_searchable()` gate — `mem.raw.*` gets `bm25_enabled=true` iff `CHRONIK_MEMORY_RAW_SEARCHABLE=1` (default off, protecting the fleet-scale WalIndexer). |
| `crates/chronik-memory/src/recall.rs` | `RawTurn`, `parse_raw_turn_from_source`, `run_raw_search`/`post_raw_search`, `build_readtime_prompt`, and `RecallBuilder::synthesize_readtime`. |
| `crates/chronik-memory/tests/eval_longmemeval.rs` | `LONGMEMEVAL_READTIME=1` gate: forces `CHRONIK_MEMORY_RAW_SEARCHABLE=1` before `init_namespace`, swaps `synthesize` → `synthesize_readtime` in the synthesis pass. |

## Running the A/B (local, zero token cost)

Prereqs (see `memory/local-eval-stack.md`): Mac MLX synth `:8080` + Mistral
judge `:8082`, warm extraction cache, local single-node chronik built from this
branch (v2.10.6 base — needs the `value`-field cold-index fix so raw turns are
searchable).

```bash
# Baseline (write-time) — the 0.278 anchor
LONGMEMEVAL_USE_SYNTHESIS=1 ...  cargo test -p chronik-memory --test eval_longmemeval \
  evaluate_longmemeval -- --ignored --nocapture

# Read-time — same everything, +one flag
LONGMEMEVAL_READTIME=1 LONGMEMEVAL_USE_SYNTHESIS=1 ...  cargo test ...
```

Compare `synth_judge_rate` between the two runs on the same pilot-18 set.

## Known limitations / not-yet

- **Raw indexing load.** Flipping `mem.raw.*` searchable re-introduces the
  fleet-scale problem the flag was created to avoid (force-indexing hundreds of
  huge raw topics starves `mem.fact.*` indexing). Fine at pilot scale; a
  production read-time path wants a *dedicated on-demand* index, not blanket raw
  indexing.
- **BM25 only.** No vector retrieval over raw yet — keeps embedding cost/indexer
  load down for the first measurement. If BM25 recall over raw is the ceiling,
  add a raw vector channel.
- **Fused extract+read.** The spike does read-time *reading* (retrieve raw →
  answer). An explicit distill step (retrieve raw → extract candidate facts →
  synthesize) is the natural v2 if the single-call reader is context-limited at
  large `k`.
- **Readiness.** The harness gates on the *fact* topic being indexed; raw has
  more docs and may lag. If empty-raw-search rate is high, add an explicit raw
  readiness poll before recall.

## Decision gate

If read-time `synth_judge` beats the 0.278 baseline on pilot-18, the paradigm is
worth a production design (dedicated read-time index + optional distill step).
If it doesn't, we've falsified the lever cheaply before touching production
plumbing — the wall would then be retrieval over raw, not extraction timing.
