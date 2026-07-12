# Memory Quality Roadmap — from 0.172 to the 0.70 gate

**Status**: PROPOSED (2026-07-04) — successor plan to [ROADMAP_AGENT_MEMORY.md](ROADMAP_AGENT_MEMORY.md) Phase 2 quality track
**Baseline**: LongMemEval-S 500-item balanced, cluster v2.7.1, gpt-4o-mini extractor+synthesizer: `synth_judge_rate = 0.172` (86/500). Raw recall judge_rate = 0.262 (131/500).
**Goal**: Phase 2 exit gate `synth_judge_rate ≥ 0.70`.
**Inputs**: (a) full-fleet failure-mode analysis over all 500 item logs, (b) code audit of `crates/chronik-memory`, (c) external research on LongMemEval SOTA systems. See Appendix for evidence.

---

## The three failure modes, quantified

Full-fleet log analysis (all 500 items, cluster v2.7.1 run) breaks the misses down precisely:

| Category | total | raw-hit | synth-hit | raw-hit→synth-MISS | n_results=0 | recall >50s |
|---|---|---|---|---|---|---|
| temporal-reasoning | 133 | 31 | 15 | **22** | 29 | **25** |
| multi-session | 133 | 39 | 22 | **30** | 20 | 11 |
| knowledge-update | 78 | 30 | 27 | 9 | 19 | 12 |
| single-session-user | 70 | 24 | 21 | 9 | 18 | 0 |
| single-session-assistant | 56 | 1 | 1 | 0 | **36** | 7 |
| single-session-preference | 30 | 6 | 0 | **6 (100%)** | 14 | 2 |
| **TOTAL** | **500** | **131** | **86** | **76** | **136** | **57** |

### FM-1: Synthesis destroys recall value (biggest single lever)

Synthesis **loses 76 items that raw recall already answered** and gains only 31 — net −45. If synthesis merely never downgraded a raw hit, the headline would be ~0.32 — nearly double — with zero retrieval work. 211/500 items (42%) had non-empty recall results and still abstained ("I don't know"). Overall abstain rate: 73.4%.

The preference category is the extreme case: raw recall answered 6/30, synthesis converted **zero** of them. Preference golds are long "the user would prefer..." paragraphs; a factoid-shaped synthesis prompt with a conservative abstention gate can't produce them.

### FM-2: Assistant-stated facts are never extracted

single-session-assistant: 36/56 items returned **zero recall results**; golds are things the assistant said ("Memrise", "Mindful.org", a Borges quote, "your jumpsuit designation was LIV"). Code audit confirms: turn `role` is rendered into the extraction prompt (`v3.rs` formats `[i] {role}: {content}`) but the extracted `FactBody` has **no speaker field** (`src/schema.rs:173`), and the V3 prompt biases toward user-stated facts. This is an extraction gap, not a retrieval or synthesis gap.

### FM-3: Temporal questions need arithmetic + timestamps, and recall times out

temporal-reasoning misses split three ways: 22 raw-hits lost in synthesis (no date math), 29 with empty recall, and **25 with recall latency >50s** (60s = query timeout → partial or empty results). Golds like "21 days", "7 days ago", "4" require (a) timestamps attached to retrieved memories in a machine-usable form and (b) a synthesis step willing to do arithmetic. Timestamps currently reach synthesis only as an RFC3339 string prefix per memory line (`recall.rs:1038`); decay uses them but ranking/synthesis barely does.

---

## The math that frames everything

Two constraints bound what any lever can achieve:

1. **The abstention cap**: only ~30/500 questions are `_abs` (correct answer = "you didn't mention this"). Abstaining on 367 items mathematically caps the score at ~0.33 no matter how good the answered items are. Our precision on answered items is already ~64% (86 hits / ~133 answered) — synthesis precision is not the disaster; *answering more questions correctly grounded* is the game.
2. **The model ceiling is far away**: full-context gpt-4o-mini scores 55.4% on this benchmark; Hindsight hits 83.6% with an OSS-20B model and TiMem 76.9% with gpt-4o-mini. **gpt-4o-mini is not our bottleneck** — architecture is. Published architecture-comparable systems: Zep 71.2%, Supermemory 81.6%, Mastra 84.2% (see Appendix C).

## Workstreams (ranked by expected lift ÷ effort)

### WS-0: Facts as keys, rounds as values [expected +0.10 to +0.20 — the architectural lever]

The LongMemEval paper's own ablation found **pure fact extraction *harms* accuracy** (information loss vs. full sessions), while *fact-expansion* — indexing extracted facts as retrieval keys but returning the underlying conversation rounds as values — gains **+9.4% recall@k and +5.4% accuracy**. Raw-turn RAG systems score **94-98% on single-session-assistant**; compressed-memory systems crater there (we: 0.018). Hindsight (83.6% with a 20B model) similarly stores "narrative" facts (2-5 comprehensive facts per exchange) rather than fragmented triples.

