# Chronik Ontology SDK

**The semantic layer for agentic systems — runtime-native, on your streams.**

Most "ontology / knowledge-graph for agents" tools (Zep/Graphiti, Cognee) build the
graph *off to the side* in a separate store, fed by an extraction pipeline. Redpanda's
Agentic Data Plane governs *access* to streams but carries no semantics. Chronik's
Ontology SDK is the piece neither has: a **meaning layer that lives inside the streaming
substrate** — rebuilt from the log, consumed at query time on the same Unified API,
point-in-time for free because the log is already temporal, with **no second database to
sync**.

The SDK has two halves:

- **Consumption is MCP-native and needs no SDK code.** A running broker exposes the
  domain as MCP tools at `POST /ontology/v1/mcp` (JSON-RPC 2.0) — any agent, in any
  language, in any framework, consumes it over standard MCP. Tools: `get_object`,
  `query_objects`, `traverse`, `neighbors`, `related`, `relations`, `explain`,
  `list_types`.
- **Authoring is this SDK.** You declare your domain (object types + link types) in one
  YAML/JSON file and apply it with the `chronik-server ontology` CLI, then feed it data.

---

## Quickstart

The authoring CLI is the standalone **`chronik`** binary (build it with
`cargo build --release -p chronik-cli`; it is separate from the `chronik-server`
broker binary):

```bash
# 1. Validate your schema offline (good for CI — no broker needed)
chronik ontology validate my-domain.ontology.yaml

# 2. Publish the object types + link types to the broker
chronik ontology apply my-domain.ontology.yaml --brokers localhost:9092

# 3. Feed it facts (JSONL of {subject,predicate,object,valid_from?,valid_to?})
chronik ontology ingest my-domain.facts.jsonl --namespace mydomain

# 4. Any agent now consumes the domain over MCP:
#    POST http://localhost:6092/ontology/v1/mcp   (tools/list, tools/call)
```

A worked example lives in [`examples/ontology/`](../examples/ontology/):
`issue-tracker.ontology.yaml` + `issue-tracker.facts.jsonl`.

---

## Schema reference

```yaml
schema_version: 1
namespace: issues              # topics are per-TENANT (the part before the first ':')

object_types:
  - name: Ticket               # the object type an agent asks for
    description: A unit of work.
    identity:
      field: subject           # which backing field is the object id (default: subject)
      normalize: true          # lowercase+trim the id when matching (default: true)
    backing:
      topic_prefix: mem.fact    # projection the attributes come from (default: mem.fact)
      append_only: false        # true = safe for offset/time as_of on this backing
    attributes:
      - name: status
        type: string            # string | number | bool | timestamp (default: string)
        from_predicate: status  # backing predicate supplying this attribute (default: name)
        multi: false            # true = the attribute holds a list (default: false)

link_types:
  - name: blocked_by            # the relation an agent names
    predicate: blocked_by       # underlying edge predicate in mem.fact (default: name)
    inverse: blocks             # name of the reverse reading (optional)
    description: Ticket A is blocked by ticket B.
```

**Rules the validator enforces:** non-empty namespace with no `.`; unique object-type
names; unique attribute names within a type; every relation name **and** inverse
globally unique (both are resolvable names); an inverse must differ from its name.

**What a fact looks like** (one JSON object per line for `ingest`):

```json
{"subject":"T2","predicate":"blocked_by","object":"T1","valid_from":"2026-01-01T00:00:00Z"}
```

A fact whose `object` is another entity id becomes a graph **edge** (traversable via
`related`/`neighbors`); a fact whose object is a scalar becomes an **attribute** value.
Bi-temporal `valid_from`/`valid_to` power `as_of` (point-in-time) reads — because facts
are keyed per `(subject, predicate, object)`, superseded values are retained, so history
survives compaction.

---

## What the agent gets

Once applied, an agent asks the domain semantic questions in one call — the things raw
search and access-governance layers cannot do:

| Tool | Answers |
|------|---------|
| `get_object{type,id,as_of?}` | an object's attributes with provenance; `as_of` gives its state at a past time |
| `related{node,relation,depth}` | traverse a **named** relation (multi-hop) — the whole transitive chain in one call |
| `neighbors{node,direction,edge_type}` | direct forward/reverse edges |
| `relations` / `list_types` | the domain's declared relations / object types |
| `explain{type,id}` | why an object holds its values, with its edges |

---

## Requirements

- **The `chronik` CLI** (authoring): `cargo build --release -p chronik-cli` → `chronik`.
  It talks to the broker only over the Kafka wire protocol, so it needs no server
  features.
- **The broker** (serving the domain to agents) must be built with the **`memory`**
  feature (`cargo build --release --features memory --bin chronik-server`) and run with
  **`CHRONIK_ONTOLOGY_ENABLED=true`** plus the memory registry env (`CHRONIK_MEMORY_KAFKA`,
  `CHRONIK_MEMORY_API`) and an embedding provider (`CHRONIK_EMBEDDING_*`) — v2.12 rejects
  the `vector.enabled` fact topic without one.

## Known limitation (v0)

The broker's ontology consumers pick up **upserts to a tenant they already track**
immediately, but a **brand-new tenant** created mid-session can lag until its topics enter
the consumer's subscription (a consumer-group rebalance-on-new-topic limitation, tracked
separately from the SDK). If a freshly-applied *new* namespace does not resolve, ensure
its topics exist before the broker's ontology consumers start. Existing tenants update
live.

## Roadmap (v0.1+)

- `actions:` in the schema (validated state transitions) once the action engine is
  broker-wired.
- A reference agentic vertical built on the SDK, and a benchmark vs. side-store
  temporal-graph memory (Zep/Graphiti) on multi-hop + point-in-time.
- Language client libraries (TypeScript) layered over the MCP endpoint.
