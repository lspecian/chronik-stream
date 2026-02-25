# RAG Chatbot Demo

A self-contained Retrieval-Augmented Generation (RAG) chatbot powered by Chronik's full-text search and query orchestrator.

## What It Does

1. Starts a Chronik server with text indexing enabled
2. Loads 50 knowledge base articles (fictional "Acme Cloud Platform" docs) via Kafka protocol
3. Waits for Tantivy/BM25 indexing to complete (~4 seconds)
4. Opens an interactive Q&A loop where you ask questions
5. Chronik retrieves relevant documents via `POST /_query` with ranked text search
6. Retrieved context is sent to an LLM for answer generation
7. Answer displayed with sources, scores, and retrieval stats

## Architecture

```
User Question
     │
     ▼
Chronik /_query API  (text search, BM25 ranking)
     │
     ├─ Tantivy full-text index
     ├─ RRF candidate merging
     └─ RuleRanker (relevance/freshness profiles)
     │
     ▼
Top-K Retrieved Documents
     │
     ▼
LLM Generation (Ollama / OpenAI / Anthropic)
     │
     ▼
Answer + Sources
```

## Prerequisites

- **Chronik server binary** built: `cargo build --release --bin chronik-server`
- **kafka-python**: `pip3 install kafka-python`
- **jq**: `sudo apt install jq`

### LLM Backend (optional, auto-detected)

The demo auto-detects available LLM backends in priority order:

| Priority | Backend | Requirement | Notes |
|----------|---------|-------------|-------|
| 1 | Ollama | Running at `localhost:11434` with a model | Fully offline, no API key |
| 2 | OpenAI | `OPENAI_API_KEY` set | Uses gpt-4o-mini by default |
| 3 | Anthropic | `ANTHROPIC_API_KEY` set | Uses claude-sonnet by default |
| 4 | None | — | Retrieval-only mode (shows documents) |

**Ollama setup** (recommended for offline use):
```bash
# Install Ollama: https://ollama.com
ollama pull gemma3:4b    # 2.3GB, good for generation
```

## Usage

```bash
# From the repository root
bash examples/rag-chatbot/demo.sh
```

### Example Session

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  RAG Chatbot Demo — Powered by Chronik
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  Ollama detected — using gemma3:4b for generation (local, no API key)

Starting Chronik server...
  Server ready (PID 12345)

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Loading Knowledge Base
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  Produced 50/50 articles

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  RAG Chatbot Ready
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

You: How do I authenticate API requests?

  Searching knowledge base...
  Found 5 documents (from 50 candidates, 3ms)

  Sources:
  [0] Authentication (getting-started, score: 12.45)
  [1] Quick Start Guide (getting-started, score: 8.21)
  [2] Rate Limits (api, score: 5.67)

  Assistant: All API requests require authentication via Bearer token
  in the Authorization header. Example: Authorization: Bearer your-api-key.
  API keys can be created and rotated from the dashboard under Settings >
  API Keys. For service-to-service auth, use OAuth2 client credentials
  flow with your client ID and secret.
```

### Example Questions

- "How do I authenticate with the API?"
- "What instance types are available?"
- "How do I fix a 502 bad gateway error?"
- "What is the SLA uptime guarantee?"
- "How do I migrate from AWS?"
- "What are the API rate limits?"
- "How does auto scaling work?"
- "What encryption is used for data at rest?"
- "How do I set up budget alerts?"
- "What Kubernetes features are available?"

## Configuration

Environment variables to customize behavior:

| Variable | Default | Description |
|----------|---------|-------------|
| `OLLAMA_URL` | `http://localhost:11434` | Ollama API URL |
| `RAG_OLLAMA_MODEL` | `gemma3:4b` | Ollama model for generation |
| `RAG_LLM_MODEL` | `gpt-4o-mini` | OpenAI model for generation |
| `RAG_ANTHROPIC_MODEL` | `claude-sonnet-4-20250514` | Anthropic model for generation |
| `OPENAI_API_KEY` | — | OpenAI API key |
| `ANTHROPIC_API_KEY` | — | Anthropic API key |

## Running Tests

The test suite validates the retrieval pipeline without an LLM:

```bash
bash examples/rag-chatbot/test.sh
```

Tests 7 scenarios with 12 assertions covering:
- Text search returns relevant results
- Query responses include `query_id` and stats
- Top results have positive relevance scores
- Ranking features and explanations are present
- Relevance vs freshness profiles both return results
- Grouped result format works

All tests pass in ~10 seconds (including server startup and indexing).

## Knowledge Base

The demo loads 50 articles across 15 categories about a fictional cloud platform:

| Category | Articles | Topics |
|----------|----------|--------|
| Getting Started | 3 | Quick start, authentication, SDKs |
| API Reference | 5 | CRUD resources, webhooks, rate limits |
| Compute | 3 | Instance types, auto-scaling, containers |
| Storage | 3 | Object storage, block storage, databases |
| Networking | 3 | VPCs, load balancers, CDN |
| Security | 3 | IAM, encryption, compliance |
| Billing | 3 | Pricing, cost management, invoicing |
| Troubleshooting | 5 | Timeouts, 502 errors, CPU, DB pools, SSL |
| Monitoring | 3 | Metrics, alerts, log management |
| Deployment | 3 | CI/CD, IaC, environments |
| Data & Analytics | 3 | Pipelines, data warehouse, ML platform |
| Support | 2 | Support plans, SLA |
| Migration | 2 | AWS migration, GCP migration |
| Edge Computing | 1 | Edge functions |
| Integrations | 1 | Third-party integrations |
| Kubernetes | 1 | Managed Kubernetes |
| Disaster Recovery | 2 | Backup, multi-region |

Articles are JSON objects with `id`, `category`, `title`, and `content` fields, produced to a `knowledge-base` Kafka topic.

## How Chronik Powers RAG

This demo uses Chronik features from the Query Orchestrator (Phase 9):

- **`POST /_query`** — Unified query endpoint with multi-mode support
- **Text search** — Tantivy BM25 full-text indexing (automatic via WalIndexer)
- **Ranking profiles** — `relevance` and `freshness` profiles for different use cases
- **Feature extraction** — Per-result `text_score`, `freshness`, `rrf_score`
- **Explanations** — Human-readable ranking explanations per result
- **Result formats** — `merged` (flat list) or `grouped` (by topic)
- **Query stats** — Candidate count, latency, and ranking metadata

The key env var is `CHRONIK_DEFAULT_SEARCHABLE=true`, which enables Tantivy text indexing for all topics. Without it, topics must be explicitly configured with `searchable=true`.

## Cleanup

The demo cleans up automatically on exit (Ctrl+C or typing `quit`):
- Stops the Chronik server
- Removes the temporary data directory (`/tmp/chronik-rag-demo`)