Our current pipeline retrieves and synthesizes from the (subject, predicate, object) fact records themselves — the values ARE the compressed facts. This loses exact wording, numbers, lists, and assistant-side content wholesale.

**Change**: keep the extraction + hybrid indexing exactly as-is (facts remain the searchable keys), but store a `source_round_ref` on every extracted memory pointing at the raw round (we already have `mem.raw.*` topics with full turn content and `source_indexes` on every fact — the linkage exists). At recall time, after RRF ranking, **resolve the top-k facts to their source rounds and pass the raw round text (with speaker labels) to synthesis** alongside the fact summary. Round text preserves assistant wording (fixes single-session-assistant without any schema change), exact numbers (helps temporal/knowledge-update), and preference context (fixes preference).

*Effort: M — recall-side change + synthesis prompt update; extraction unchanged. This subsumes half of WS-2 and converts WS-1.4 from "nice" to "automatic".*

### WS-0b: Round-level granularity [expected +0.03 to +0.06]

The paper also shows round-level decomposition beats session-level for reading. Our 50-turn chunks are far coarser than a "round" (one user+assistant exchange). Store `source_indexes` at round granularity (already per-turn) and resolve WS-0 values per-round, not per-50-turn-chunk — otherwise the "value" passed to synthesis is a 50-turn blob. *Effort: S on top of WS-0.*

### WS-1: Synthesis rescue — stop losing raw hits [expected +0.10 to +0.15]

The single largest, cheapest lever. Recall already answers 131/500; synthesis keeps 55 of them.

1. **Make the v2 synthesis prompt the default** (`CHRONIK_MEMORY_SYNTH_PROMPT=v2` today, `src/recall.rs:1053-1121`). v2 already contains "assistant-stated facts count" and "one clear candidate wins" — it was built for exactly this failure and never promoted. *Effort: S.*
2. **Narrow the abstention gate**: abstain only when recall is empty or all results are below a relevance floor — not whenever the model feels unsure. Guard `_abs` items (correct-abstention questions) with an explicit instruction: "abstain when the memories are on a *different* subject than the question, commit when they are on the *same* subject." *Effort: S. Risk: wrong-commits on true `_abs` items — measure the abstention subset separately; the fleet shows only 76/500 wrong-commits today, so there is headroom.*
3. **Category-shaped answers for preference questions**: detect preference-shaped questions ("what would I prefer", "suggest...for me") and switch synthesis to a "summarize the user's relevant preferences + constraints" template instead of a factoid answer. Preference raw→synth conversion is 0/6 today; the judge accepts paraphrase, so a preference summary grounded in the retrieved memories should convert most raw hits. *Effort: S-M.*
4. **Pass structured metadata to synthesis**: today each memory is a 280-char flattened string. Give the synthesizer `(speaker, timestamp, type, subject, predicate, object)` as structured fields so it can reason about who said what and when. *Effort: M (depends on WS-2 for speaker).*

### WS-2: Speaker attribution — unlock single-session-assistant [expected +0.03 to +0.06]

56 items (11.2% of the set) are near-zero today (0.018).

1. Add `speaker: "user" | "assistant"` to `FactBody` (`src/schema.rs:173`) and the extraction tool schema (`v3.rs:159-172`).
2. Extend the V3 prompt: "Extract facts stated by the ASSISTANT that the user accepted or acted on (recommendations, designations, quotes, URLs); set speaker=assistant."
3. Thread speaker through `render_memory_for_synthesis` (`recall.rs:1002-1039`) so synthesis can answer "what did you tell me about X" from assistant-stated memories.
4. Query-time: questions of the form "what did you say/tell/recommend..." boost `speaker=assistant` memories.

*Effort: M. Note: `TwoPassExtractor` (pilot 9) attacked this obliquely with an entity re-sweep and gained nothing because the schema still couldn't represent the speaker — the field is the missing piece, not more extraction passes.*

### WS-3: Temporal reasoning — timestamps + arithmetic + recall latency [expected +0.06 to +0.12]

External evidence is strong here: timestamped facts + time-aware query expansion gained **+6.8-11.3% temporal recall** in the LongMemEval paper's ablations; Zep's bi-temporal validity intervals moved temporal-reasoning from 45.1% → 62.4% (+38% relative); Hindsight's dedicated temporal retrieval channel got temporal to 79.7% on a 20B model.

133 items (26.6% of the set) at 0.233 (raw was 0.31 before synthesis losses).

