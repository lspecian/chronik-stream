# LongMemEval Memory-Quality Research — Conclusion

**Status:** Concluded 2026-07-12. **Finding: `synth_judge = 0.546` (v3 prompt +
supersession lint, gemma3:4b synth) is the empirical quality ceiling for the
free-hardware stack.** Every other lever — across retrieval, fact-cleaning, and
synth-model axes — fails to beat it.

## What we measured

LongMemEval-S: 500 long-conversation memory questions across 6 categories
(single-session user/assistant/preference, multi-session, knowledge-update,
temporal-reasoning). The Chronik memory pipeline is:

```
conversation turns → EXTRACT typed facts (cached gpt-4o-mini) → index (BM25 + optional vector)
                                                                      │
question → RECALL top-k → SYNTHESIZE one answer (local LLM) → JUDGE vs gold (local LLM)
```

Headline metric = `synth_judge_rate` (LLM-graded synthesized answer). Because
the judge itself is a local model, **absolute numbers are only comparable within
the same judge.** All levers below were measured against the same
gemma3:4b-judged baseline; they are NOT comparable to the earlier OpenAI-judged
0.336 (a stricter judge, different scale).

### Metric-integrity rule (learned the hard way)

Verdicts come **only** from the harness's official end-of-shard summary
(`synth_judge_rate`, `synth_sub_rate`), never ad-hoc per-item greps (which
double-count substring tokens — this produced a false "8B triples quality"
claim in E5 that had to be retracted). And `substring_rate` (raw retrieval)
≠ `synth_sub_rate` (synthesized-answer exact match) — only the latter is a
judge-free measure of answer quality.

## Results

| Experiment | Config (Δ from baseline) | synth_judge | Verdict |
|---|---|---|---|
| E1c | text-only baseline (local judge) | 0.506 | baseline |
| E2 | + v3 never-abstain synth prompt | 0.530 | small win, kept |
| **E4** | **+ recall-time supersession lint (exact key)** | **0.546** | **WIN — best config** |
| E5 | synth model granite-8b-q4 (judge confounded) | — | wash (synth_sub 0.128 ≈ 0.136) |
| T1 | + temporal `[N days ago]` fact annotation | 0.504 | rejected |
| T2 | + entity-centric BM25 recall union | 0.504 | rejected (dilutes top-k) |
| T3 | + normalized-key supersession (entity resolution) | 0.502 | rejected (double-edged) |
| M1 | synth model qwen2.5:7b (judge held = gemma3:4b) | 0.446\* | rejected (\*judge artifact — see below) |

Earlier retrieval-surface experiments (separate harness) told the same story:
adding a **vector** recall surface and **concept-page rollups** both scored
*below* the text-only pipeline — extra surfaces compete for top-k slots and
dilute an already-strong atomic set.

## Why 0.546 is a ceiling — three axes, all exhausted

**1. Retrieval is already ahead of synthesis.** Across the winning runs,
recall `hit_rate ≈ 0.60` (the answer-fact IS retrieved) while `synth_judge ≈
0.55`. The bottleneck is the *reader*, not the *retriever*. So every
retrieval-side lever (vector, concepts, entity-recall) can only dilute — it
adds no answer-facts that weren't already present. T2 confirmed this directly:
`substring_rate` stayed flat while multi-session *dropped*.

**2. Cleaning is a knife-edge.** The E4 lint (drop stale facts sharing exact
`(namespace, subject, predicate)`, keep newest) is a real, judge-independent
win — it lifted `synth_sub` and knowledge-update, not just judge-lenient
paraphrases. But it is a **local optimum**: loosening the key (T3, normalized
entity/relation resolution) helps knowledge-update (+0.029) yet over-merges
distinct cross-session entities and craters multi-session (−0.158), for a net
loss. Tightening isn't possible; the exact key is already minimal.

**3. The model is not the lever within free hardware.** The judge-free
`synth_sub_rate` is **flat across three model classes** — gemma-4b 0.136,
granite-8b-q4 0.128, qwen2.5-7b 0.140 (a 0.012 spread = noise). M1's apparent
gemma-judge drop to 0.446 is a **judge-style artifact**: a weak 4B judge grades
its own model's phrasing (E4) more favorably than qwen's differently-worded but
equally-correct answers (M1 was tied on exact match, just abstained more). No
local model that fits our GPUs (8GB Tesla P4 / 6GB RTX 3060) produces better
answers.

## What would break the ceiling (not pursued)

- **A genuinely large synth model** (27B+ or a strong cloud model). Needs GPU
  capacity we don't have, or per-run cloud spend that was ruled out. This is
  the single highest-upside untested axis.
- **A different answer step** — multi-hop reasoning over facts, or
  self-consistency voting (sample N answers, majority-vote). Multiplies compute
  per item; not attempted.

## What shipped from this work

- **Winning config, default-on:** v3 never-abstain synth prompt +
  `CHRONIK_MEMORY_LINT` exact-key supersession lint.
- **Rejected levers, kept default-off** (code retained, env-gated):
  `CHRONIK_MEMORY_CURATION` (T3), `CHRONIK_MEMORY_TEMPORAL` (T1),
  `CHRONIK_MEMORY_ENTITY_RECALL` (T2), vector recall.
- **Harness infrastructure:** `LONGMEMEVAL_EXTRACTION_PROVIDER` (replay cached
  cloud extraction for free), `LONGMEMEVAL_SYNTH_MODEL` (isolate the synth
  model from the judge — fixes the E5 confound).
- **Chronik product fixes surfaced en route** (v2.7.4/v2.7.5): DeleteTopics
  actually deletes (protocol version cap), on-disk WAL reclamation on delete,
  and WalIndexer orphan-GC — reclaimed 88GB and unclogged the indexer.
