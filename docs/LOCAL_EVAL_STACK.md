# Local Eval Stack (LongMemEval, zero-API-cost)

How to run the memory-quality / LongMemEval evaluation entirely against
**local, self-hosted models** — no paid API calls. This documents the *method*
and the *environment variables*; the concrete host addresses for one particular
lab live in a private, out-of-repo `eval-stack.env` (see
[Concrete addresses](#concrete-addresses-out-of-repo)) and are intentionally not
committed.

The harness is the `#[ignore]`d integration test in `chronik-memory`:

```
crates/chronik-memory/tests/eval_longmemeval.rs   ::  evaluate_longmemeval
```

## Why external servers (not a built-in embedder)

Chronik has a `local-models` cargo feature for an in-process ONNX embedder, but
it is **disabled** — the `ort`/`tokenizers` deps are commented out in
`crates/chronik-embeddings/Cargo.toml` (ONNX Runtime `ort 2.0` was still RC when
this was written). So "local models" here means **separate model servers on your
network, exposed over an OpenAI-compatible HTTP API**, which Chronik and the eval
harness talk to as if they were OpenAI. Any server that speaks
`/v1/chat/completions` and `/v1/embeddings` works (mlx_lm, Ollama, vLLM,
llama.cpp, LocalAI, …).

## Roles

The eval uses up to three model roles. Each is just an OpenAI-compatible endpoint
+ a model id; run them wherever you like (one box or several).

| Role | Serves | Used by | Notes |
|------|--------|---------|-------|
| **Reader / synthesizer + extractor** | `/v1/chat/completions` | the eval harness | Answers questions from recalled memories; also does write-time extraction. |
| **Judge** | `/v1/chat/completions` | the eval harness | Grades answers. **Must be a different model than the reader** — a self-judge inflates the score badly (observed 0.278 → 0.667). |
| **Embedder** | `/v1/embeddings` | Chronik broker | Only needed for the vector channel; the read-time text path does not use it. |

## 1. Stand up the model servers

Examples with two common local servers. Substitute your own hosts/ports/models.

**Chat models (reader + judge) — e.g. Apple `mlx_lm`:**

```bash
# Reader/extractor/synthesizer — pick a NON-thinking (or thinking-off) model,
# the harness assumes max_tokens≈1024 and does not budget for reasoning tokens.
python3 -m mlx_lm server \
  --model <chat-model-id> --host 0.0.0.0 --port 8080 \
  --chat-template-args '{"enable_thinking": false}'

# Judge — a DIFFERENT model, on another port
python3 -m mlx_lm server \
  --model <judge-model-id> --host 0.0.0.0 --port 8082
```

Verify a chat model actually honors `enable_thinking` before relying on it
(some models are thinking-native with no off switch and are unusable for
high-volume extraction).

**Embedder — e.g. Ollama (OpenAI-compatible, only for the vector channel):**

```bash
OLLAMA_HOST=0.0.0.0:11434 ollama serve &
ollama pull <embedding-model>          # e.g. a 768-dim text-embedding model
# endpoint: http://<embed-host>:11434/v1/embeddings
```

## 2. Wire the Chronik broker (embeddings only)

Run a normal single-node broker; point its embedding provider at your local
embedder. Ollama et al. ignore the API key, but the `openai` provider may insist
on a non-empty `OPENAI_API_KEY`, so pass a dummy.

```bash
export CHRONIK_EMBEDDING_PROVIDER=openai
export CHRONIK_EMBEDDING_ENDPOINT="http://<embed-host>:<embed-port>/v1/embeddings"
export CHRONIK_EMBEDDING_MODEL="<embedding-model>"
export CHRONIK_EMBEDDING_DIMENSIONS=768          # match your model
export OPENAI_API_KEY=local-dummy                # provider wants it non-empty
export CHRONIK_DEFAULT_SEARCHABLE=true           # read-time path needs BM25 topics

./target/release/chronik-server start --data-dir ./eval-data
```

The read-time-extraction eval needs raw memory topics created BM25-searchable.
If your build predates that being the default, also set
`CHRONIK_MEMORY_RAW_SEARCHABLE=true`.

## 3. Run the harness

The test is gated behind `--ignored` and a few env vars. `ANTHROPIC_API_KEY` only
has to be **present** (a dummy is fine) — the actual LLM calls go to your local
endpoints via the `LONGMEMEVAL_*_PROVIDER=local` overrides.

```bash
CHRONIK_INTEGRATION=1 \
ANTHROPIC_API_KEY=local-dummy \
CHRONIK_API=http://localhost:6092 \
CHRONIK_KAFKA=localhost:9092 \
\
LONGMEMEVAL_PATH=datasets/longmemeval_s_pilot20.jsonl \
LONGMEMEVAL_N=20 \
LONGMEMEVAL_TENANT=pilot20-$(date +%s) \
LONGMEMEVAL_READTIME=1 \
LONGMEMEVAL_USE_SYNTHESIS=1 \
LONGMEMEVAL_SYNTH_K=40 \
LONGMEMEVAL_RECALL_K=40 \
\
LONGMEMEVAL_LLM_PROVIDER=local \
LONGMEMEVAL_LLM_ENDPOINT=http://<chat-host>:8080 \
LONGMEMEVAL_LLM_MODEL=<chat-model-id> \
\
LONGMEMEVAL_USE_LLM_JUDGE=1 \
LONGMEMEVAL_JUDGE_PROVIDER=local \
LONGMEMEVAL_JUDGE_ENDPOINT=http://<judge-host>:8082 \
LONGMEMEVAL_JUDGE_MODEL=<judge-model-id> \
\
LONGMEMEVAL_EXTRACTION_CACHE=/path/to/lme-extraction-cache \
  cargo test -p chronik-memory --test eval_longmemeval evaluate_longmemeval \
  -- --ignored --nocapture
```

### Key environment variables

Authoritative list is whatever `crates/chronik-memory/{src/eval,tests}` reads;
the ones that matter for a local run:

| Variable | Purpose |
|----------|---------|
| `CHRONIK_INTEGRATION=1` | Required gate; without it the test no-ops. |
| `ANTHROPIC_API_KEY` | Must be set (dummy ok when providers are `local`). |
| `CHRONIK_API` / `CHRONIK_KAFKA` | Broker Unified-API (6092) and Kafka (9092) endpoints. |
| `LONGMEMEVAL_PATH` | Dataset jsonl (see `datasets/`). |
| `LONGMEMEVAL_N` | Number of items to evaluate. |
| `LONGMEMEVAL_TENANT` | Namespace for the run; use a **fresh** value each run. |
| `LONGMEMEVAL_READTIME` | `1` = read-time extraction arm; omit for the write-time baseline. |
| `LONGMEMEVAL_USE_SYNTHESIS` | `1` = LLM synthesizes the answer from recalled memories. |
| `LONGMEMEVAL_SYNTH_K` / `LONGMEMEVAL_RECALL_K` | How many memories feed synthesis / recall. |
| `LONGMEMEVAL_LLM_PROVIDER` / `_ENDPOINT` / `_MODEL` | Reader/extractor/synthesizer model. `local` → your endpoint. |
| `LONGMEMEVAL_USE_LLM_JUDGE` | `1` = grade with an LLM judge. |
| `LONGMEMEVAL_JUDGE_PROVIDER` / `_ENDPOINT` / `_MODEL` | Independent judge override (mirrors the LLM one). |
| `LONGMEMEVAL_EXTRACTION_CACHE` | Dir for the extraction cache (see below). |
| `LONGMEMEVAL_USE_VECTOR` / `LONGMEMEVAL_WAIT_VECTOR` | Enable/await the vector channel (needs the broker embedder). |
| `LONGMEMEVAL_INDEX_SLEEP_MS` | How long to wait for indexing before search. |

The reader and judge share one endpoint variable *family* each — there is no
per-call endpoint split, so a thinking-on reader with a non-thinking judge (or
vice-versa) is fine, but you cannot mix thinking/non-thinking within the reader
role in one run.

## Fast iteration: the extraction cache

Write-time extraction over a full dataset is the slow part (~2h). Point
`LONGMEMEVAL_EXTRACTION_CACHE` at a persistent directory and repeated runs replay
extraction in seconds (~20 min end-to-end). The cache key includes the extractor
identity, so **changing the extraction model/prompt (e.g. two-pass) invalidates
it** and forces a full re-extract.

## Interpreting results (context, not a spec)

- A capable local reader does **not** beat the recall/synthesis ceiling on its
  own; model size was repeatedly falsified as the lever.
- The lever that moved it was **read-time extraction** over retained raw turns
  (`LONGMEMEVAL_READTIME=1`), not a bigger model — see
  `docs/LONGMEMEVAL_RESEARCH_CONCLUSION.md` and `docs/ROADMAP_MEMORY_QUALITY.md`.
- Always use an **independent judge**; a self-judge credits abstentions as hits.

## Gotchas

- **Stop the broker by port PID** (`ss -ltnp | grep :9092`), not
  `pkill -f chronik-server` — that also matches your own shell.
- **Wipe the broker data dir between runs** to avoid segment backlog skewing
  indexing latency; use a fresh `LONGMEMEVAL_TENANT` too.
- **Setting `INDEX_SLEEP_MS` too low** can fire the search before indexing
  finishes → 0 recalled turns → 100% abstain. If raw retrieval looks empty,
  raise it before suspecting a real regression.
- **Dummy keys:** `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` must be non-empty even
  when everything is local.

## Concrete addresses (out of repo)

The real hosts, ports, model ids, and paths for the home-lab setup live in a
private `eval-stack.env` outside the repository (kept out deliberately, alongside
the lab-IP scrub in PR #25), driven by two scripts — `stack.sh`
(`status`/`up`/`down`/`restart [--with-models]`) and `run-eval.sh` — that read
that single env file and hard-code no address in any script body. This doc is the
IP-free equivalent: fill the `<...>` placeholders with your own endpoints. To
reproduce the exact lab run, ask the maintainer for that env file.