1. **Fix the recall >50s timeouts first** (25 items, plus 11 in multi-session and 12 in knowledge-update; likely the query fan-out or HNSW scan under concurrent shards). Profile a temporal query under 10-shard load against the cluster; suspects are the vector channel fan-out and the SQL channel. This is a straight perf bug worth ~+0.02 alone across categories. *Effort: M.*
2. **Session/turn timestamps into extraction**: LongMemEval sessions carry dates; `Turn.ts` exists (`extractor/mod.rs:46`) but the eval feeds turns without them, so `valid_from` ≈ ingestion time for everything and date arithmetic is impossible. Wire session dates into `Turn.ts` in the harness and use `turns[0].ts` as the chunk baseline. *Effort: S-M — mostly harness + one extractor change.*
3. **Synthesis date-math instruction**: with real timestamps attached, add to the synthesis prompt: "For 'when/how long/how many times' questions, compute from the memory timestamps; show the computed value." *Effort: S.*
4. **Intent-adaptive fanout**: raise `DEFAULT_FANOUT_SIZE` (50, `recall.rs:47`) to 150 for temporal/numeric intents — count-style questions ("how many times did X") need many events retrieved, not the top-10. *Effort: S.*

### WS-4: Retrieval depth — raise the raw ceiling [expected +0.05 to +0.10]

Raw recall at 0.262 is the ceiling on everything downstream (SOTA memory systems report much higher retrieval recall on this benchmark; see Appendix C).

1. **Fanout 50→150 globally** + k=10 stays; RRF absorbs the extra candidates. *Effort: S.*
2. **Query decomposition for multi-session questions**: "did I mention X before or after Y?" needs 2+ sub-queries. Decompose with one cheap LLM call at recall time; union the results before RRF. multi-session is the largest category (133) and its raw-hit rate (0.293) has the most absolute headroom. *Effort: M.*
3. **Numeric objects as JSON numbers**: `intent_boost` checks `object.is_number()` (`ranking.rs:238`) but the extractor stringifies ("$750K"), so the numeric boost rarely fires. One prompt line. *Effort: S.*
4. **Session-summary channel**: extract a 2-3 sentence per-session summary at ingest (cheap with the existing chunking) and index it as a `concept`-type memory. Session-level retrieval is what several published systems attribute their multi-session gains to; our infrastructure (concept type, type_weight=1.5) already exists behind `LONGMEMEVAL_USE_CONCEPTS`. *Effort: M.*

### WS-5: Extractor/synthesizer model quality [expected +0.03 to +0.10, costs money]

The whole pipeline currently runs on gpt-4o-mini. Before investing heavily in WS-1..4 tuning, run a 100-item A/B with a stronger synthesizer (gpt-4o or Claude Sonnet) while keeping the mini extractor — synthesis quality is implicated in FM-1, and a stronger judge-facing model may convert raw hits that prompt engineering can't. Extraction A/B second (extraction volume is 10× synthesis volume, so cost scales differently). *Effort: S to run; decision input for where to spend prompt-tuning time.*

---

## Sprint 1 — RESULT (2026-07-04): synth_judge_rate = 0.240 (+40% over baseline)

Shipped: v2 synthesis default + narrowed abstention gate (WS-1.1/1.2), preference template (WS-1.3), fanout 50→150 (WS-4.1), numeric-object JSON numbers (WS-4.3). Fleet: `lme-runner:sprint1`, tenant `lme500sprint1`, same cluster/image (`chronik-server:v2.7.1-flusher`), 10×50 items, zero Chronik-side failures.

| Metric | Baseline (v2.7.1) | Sprint 1 | Δ |
|---|---|---|---|
| **synth_judge_rate** | **0.172** | **0.240 (120/500)** | **+40%** |
| raw judge_rate | 0.262 | 0.298 | +14% |
| synth_abstain_rate | 0.734 | 0.656 | −11% |
| n_results=0 items | 136 | **77** | **−43%** |
| raw-hits lost by synthesis | 76 | 63 | −17% |

Per-category (raw judge, from fleet logs):

| Category | Baseline raw→synth | Sprint 1 raw→synth | Verdict |
|---|---|---|---|
| single-session-user | 24→21 | 28→27 | conversion nearly lossless now |
| multi-session | 39→22 | 45→31 | better, still −27 lost — biggest remaining synth loss |
| knowledge-update | 30→27 | 27→27 | lossless conversion |
| single-session-preference | 6→0 | 10→**8** | **preference template worked** (0% → 80% conversion) |
| temporal-reasoning | 31→15 | 36→24 | better; still −18 lost, needs timestamps/date-math |
| single-session-assistant | 1→1 | 3→3 | still the crisis: 26/56 items have n_results=0 |

Lever attribution: fanout 150 nearly halved empty-recall items (136→77) and lifted raw +18; the synthesis changes converted raw hits at 58% (was 42%); the preference template alone took that category's conversion from 0/6 to 8/10.

