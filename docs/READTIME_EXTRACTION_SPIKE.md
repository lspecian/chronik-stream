# Read-Time Extraction Spike

**Status:** ✅ landed on `main` 2026-08-09 (PR #26), env-gated, default-off.

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

A minimal A/B against the write-time baseline. The reader model, judge, and
answer *rules* are held constant. **Two variables change together**, so the
delta measures the *paradigm*, not raw retrieval in isolation: (1) the
**evidence source** (typed memories → raw turns), and (2) its **serialization**
— `synthesize_readtime` uses a different prompt builder (`build_readtime_prompt`)
that lists verbatim `(date) role: content` turns instead of extracted memory
bullets. A fully isolated retrieval-only A/B would hold the serialization
identical; this one does not, so don't attribute the score gap to retrieval alone.

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

Compare `synth_judge_rate` between the two runs on the same set.

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

## Result & decision (observed)

Read-time won decisively and **landed on `main`** (PR #26, env-gated default-off):

| Config | Set | Reader | Judge | synth_judge |
|--------|-----|--------|-------|-------------|
| write-time (baseline) | pilot-18 | Qwen3-30B-A3B | Mistral-3.1-24B | 0.111 |
| read-time | pilot-18 | Qwen3-30B-A3B | Mistral-3.1-24B | 0.722 |
| read-time (+`SYNTH_K=40`, hot-search punctuation fix) | LongMemEval-S first 50 | Qwen3-30B-A3B | Mistral-3.1-24B | **0.880** (sub 0.760, abstain 0.040) |

Benchmark contract (per `docs/ROADMAP_MEMORY_QUALITY.md`) for every row: **judge**
= local mlx `Mistral-Small-3.1-24B` (independent of the reader); **reader/synthesizer**
= local mlx `Qwen3-30B-A3B` (v2 answer-rules prompt); **extractor** = none for
read-time (extraction-free), cached OpenAI `gpt-4o-mini` for the write-time
baseline. The pilot-18 rows are the same-set comparison; the 0.880 row is a
different, larger set (LongMemEval-S first-50) and was re-verified on the
reconciled-with-`main` build before merge. Two
levers stacked the gain: raw-retrieval depth (`SYNTH_K` 15→40: 0.48→0.72) and
the hot-search punctuation fix (0.72→0.88, shipped v2.10.9). The methodology
caveat above stands — the write-time vs read-time gap also carries the
serialization change, so it measures the paradigm, not raw retrieval alone.

Remaining limitations (see above): raw indexing load at fleet scale wants a
dedicated on-demand index; BM25-only (no raw vector channel yet); the single-call
reader may be context-limited at large `k` (an explicit distill step is the
natural v2).
