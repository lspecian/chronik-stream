# chronik-ontology

The event-native **Ontology** layer for Chronik — user-declared **Object Types**
materialized on demand from Chronik's existing projections, with **identity**,
**provenance**, and **`as_of`** (point-in-time). A layer *on* Chronik (this crate
+ a Unified-API mount under `/ontology/v1/*`), never domain semantics inside
`chronik-server`. See [`docs/ROADMAP_ONTOLOGY.md`](../../docs/ROADMAP_ONTOLOGY.md).

Off by default — enable the server with `CHRONIK_ONTOLOGY_ENABLED=true` (rides the
`memory` cargo feature).

## What's implemented (feat/ontology-o0)

| Phase | Surface | Status |
|-------|---------|--------|
| **O-0** Object Types | `ont.types.{tenant}` registry; `get_object` (attributes + provenance + `as_of`) | ✅ done, E2E-verified |
| **O-1** Link Types | `traverse` (1..=3 hop, provenance, bi-temporal) | ✅ core done |
| **O-2** read tools | `query_objects` (list instances) | ✅; MCP wrapper + `explain` TODO |
| **O-3/O-4** | Actions/CAS, platformization | not started |

## The dogfood model (memory domain)

An **ObjectType** binds to an existing `mem.*` projection and declares which field
is its identity and how predicates map to attributes:

```json
{"type_name":"Entity",
 "attributes":[{"name":"degree","type":"string","from_predicate":"has_degree"},
               {"name":"hobbies","type":"string","from_predicate":"enjoys","multi":true}],
 "identity":{"id_field":"subject","normalize":true},
 "backing":{"topic_prefix":"mem.fact","append_only":false}}
```

Register it by producing to the compacted topic `ont.types.{tenant}` (key = the
type name). An **edge** is simply a fact whose object is another entity —
`(subject) --[predicate]--> (object)` — so traversal needs no separate edge store.

## API (`/ontology/v1/*`, port 6092)

| Endpoint | Body / query | Returns |
|----------|--------------|---------|
| `GET  /types` | `?namespace=` | ObjectTypes registered for the namespace's tenant |
| `POST /get_object` | `{namespace, type, id, as_of?}` | one instance: attributes, each with source-offset provenance |
| `POST /query_objects` | `{namespace, type, as_of?}` | every instance of the type in the namespace |
| `POST /traverse` | `{namespace, from, edge_type, depth (1..=3), as_of?}` | edges `(from)--[edge_type]-->(to)`, each provenance-carrying |

`edge_type: "*"` follows all outgoing edges. `as_of` (RFC3339) is bi-temporal:
a record is effective iff `valid_from <= as_of < valid_to`.

## Design notes / limits (roadmap §9)

- Instances are resolved over `/_search` (not `/_sql` — `SELECT _value` fails on
  the DataFusion unified view). Provenance = each fact's `source.{topic,offsets}`.
- `mem.fact` is keyed by **tenant** (first `:`-segment of a namespace); one topic
  holds several namespaces, filtered by the record's `namespace` field.
- `as_of` is exact only over **append-only** backing; over a compacted backing
  (`mem.fact`), superseded history may be gone (best-effort). Cross-projection
  consistent-snapshot token is deferred.
- Identity is deterministic (normalized id); fuzzy entity resolution deferred.

## Testing

- Unit (pure, no broker): `cargo test -p chronik-ontology` (27 tests — decode/apply,
  resolution, traversal, bi-temporal `as_of`).
- End-to-end exit gate (needs a live ontology-enabled broker + kcat + jq):
  `bash tests/integration/ontology_o0_e2e.sh` (9/9 — registry, get_object,
  provenance-cite gate, `as_of`, 1/2-hop traverse, query_objects).