**What Sprint 1 did NOT fix** (in line with the plan): single-session-assistant (needs WS-0 rounds-as-values + WS-2 speaker attribution — the facts simply aren't in the index), multi-session synthesis losses (needs WS-0 structured round context), temporal losses (needs WS-3 session timestamps + date math). These are Sprint 2/3 items.

## Sprint 2 — RESULT (2026-07-04): synth_judge_rate = 0.328 (+37% over Sprint 1, +91% over baseline)

Shipped: WS-0 rounds-as-values (`source.excerpt` on every typed memory — cited turns verbatim, capped 700 chars — rendered as quoted `|` blocks in the synthesis prompt, plus raw phrasing landing in the BM25 index), WS-0b round granularity (excerpt = cited turns, not the 50-turn chunk), WS-3.2 session timestamps (`haystack_dates` → `Turn.ts` → `[YYYY-MM-DD HH:MM]` excerpt prefixes; question anchored with "(Today is {question_date}.)"). `valid_from` deliberately left at ingestion time — bi-temporal ranking is Sprint 3. Fleet: `lme-runner:sprint2`, tenant `lme500sprint2`, zero Chronik-side failures; 3 items degraded by OpenAI 429 rate limiting in the tail (all 10 shards hit the RPM ceiling simultaneously — add extractor 429 retry-with-backoff before Sprint 3).

| Metric | Baseline | Sprint 1 | Sprint 2 |
|---|---|---|---|
| **synth_judge_rate** | 0.172 | 0.240 | **0.328 (164/500)** |
| raw judge_rate | 0.262 | 0.298 | **0.358** |
| synth_abstain_rate | 0.734 | 0.656 | **0.546** |
| n_results=0 items | 136 | 77 | **44** |
| raw-hits lost / gained by synthesis | −76/+31 | −63/+34 | −69/+54 |

Per-category (raw→synth from fleet logs):

| Category | Sprint 1 | Sprint 2 | Verdict |
|---|---|---|---|
| single-session-user | 28→27 | 30→**34** | synthesis now GAINS over raw (excerpts carry answers facts missed) |
| multi-session | 45→31 | 61→40 | raw +16 from excerpt-in-BM25; still −34 in synthesis — largest remaining loss |
| knowledge-update | 27→27 | 32→**39** | gains; dated excerpts resolve update ordering |
| single-session-preference | 10→8 | 7→8 | flat (raw dipped, conversion held) |
| temporal-reasoning | 36→24 | 47→**40** | raw +11, conversion 51%→85% of the way; date anchors working |
| single-session-assistant | 3→3 | 2→3 | **unmoved, as predicted** — extraction still skips assistant-stated facts (WS-2) |

Lever attribution: the excerpt in the index lifted raw recall +30 items (n_results=0 down 77→44); dated excerpts + question anchoring took temporal synth from 24→40; synthesis now *gains* on single-session-user and knowledge-update because the quoted rounds carry answer wording the fact summary lost.

**Remaining big rocks for Sprint 3** (in expected-value order):
1. **single-session-assistant extraction** (56 items, 0.036): the V3 prompt still extracts user-centric facts; assistant recommendations/names/quotes never become memories. WS-2 speaker field + explicit "extract assistant-stated facts" prompt rule.
2. **multi-session synthesis losses** (−34): raw finds the sessions, synthesis can't stitch multiple excerpts. Chain-of-note reading strategy (Appendix C #5) or k>10 for multi-session intents.
3. **OpenAI 429 retry** in the extractor (eval-infra, protects measurement fidelity).
4. Recall >50s latency bug (WS-3.1, deferred from Sprint 2).

## Sprint 3 — STATUS (2026-07-05): code complete + verified, measurement pending

Shipped (all tests green, 450/450): WS-2 speaker attribution end-to-end (`FactBody.speaker`, wire-level `RawFact.speaker`, V3 prompt speaker rules + tool schema, synthesis rendering, `QueryIntent::AssistantStated` + 1.8× boost), chain-of-note reading in the v2 synthesis prompt, harness k 10→15, OpenAI 429 retry-with-backoff in the extractor.

**Two fleet runs invalidated by an indexing race, not by the levers**:
- Run 1 (0.182): empty recalls went 17/250 → 172/250 between first and second half — the cluster's WalIndexer fell hours behind, and recall only ever hit the in-memory hot-text tail, which Sprint 3's ~2× fact volume outran. Direct proof the code works: post-run, the index contains `{"predicate":"recommended_language_app","object":"Memrise","speaker":"assistant"}` for an item that scored n_results=0 at eval time.
- Run 2 (killed early, no spend): new index-readiness polling correctly refused to measure — 0 facts visible after 300s even for item 1.

**Root cause of the indexer collapse**: `mem.fact/event/instruction/task` topics are created `vector.enabled=true`, so the WalIndexer synchronously embeds every extracted fact through OpenAI before indexing the next segment. Four fleets of debris (597 mem.* topic dirs) queued the indexer hours deep. Fixes landed: `CHRONIK_MEMORY_VECTOR_TOPICS=false` env gate (topics.rs) + harness readiness polling (`LONGMEMEVAL_INDEX_TIMEOUT_MS`, warns on timeout).

**To resume**:
1. Wipe `mem.*` topic data on the Thunderbird pods (user approval pending; DV datasets untouched) + restart pods.
2. Top up OpenAI (~$15/run; the $50 top-up was consumed by ~3.5 fleet runs at ~$13-15 each — extraction cost, not embeddings).
3. Re-run the fleet (`lme-runner:sprint3b` image already distributed; job YAML at `/tmp/lme-runner/job-sprint3b.yaml`) with `CHRONIK_MEMORY_VECTOR_TOPICS=false` added to the job env.
4. Aggregate vs the 0.328 Sprint 2 baseline. Mid-run signal from the invalidated run (first-half items, before the index fell behind): synthesis conversion near-lossless; raw recall possibly diluted by assistant-fact volume — watch the ssu/KU/temporal categories for dilution, ssa for the win.

## Sprint 3 — RESULT (2026-07-08): synth_judge_rate = 0.336 (flat on aggregate, structural win on shape)

Shipped: WS-2 speaker attribution end-to-end (`FactBody.speaker`, wire-level `RawFact.speaker`, V3 prompt speaker rules + tool schema, synthesis rendering, `QueryIntent::AssistantStated` + 1.8× boost), chain-of-note reading in the v2 synthesis prompt, harness k 10→15, OpenAI 429 retry-with-backoff. Clean run: `lme-runner:sprint3c` image, tenant `lme500s3i`, **gpt-4o-mini + full V3** (identical extractor to the 0.328 baseline — a like-for-like measurement), cluster v2.7.1, 10 shards, `CHRONIK_MEMORY_VECTOR_TOPICS=false`. 56 chunk-level failures (OpenAI transient, non-fatal). Cost ~$13 of a promo-topped $149 balance.

Getting here also required an infra gauntlet (all documented in the `memory-quality-sprints` project memory): LM Studio crash/JIT-context reset, ES-compat `hits.total` size:0 quirk, per-node cold-index visibility race, recall channel fault-tolerance, tool-JSON truncation on the local 30B (fixed with a slim V3-lite schema), stale-tenant-after-wipe produce hangs, and a physical node outage (dell-2 down → node-1 local PVC unschedulable → MAAS power-cycle). The local-model (qwen3-coder-30b) path validated end-to-end at $0 but was set aside for this run in favor of the like-for-like OpenAI comparison.

| Metric | Baseline | Sprint 1 | Sprint 2 | Sprint 3 |
|---|---|---|---|---|
| **synth_judge_rate** | 0.172 | 0.240 | 0.328 | **0.336 (168/500)** |
| raw judge_rate | 0.262 | 0.298 | 0.358 | 0.332 |
| synth_abstain_rate | 0.734 | 0.656 | 0.546 | 0.526 |

Per-category (raw→synth from fleet logs):

| Category | Sprint 2 synth | Sprint 3 synth | Verdict |
|---|---|---|---|
| **single-session-assistant** | ~0.036 | **0.179 (10/56)** | **WS-2 speaker attribution cracked the 3-sprint crisis category — ~5×** |
| knowledge-update | 0.500 | 0.500 (39/78) | held |
| multi-session | 0.301 | 0.316 (42/133) | +raw, still −26 in synthesis |
| temporal-reasoning | 0.301 | 0.308 (41/133) | synth GAINS +18 over raw (41 vs 35) — chain-of-note + dated excerpts |
| single-session-preference | 0.267 | 0.267 (8/30) | flat |
| single-session-user | ~0.486 | **0.386 (27/70)** | **DOWN — the dilution** |

**Interpretation**: Sprint 3's headline is flat (+0.008, within noise), but the *shape* improved structurally. The speaker-attribution lever did exactly what it was designed to — single-session-assistant went 0.036 → 0.179, the category no memory-system change had moved in three sprints. That win is offset almost exactly by dilution: the extra assistant-stated facts (and the 1.8× AssistantStated boost) crowd the top-k for single-session-user, dropping it ~0.10. Net +4 items. Synthesis is now net-additive on 4 of 6 categories (temporal, ssu, preference, ssa all gain in synth vs raw) — the chain-of-note + k=15 changes work. No category is near-zero anymore.

**Sprint 4 priorities** (bank the Sprint 3 gain by killing the dilution):
1. **Gate the AssistantStated boost on intent** — apply the 1.8× only for assistant-shaped questions ("what did you tell me…"), not globally. Right now assistant facts are boosted on every query, sinking user-fact recall. This is the single highest-value fix — should recover ssu without losing ssa.
2. **Cap assistant-fact extraction volume** — the V3 speaker prompt may over-extract low-value assistant statements. Add a selectivity instruction / per-chunk cap.
3. **multi-session synthesis stitching** — still the largest synthesis loss (−26). Query decomposition or a higher k for multi-session intents.
4. Re-run cheaply: the `openai-v3` extraction cache is now populated (this run filled it), so a Sprint 4 recall/ranking-only change re-runs for ~$1-2, not $13.

## Sequencing and measurement

**Sprint 1 — synthesis + cheap retrieval wins (all S/M)**: WS-1.1 + WS-1.2 (v2 synth prompt default + narrowed abstention), WS-4.1 (fanout 150), WS-4.3 (numeric objects), WS-3.4 (intent-adaptive fanout). Re-run 500-item fleet. Expected: 0.172 → 0.26-0.32.

**Sprint 2 — the architectural lever**: WS-0 + WS-0b (facts as keys, rounds as values, round granularity), WS-1.3 (preference template), WS-3.2 (session timestamps into Turn.ts). Re-run. Expected: → 0.40-0.50. This sprint carries the strongest external evidence (raw-turn values fix single-session-assistant at the source, +9.4% recall from fact-expansion).

**Sprint 3 — temporal + depth**: WS-3.1 (recall >50s latency bug), WS-3.3 (date-math synthesis), WS-2 (speaker field, now cheap on top of WS-0), WS-4.2 (query decomposition), WS-4.4 (session summaries). Expected: → 0.50-0.60.

**Beyond 0.60**: WS-5 (model A/B) + cross-encoder reranking (Hindsight's 4th channel) + bi-temporal validity intervals (Zep's approach) — re-derive from the Sprint 3 failure analysis. Published systems show 0.70+ is reachable with mini-class models given the right architecture (TiMem 76.9% on gpt-4o-mini), so the Phase 2 gate does not require a model upgrade.

**Measurement discipline** (unchanged from ROADMAP_AGENT_MEMORY.md): every sprint ends with a full 500-item fleet on the cluster (never single-node, never a pilot subset — both have burned us); per-category table archived in this file; no lever ships without a before/after fleet number.

---

## Appendix A: Evidence — fleet log analysis (2026-07-04)

- Analysis over `/tmp/lme-analysis/all.log` (all 10 shard logs, 500 item lines, job `lme500-oai-cluster-v271`).
- raw-hit = judge HIT on raw recall grading; synth-hit = judge HIT on synthesized answer.
- 211 items abstained despite n_results>0; 136 items had n_results=0; 57 items had recall latency >50s (query timeout 60s).
- Example assistant-category golds with n_results=0: "Memrise", "Mindful.org.", "International Budget Hostel", "The designation on your jumpsuit was 'LIV'."
- Example preference pattern: item `caf03d32` — raw judge HIT (rec found the slow-cooker context), synth abstained → miss.

## Appendix B: Evidence — code audit refs

| Finding | Location |
|---|---|
| No speaker field on facts | `crates/chronik-memory/src/schema.rs:173` (FactBody) |
| Role rendered into prompt but discarded | `src/extractor/prompts/v3.rs:226` |
| v2 synthesis prompt exists, not default | `src/recall.rs:1053-1121`, env `CHRONIK_MEMORY_SYNTH_PROMPT` |
| Abstention literal + tolerant detection | `src/recall.rs:83,1128-1138` |
| DEFAULT_FANOUT_SIZE = 50, k = 10 | `src/recall.rs:47,149` |
| Intent 3-tier classifier + boosts | `src/ranking.rs:143-288` |
| Numeric boost requires JSON-number object | `src/ranking.rs:238-239` |
| Timestamp only as string prefix in synthesis | `src/recall.rs:1038` |
| Turn.ts optional, unused by eval harness | `src/extractor/mod.rs:46`, `tests/eval_longmemeval.rs` |
| Concept channel exists behind flag | `src/recall.rs:236-256,441-467`, `LONGMEMEVAL_USE_CONCEPTS` |
| Semantic dedup, no NL contradiction detection | `src/compaction.rs:207` |

## Appendix C: External research (LongMemEval SOTA, researched 2026-07-04)

### Published scores (LongMemEval-S, judge-graded accuracy)

| System | Overall | Notable per-category | Source |
|---|---|---|---|
| Full-context GPT-4o (baseline) | 60.2% | temporal 45.1%, KU 78.2%, ssa 94.6% | [Zep paper](https://arxiv.org/abs/2501.13956) |
| Full-context gpt-4o-mini | 55.4% | — | [Zep paper](https://arxiv.org/abs/2501.13956) |
| Oracle (evidence-sessions-only, GPT-4o) | 87.0% | — | [LongMemEval paper](https://arxiv.org/html/2410.10813v2) |
| Zep/Graphiti (gpt-4o) | 71.2% | temporal 62.4%, KU 83.3%, ssa 80.4%, ssp 56.7%, multi 57.9% | [Zep paper](https://arxiv.org/html/2501.13956v1) |
| Mem0 | 66.9% (3rd-party) | no first-party publication | [ByteRover](https://www.byterover.dev/blog/benchmark_ai_agent_memory_real_production_byterover_top_market_accuracy_longmemeval) |
| Supermemory (gpt-4o) | 81.6% | ssu 97.1%, ssa 96.4%, ssp 70.0%, KU 88.5%, temporal 76.7%, multi 71.4% | [Supermemory](https://x.com/DhravyaShah/status/1994106663192924546) |
| Hindsight (OSS-20B / Gemini-3) | 83.6% / 91.4% | temporal 79.7→91.0%, KU 84.6→94.9%, ssa 94.6% | [Hindsight](https://arxiv.org/html/2512.12818v1) |
| Mastra Observational Memory (gpt-4o / gpt-5-mini) | 84.2% / 94.9% | ssp 100%, temporal 95.5%, KU 96.2%, multi 87.2% | [Mastra](https://mastra.ai/research/observational-memory) |
| ByteRover | 92.2-92.8% | vendor-reported | [ByteRover](https://www.byterover.dev/blog/benchmark_ai_agent_memory_real_production_byterover_top_market_accuracy_longmemeval) |

(ssu/ssa/ssp = single-session user/assistant/preference; KU = knowledge-update.)

### Techniques with measured gains, ranked by evidence strength

1. **Facts as keys, rounds as values ("fact-expansion")**: pure fact extraction *harms* accuracy per the original paper's ablation; fact-expansion of round values gains +9.4% recall@k, +5.4% accuracy. Hindsight likewise credits *narrative* facts (2-5 comprehensive facts/exchange) over fragmented triples. → our WS-0.
2. **Round-level value granularity**: decomposing sessions into rounds significantly improves reading accuracy (same paper). → WS-0b.
3. **Timestamped facts + time-aware query expansion**: +6.8-11.3% temporal recall (paper); Zep bi-temporal intervals: temporal 45.1→62.4%; Hindsight temporal channel: 79.7% on a 20B model. → WS-3.
4. **Hybrid 4-way retrieval (semantic + BM25 + graph + temporal) + cross-encoder rerank**: Hindsight's architecture, closest published analogue to ours, 83.6% with a small model. We have 2 of 4 channels; NB our pilot-11 BGE reranker regressed — Hindsight's rerank operates over round values, not fact fragments, which may explain the difference. → Sprint 3+.
5. **Reading strategy (Chain-of-Note + structured JSON context)**: up to +10 absolute points at synthesis time (paper). → WS-1.4.
6. **Dense observation logs (Mastra)**: 84.2% with gpt-4o — but an architecture replacement, not a bolt-on; noted, not planned.
7. **Extractor model quality matters less than architecture**: GPT-4o extracts fewer/more-unique facts than mini ([Knowledge-Instruct](https://arxiv.org/pdf/2504.05571)), yet Hindsight scores 83.6% with an OSS-20B and TiMem 76.9% with gpt-4o-mini. → WS-5 is a tuning input, not a precondition.

### Gotchas confirmed by published error analyses

- single-session-assistant answers live in assistant turns and depend on exact wording; raw-turn RAG scores 94-98% there while compressed-memory systems crater ([MemPro/AtomMem analyses](https://arxiv.org/pdf/2606.00619)). Matches our 0.018 exactly.
- Only ~30/500 questions are `_abs` → a 73% abstain rate mathematically caps the score near 0.33. Recall coverage, not synthesis precision, is the binding constraint.
- Temporal questions need arithmetic over timestamps; time-agnostic indexes fail structurally.
- On vendor numbers: treat ByteRover/Supermemory self-reported figures with caution ([Zep's critique of Mem0's claims](https://blog.getzep.com/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/) illustrates the reproducibility problem in this space).

## CONCLUSION (2026-07-07): Vector search is NOT the lever — pivot to compounding synthesis

**The measurement.** After fixing every infrastructure confound (cold-embed decoupling v2.7.2, dedicated embed runtime v2.7.3, debris wipe + pod restart, and finally **multi-partition memory tenants** to spread indexing across nodes), the first clean full-500 hybrid run (`lme500-s12`: 10 shards, **0% contamination, 90% non-empty recall**) scored **synth_judge_rate = 0.262** — *uniformly below* the 0.336 text-only baseline in every category:

| Category | text-only 0.336 | s12 hybrid 0.262 |
|---|---|---|
| knowledge-update | 0.513 | 0.397 |
| single-session-user | 0.386 | 0.229 |
| temporal-reasoning | 0.308 | 0.203 |
| multi-session | ~0.32 | 0.271 |
| single-session-preference | 0.267 | 0.167 |
| single-session-assistant | 0.179 | 0.107 |

A *uniform* drop is the tell: a vector-specific effect would be *mixed* (help paraphrase, hurt elsewhere). This is systemic — partly multi-partition **BM25 IDF fragmentation** (3 Tantivy indexes, each 1/3 the docs), partly genuine **vector dilution** of an already-strong lexical pipeline. Both point negative.

**Why the literature agrees for our setup** (not the generic "hybrid is gold standard"):
1. Benchmark wins are hybrid vs *vector-alone* (~92% vs low-80s Recall@5 on LongMemEval), not hybrid vs *BM25-alone*. LongMemEval is keyword/entity-heavy (names, dates, preferences) = BM25 territory.
2. Adding BM25 helps *weak* embedding models but *degrades* sophisticated pipelines that already encode lexical+semantic signal. Inverse applies: we retrieve LLM-extracted facts + verbatim source excerpts (WS-0) — keyword-dense, exactly what BM25 wants. The vector channel dilutes.
3. **Karpathy's LLM Wiki (May 2026)** explicitly rejects embedding-RAG as primary retrieval ("nothing is built up"), favoring keyword-first over a curated index + **compounding synthesis**.

**The realization: our memory system already IS the Karpathy wiki pattern** — extraction into typed facts + synthesis + excerpts. Every real gain (0.172→0.336) came from *synthesis/structure* levers, not retrieval method.

**Reprioritized roadmap (away from vector):**
1. **Concept-page rollups** (`synthesize_concept`/`remember_concept`, `include_concepts`) — Karpathy's "compound once" idea; make it default and measure.
2. **Contradiction/lint passes** — detect outdated/conflicting facts; targets the weakest clean-run categories (knowledge-update, temporal).
3. **Extraction quality + structured round context.**
4. **Vector**: keep available (helps the narrow single-session-user paraphrase case, +0.11 in s5b), default **off** per-topic for keyword/cost-sensitive workloads.

Server fixes shipped while chasing this (cold-embed decoupling, dedicated embed runtime, multi-partition memory tenants) are genuine product wins regardless — they make hybrid/vector work correctly at scale for anyone who *does* want it.

---

## UPDATE (2026-07-08): Concept-page rollups ALSO falsified — the lever is the ANSWER STEP, not retrieval

Ran the #1 reprioritized item above (concept-page rollups) as a clean full-500: concepts ON, vector OFF, fresh tenant `lme500s13`, warm extraction cache (~$2).

| metric | concepts ON (s13) | text-only baseline | vector (s12) |
|---|---|---|---|
| **synth_judge_rate** | **0.268** (134/500) | **0.336** | 0.262 |
| raw judge_rate | 0.258 | — | — |
| **synth_abstain_rate** | **0.610** (305/500) | — | — |

Per-category (all suppressed vs text-only): knowledge-update 0.359, multi-session 0.331, temporal 0.226, single-session-preference 0.233, single-session-user 0.171, single-session-assistant 0.143.

**Conclusion: concept rollups land at 0.268 — right where vector did (0.262), ~0.07 below text-only.** The concept page inlines at rank-0 with 1.5× `type_weight`, displaces the specific answer-bearing fact in the k=10, and the model abstains or errs. **Two "add a retrieval surface" levers are now falsified (vector + concept-rollups).** The pattern is unambiguous: the atomic-extraction pipeline is already strong; *anything that competes with it in the k=10 dilutes.* Do not try a third retrieval-surface variant.

**The real signal is abstention.** 61% of items get "I don't know," yet answered items are ~69% accurate. The bottleneck is a too-conservative **synthesis/answer step**, not retrieval breadth. Reprioritized again:

1. **Reduce abstention** (NEW #1) — tune the synthesis prompt to commit more often. Largest recoverable pool; orthogonal to retrieval; adds no competing surface. First: confirm the text-only baseline's own abstain rate to know if this is pure upside or whether concepts caused the spike.
2. **Contradiction/lint on the atomic facts** — *cleans* the existing surface rather than adding one (the distinction that matters).
3. ~~Concept-page rollups~~ — FALSIFIED (0.268). Code retained (`synthesize_concept`/`include_concepts`), default OFF like vector.
