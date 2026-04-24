# Search Field Model Investigation (v2.5.4 follow-up)

## Problem

Against a Kafka-produced topic, `{"match": {"value": "foo"}}` returns 0 when the
produced value is `{"note":"has foo content"}` — even though the substring
"foo" is clearly in the Kafka record value. Reproduces with
`CHRONIK_HOT_TEXT_ENABLED=false`. Issue #2's `match.key=B` clause also
silently has no effect — symptom of the same root cause.

## Tantivy schemas found in the repo

| # | File | Fields | Underscore? | Live? |
|---|------|--------|-------------|-------|
| A | `chronik-search/src/realtime_indexer.rs:473` | `_id`, `_topic`, `_partition`, `_offset`, `_timestamp`, `_json_content` (JSON), `_key` (STRING), `_value` (TEXT) | Yes | **Yes — disk cold path for `/_search`** |
| B | `chronik-search/src/hot_text_index.rs:104` | `topic`, `partition`, `offset`, `timestamp`, `key` (TEXT), `value` (TEXT), `headers` (TEXT) | No | **Yes — hot NRT path** |
| C | `chronik-search/src/indexer.rs:63` (`TantivyIndexer`) | `topic`, `partition`, `offset`, `timestamp`, `key` (TEXT), `value` (TEXT), `headers` (TEXT) | No | **No** — `chronik-search/src/integration.rs` only; README + tests |
| D | `chronik-storage/src/tantivy_segment.rs:45` (`SchemaFields`) | `_offset`, `_ts`, `_key` (BYTES), `_value` (BYTES), `_headers` (TEXT), `_attributes`, `_base_offset`, producer ids… | Yes | **Yes for archival; NO for text search** — WAL indexer emits tar.gz with this schema to object store. `_key`/`_value` are BYTES, not tokenized. `search_wal_indices_for_topic` tries `Index::open_in_dir` on `{data}/tantivy_indexes/{topic}[-N]/`, but nothing unpacks the tar.gz into a live dir — the lookup effectively returns empty. |
| E | `chronik-storage/src/search.rs:346` (`KafkaMessageIndex`) | `_offset`, `_timestamp`, `_key` (STRING), dynamic per-JSON-field columns | Yes | **No external callers** |
| F | `chronik-storage/src/index.rs:66` (`IndexBuilder`) | `_offset`, `_timestamp`, `_content` (TEXT) | Yes | Exported but no server/search callers |

## Live ingest paths

| Writer | Calls | Target schema | Status |
|--------|-------|---------------|--------|
| `produce_handler::send_to_hot_text_index` ([produce_handler.rs:3032](crates/chronik-server/src/produce_handler.rs#L3032)) | `HotTextIndex::add_batch` | **B** | Correct — uses `record.key` / `record.value` directly |
| `produce_handler::send_to_indexer` ([produce_handler.rs:3066](crates/chronik-server/src/produce_handler.rs#L3066)) | mpsc → `RealtimeIndexer::start` → `index_json_document` | **A** | **Broken** — see below |
| `builder.rs:770+` hot warm-up | `HotTextIndex::add_batch` from WAL | **B** | Correct |
| `WalIndexer::create_tantivy_index` | `TantivySegmentWriter::write_batch` | **D** | Bytes-only, archival — not text-searchable |

## Dead paths (do not touch in this fix)

- `JsonPipeline` ([json_pipeline.rs:153](crates/chronik-search/src/json_pipeline.rs#L153)) — constructed in `produce_handler` but never invoked. Separate cleanup.
- `TantivyIndexer` / `SearchIntegration` — README example only.
- `chronik-storage/src/search.rs` (`KafkaMessageIndex`) — no external callers.
- `chronik-storage/src/index.rs` (`IndexBuilder`) — exported but no server callers.

## Live search path (`POST /:topic/_search`, [handlers.rs:361](crates/chronik-search/src/handlers.rs#L361))

1. Query in-memory REST indexes (`api.indices`) — for indexes created via REST.
2. Query `{data}/tantivy_indexes/` — Schema D tar.gz, effectively empty for text.
3. Query `{data}/index/` — **Schema A, realtime disk**. This is the only live cold source for Kafka-produced topics.
4. Query hot text index (Schema B) when enabled.
5. Merge hot + cold, dedup by `(_index, _id)`.

`build_tantivy_query` → `resolve_field` already aliases `value ↔ _value`, `key ↔ _key` ([handlers.rs:790](crates/chronik-search/src/handlers.rs#L790)), so cross-schema queries work at the query layer.

## Root cause

The data model between Kafka record and Schema A is broken at
[produce_handler.rs:3086-3136](crates/chronik-server/src/produce_handler.rs#L3086-L3136):

```rust
// Branch 1: Kafka value parses as a JSON object.
if let serde_json::Value::Object(mut obj) = json_value {
    for (k, v) in &metadata { obj.insert(k.clone(), v.clone()); }
    JsonDocument { content: serde_json::Value::Object(obj), ... }
}
// Branch 2: Kafka value is non-object JSON (array/scalar).
//   inserts content["_value"] = json_value
// Branch 3: Kafka value is not JSON.
//   inserts content["_value"] = string-or-base64
```

**On all three branches, the Kafka record key is silently dropped.**
**On Branch 1 (the common case with JSON producers), the raw Kafka value is
silently dropped** — only its parsed fields survive as top-level entries in
`content`, which collides with any user JSON field also named `_value`.

Then [realtime_indexer.rs:555-566](crates/chronik-search/src/realtime_indexer.rs#L555-L566):

```rust
if let Some(key) = doc.content.get("key").and_then(|v| v.as_str()) {
    if let Ok(field) = schema.get_field("_key") { tantivy_doc.add_text(field, key); }
}
if let Some(value) = doc.content.get("value").and_then(|v| v.as_str()) {
    if let Ok(field) = schema.get_field("_value") { tantivy_doc.add_text(field, value); }
}
```

Reads `content["key"]` / `content["value"]` — looking for user JSON fields
literally named `"key"` / `"value"`. Category error: conflates "a JSON field
named `value`" with "the Kafka record value." For `{"note":"has foo content"}`
neither field exists, so Schema A's `_key` and `_value` stay empty and
`match.value=foo` matches nothing.

## Why the uncommitted `resolve_field` is necessary but not sufficient

`resolve_field` correctly lets `match.value` reach Schema A's `_value` field
and Schema B's `value` field from the same caller. It fixes the query-side
naming mismatch. But `_value` in Schema A is empty, so the query still
returns zero hits on the cold path for JSON-produced topics. The ingest-side
data loss has to be fixed too.

## Why Options 1 and 2 from the pre-investigation were both wrong

- **Option 1** (patch only `realtime_indexer.rs:555-566`) doesn't help
  because the raw Kafka value doesn't reach that function — it was dropped
  two layers upstream in `send_to_indexer`.
- **Option 2** (make `_value` queries fall back to `_json_content` in
  `build_tantivy_query`) papers over the data loss. It permanently conflates
  "Kafka value" with "parsed JSON content" at the query layer, makes scoring
  unpredictable, and leaves the `_key` branch still broken.

## Proper fix

Three edits, one logical change: **preserve the raw Kafka key and value
end-to-end from `ProduceRecord` through `JsonDocument` into Schema A's
Tantivy `_key` / `_value` fields.** No schema renames, no query-layer
fallback, no migrations.

### Edit 1 — `JsonDocument` carrier

File: `crates/chronik-search/src/realtime_indexer.rs` (around line 145)

Add two explicit fields:

```rust
pub struct JsonDocument {
    pub id: String,
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub timestamp: i64,
    pub content: JsonValue,
    pub metadata: Option<JsonMap<String, JsonValue>>,
    /// Raw Kafka record key as UTF-8, or None if key absent or non-UTF-8.
    pub raw_key: Option<String>,
    /// Raw Kafka record value as UTF-8 (lossy), or None only for empty values.
    pub raw_value: Option<String>,
}
```

Why not smuggle them through `content`? Because `content` is merged with the
parsed JSON body — injecting `_value` there would shadow any user-produced
field named `_value` and is indistinguishable from user data. Separate
fields keep the Kafka record identity out of the user-namespace.

### Edit 2 — Ingest

File: `crates/chronik-server/src/produce_handler.rs` (around line 3070)

In `send_to_indexer`, populate `raw_key` and `raw_value` from the
`ProduceRecord` unconditionally on all three branches, independent of any
JSON parsing of the value:

```rust
let raw_key = record.key.as_deref()
    .and_then(|k| std::str::from_utf8(k).ok().map(String::from));
let raw_value = Some(String::from_utf8_lossy(&record.value).into_owned());
// ... existing content-shaping logic unchanged ...
JsonDocument { /* existing */, raw_key, raw_value }
```

Do **not** remove the existing `content["_value"]` injections on branches 2/3
— leaves the document structure stable for any observer (e.g. anything that
serialises `_source`). The Tantivy `_value` field gets its data from
`raw_value` now; `content["_value"]` becomes redundant but harmless.

### Edit 3 — Indexer

File: `crates/chronik-search/src/realtime_indexer.rs` (lines 555-566)

Replace:

```rust
if let Some(key) = doc.content.get("key").and_then(|v| v.as_str()) { ... }
if let Some(value) = doc.content.get("value").and_then(|v| v.as_str()) { ... }
```

with:

```rust
if let (Ok(field), Some(k)) = (schema.get_field("_key"), doc.raw_key.as_deref()) {
    tantivy_doc.add_text(field, k);
}
if let (Ok(field), Some(v)) = (schema.get_field("_value"), doc.raw_value.as_deref()) {
    tantivy_doc.add_text(field, v);
}
```

`_json_content` indexing (line 547-553) stays — it's what enables queries
against individual parsed JSON fields via the JSON field path, and it's not
the bug.

### Edit 4 — the already-uncommitted `resolve_field` aliases

File: `crates/chronik-search/src/handlers.rs`

Keep exactly as the current uncommitted diff has it. Already correct.

## Additional fix surfaced by the test matrix

### Edit 5 — Schema B `key` TEXT → STRING

File: `crates/chronik-search/src/hot_text_index.rs` (around line 118)

The matrix's `match.key="A"` probe failed in the hot-on cell while passing
hot-off. Cause: Schema B registered `key` as TEXT, which runs through the
English analyzer pipeline (`SimpleTokenizer → LowerCaser → StopWordFilter →
Stemmer` — see `crates/chronik-storage/src/text_analysis.rs`). The stopword
filter drops `"a"`, so a produced key of `"A"` lowercases to `"a"` and is
filtered out entirely at index time. The doc lands with no searchable key
term; `match.key="A"` returns zero. Cold (Schema A) uses STRING and is
unaffected.

Issue #2's own queries (`match.key="B"`) happened to pass in every cell by
coincidence — both sides return 0 for different reasons — so the bug was
latent. Any user who produces with a single-letter key or a stopword-shaped
key (`"the"`, `"a"`, `"is"`…) would have had it silently disappear from hot
search hits.

Kafka keys are opaque identifiers, not prose. Schema B now matches Schema A:

```rust
let key_field = schema_builder.add_text_field("key", STRING | STORED);
```

No migration — hot indexes are in-memory only, rebuilt on restart.

## What the fix does NOT change

- **Schema D bytes `_key` / `_value`.** Archival only; no text search uses it.
- **Field naming across schemas.** `resolve_field` handles it at query time.
- **JsonPipeline, TantivyIndexer, KafkaMessageIndex, IndexBuilder.** Dead
  code. Separate cleanup.

## Matrix results (v2.5.5 candidate, 2026-04-24)

Produced via `kcat -k A -P` with value `{"note":"has foo content"}`, single
record, single partition, `CHRONIK_DEFAULT_SEARCHABLE=true` on every cell.
Each cell runs six queries; `A`/`B` are issue #2's exact bodies.

| Query | Expected | hot-on | hot-off | rest-only |
|-------|----------|--------|---------|-----------|
| A: `bool.must[match_all, match.key=B]` | 0 | 0 | 0 | 0 |
| B: `bool.must[match.value=foo, match.key=B]` | 0 | 0 | 0 | 0 |
| C: `match.value=foo` | 1 | 1 | 1 | 1 |
| D: `match.key=A` | 1 | 1 | 1 | 1 |
| E: `match._value=foo` (alias) | 1 | 1 | 1 | 1 |
| F: `match._key=A` (alias) | 1 | 1 | 1 | 1 |

Run: `tests/issue2_matrix.sh` — every cell green, ready to tag.

## Pre-tag test matrix (release_discipline)

Repro `{"note":"has foo content"}` produced via `kcat -k A`, single record,
single partition.

For each cell below, run:
- Query A: `bool.must = [match_all, {match: {key: "B"}}]` — expect **0 hits**
- Query B: `bool.must = [{match: {value: "foo"}}, {match: {key: "B"}}]` — expect **0 hits**
- Query C: `{match: {value: "foo"}}` — expect **1 hit**
- Query D: `{match: {key: "A"}}` — expect **1 hit**
- Query E: `{match: {_value: "foo"}}` — expect **1 hit** (prove alias is symmetric)
- Query F: `{match: {_key: "A"}}` — expect **1 hit**

| Cell | Hot | Cold (disk realtime) | Expected |
|------|-----|----------------------|----------|
| hot-on | Schema B | Schema A | all queries pass |
| hot-off (`CHRONIK_HOT_TEXT_ENABLED=false`) | — | Schema A | all queries pass |
| REST-only (create index via REST API, no Kafka) | — | in-memory REST index | all queries pass |

No tag until every cell passes. If any cell fails, fix forward, don't revert.
