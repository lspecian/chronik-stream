# Search profiles and the archive tier

Status: proposal (2026-11-24). Independent of the OpenSearch adaptor: this is
about Chronik's own indexing and `/_search`. The adaptor (local branch
`feat/opensearch-adaptor-v2`, not merged) is where the gap was measured and
would benefit, but nothing here depends on it.

## 1. Why

Benchmarking the OpenSearch adaptor against OpenSearch 2.19.1 (single node,
same machine, 1M synthetic log documents, `tests/os_adaptor/bench/`) and then
plain Tantivy on the same corpus
(`crates/chronik-search/examples/plain_tantivy_bench.rs`) showed that the gap
is in how Chronik builds and queries its indexes, not in Tantivy. The same
causes apply to every searchable topic produced over Kafka:

| 1M documents | Plain Tantivy (in-process) | OpenSearch (HTTP) | Chronik (HTTP) |
|---|---|---|---|
| Index | 2.6 s | 5.0 s | 2.7 s |
| New document searchable | 13 ms | 1.0 s | 22.8 s |
| Index on disk | 125 MB | 392 MB | 940 MB (241 index + 699 raw copy) |
| Text match p50 / p99 | 0.07 / 0.13 ms | 0 / 2 ms | 3 / 13 ms |
| Term filter p50 / p99 | 0.27 / 0.52 ms | 2 / 4 ms | 3 / 14 ms |
| Range + terms aggregation | 2.4 / 4.5 ms | works | no results |
| Date histogram (1m, 1M docs) | 8.5 / 11 ms | works | no results |

All three return identical totals on the comparable queries.

Causes, from the code:

1. **Generic schema.** Ingested records are indexed as text plus a JSON field
   (`_value`, `_json`). No typed fast fields, so ranges and aggregations over
   ingested data return nothing, keyword filters go through the JSON field,
   and responses are assembled from a record envelope.
2. **Archive count grows with data.** One archive per sealed WAL segment
   (250 MB rotation ≈ 185K small records → ~46 MB archive). Merging only
   combines archives under half of a 64 MB target, so normal archives are never
   merged: 6 archives at 1M documents per partition, ~600 at 100M. Every
   all-time search visits each one.
3. **One hot writer on the minimum heap.** The hot index uses Tantivy's
   minimum writer heap (15 MB), one writer per partition (the benchmark wrote
   to one partition): ~44K docs/s, so a 1M burst takes ~23 s to become searchable.
4. **Newest-first on every query.** Ranking is score, then timestamp; with no
   index sort, `match_all` scans hundreds of thousands of documents to find the
   newest ten.
5. **An uncompressed second copy.** The raw tier (`segments/segments`) keeps
   batches as produced; producers that send uncompressed batches (the adaptor
   in the benchmark, many Kafka clients by default) are stored uncompressed.

And one correctness issue found while reading:

6. **A search coverage gap.** The hot index trims to the newest 100K offsets
   per partition regardless of whether older offsets are archived; archives
   trail the log head by up to ~60 s (30 s seal + 30 s indexer interval).
   Under sustained ingest above ~100K docs/min/partition, records between the
   hot window and the newest archive are not searchable until archived.

## 2. Goals, non-goals, targets

Goals: typed and timeseries indexing as a per-topic choice; query cost roughly
flat as data grows; sub-second freshness under bursts; no unsearchable window;
smaller disk footprint. For every searchable topic, adaptor or not.

Non-goals: OpenSearch Dashboards; dynamic per-field mapping of unknown fields
(Tantivy schemas are fixed per index; unknown fields stay in the catch-all).

Targets (1M and 10M documents, single node, bench harness):

| Metric | Target |
|---|---|
| Query p99 (text, term, bool) | ≤ 2× plain Tantivy, ≤ OpenSearch |
| Range + aggregations | correct (equal to OpenSearch), p99 ≤ 2× plain Tantivy |
| Freshness after a 1M burst | ≤ 1 s |
| Unsearchable window | none (invariant, checked) |
| Disk | ≤ OpenSearch |
| Archives per partition | roughly flat in data size (≤ ~20 at 100M) |

## 3. Search profiles (per-topic configuration)

OpenSearch makes this configuration (mappings, index settings such as
`index.sort.field`, index templates, data streams). Chronik topics already carry
per-topic configs (`searchable`, `columnar.*`, `vector.*`); search gets the same.

| Config | Values | Default |
|---|---|---|
| `search.profile` | `generic`, `typed`, `timeseries` | `generic` |
| `search.fields` | reuses `columnar.columns` syntax: `name:type:/json/pointer;...` | inherit `columnar.columns` |
| `search.time_field` | a declared timestamp field | `timeseries`: required |
| `search.sort` | `field:desc` | `timeseries`: time field desc |
| `search.bucket` | `hour`, `day` | `timeseries`: `day` |
| `search.hot.writer_threads` / `search.hot.heap` | integers | derived from profile |

- `generic`: today's schema (text + JSON). Unknown payloads keep working.
- `typed`: declared fields become typed Tantivy fields (keyword → `STRING|FAST`,
  numbers → `INDEXED|FAST`, timestamp → date `INDEXED|FAST`, text → `TEXT`) plus
  the catch-all text and a stored `_source` (the original value, so responses
  return the document).
- `timeseries`: `typed` plus index sort by the time field and time-bounded
  archives (§4.2).

One declaration feeds both engines: a field declared once is a typed SQL column
(`/_sql`, projected columns #73) and a typed search field. If the adaptor ships, it maps an
OpenSearch mapping / index template onto these configs; nothing else here
needs it.

Changing a profile or fields: new archives use the new schema; existing ones
keep theirs (search already handles mixed layouts per archive). A reindex
operation (§6) rewrites history when wanted.

## 4. Archive tier and merging

### 4.1 Phase A — size-tiered merging and compression (3–5 days)

- Replace "merge only small archives" with a size-tiered policy: merge runs of
  `min_run` archives of similar size (within a factor) into the next tier, up
  to `max_archive_bytes` (default 2 GB, configurable). Steady state is
  logarithmic in data size per partition.
- Keep the existing merge machinery unchanged: atomic catalogue swap only if
  every source is still registered, orphan sweep, grace period, crash tests
  (`archive_merge_crash.sh`).
- Merge only archives of one schema (as today) and, for `timeseries`, of one
  time bucket.
- Compress raw-tier segments that arrive uncompressed (zstd) when they are
  written to tier 2.
- `segment_compaction.rs`: deleted (never wired; its retention deleted search
  indexes only, leaving raw and Parquet copies). Topic `retention.ms` /
  `retention.bytes` are now enforced on the archive (`retention.rs`).

### 4.2 Phase B — profiles, typed archives, time buckets (1–2 weeks)

**As built (2026-11-24):** instead of declared typed fields per topic, the JSON
field became a fast (columnar) field in archives and the hot index, so ranges
and aggregations work on every JSON field with no configuration — like an
OpenSearch dynamic mapping, which also keeps keyword doc values for strings.
Cost on the 1M-record benchmark: disk 301 → 354 MB (still under OpenSearch's
392 MB). Aggregations run on Tantivy's distributed collector (terms, histogram,
date_histogram as a millisecond histogram, range, stats, avg/sum/min/max,
cardinality, percentiles), partials merged per node and across the cluster;
each record counted once (hot only above the archived watermark, each node
only the partitions it leads). Exact totals for `size: 0`, aggregations and
`track_total_hits: true`. Declared typed / timeseries profiles stay possible
for index sorting and time pruning if measurements call for them.

Also found and fixed: query field names were tried with a `_` prepended, so a
JSON field named like an archive's internal column (`ts` -> `_ts`) silently
resolved to it.

Original plan:

- Indexer builds archives with the topic's profile schema; hot index too.
- `timeseries`: archives cut at bucket boundaries as well as at WAL seals;
  index sorted newest-first so top-N by time is a prefix read.
- Range pruning: skip archives whose catalogue `[min_timestamp, max_timestamp]`
  misses the query's time range (already recorded per archive), and per-field
  min/max ("zone maps") recorded at archive build for other range fields.

### 4.3 Phase C — a live merged index per partition (2–4 weeks, only if needed)

One continuously merged local Tantivy index per partition for recent data,
Lucene-style; archives become the tiering/S3 format. Removes the hot/archive
split and its 100K cap. Touches the indexer, columnar/vector catch-up and
legacy retirement (keyed by archive offset ranges), DR and the search cache:
decide after measuring A and B.

## 5. Freshness and the hot index

- **Coverage-bounded eviction (correctness, Phase A).** The hot index may evict
  only offsets already covered by an archive (`evict_up_to(archived
  watermark)` already exists); the count cap becomes a memory budget that,
  when exceeded, applies backpressure or extends the window instead of
  dropping unarchived offsets. Invariant: every offset below the partition's
  high watermark is searchable in exactly one of {hot, archive}.
- **Writer capacity.** Writer threads and heap per profile (`timeseries`: e.g.
  4 threads, 256 MB). (Adaptor-only, if it ships: route `_bulk` by document id
  across partitions so they index in parallel.)
- **Commit cadence.** Keep the 100 ms visibility flip; measure commit cost at
  the larger heap.

## 6. More performance work

- **Per-archive result caching.** Archives are immutable, so an aggregation's
  partial result per archive can be cached (keyed by archive id + query hash)
  and only the hot index and newest archive recomputed. Dashboards refresh the
  same queries every few seconds; this makes repeated aggregations near-free.
  Natural fit for the immutable design; OpenSearch's request cache is coarser.
- **Searcher reuse.** Keep archive searchers open across requests (the search
  cache already holds unpacked archives; verify searchers are not rebuilt per
  request) and avoid per-request JSON envelope decoding once `_source` is the
  stored document.
- **Exact totals cheaply.** Count with Tantivy's `Count` collector alongside
  `TopDocs`, deduplicated across replicas by counting leader partitions only
  (the fan-out already knows partition leaders), instead of "hits collected".
- **Index once, not RF times (later).** Every replica builds its own archives
  (RF3 = 3× indexing CPU and disk). Leaders could build and upload; followers
  fetch archives from the object store, as Kafka tiered storage does. Large
  change; trades follower CPU for object-store reads.

## 7. Reliability

- **Coverage checker.** Background check and metric (`search_unsearchable_offsets`)
  comparing the high watermark with hot + archive coverage per partition; e2e
  test under sustained ingest above the hot cap.
- **Archive verification.** Verify the tar and Tantivy checksums when an
  archive is opened from the object store; periodic background scrub of local
  archives; quarantine and rebuild from the log on mismatch.
- **Indexer lag SLO.** `wal_indexer_lag_seconds` exists; add hot-index lag and
  an alert threshold; document what happens when exceeded (backpressure, not
  drops).
- **Reindex.** Admin operation to rebuild a topic's archives from the log (after
  a profile change, or to repair), throttled, with progress, using the same
  catalogue swap as merges.
- **Schema versioning.** Each archive records its schema/profile version; search
  per-archive layout detection (already present for legacy layouts) extends to it.
- **Perf regression guard.** Run the bench harness at reduced scale in CI (or
  nightly) and fail on regressions beyond a threshold; keep the plain-Tantivy
  benchmark as the reference ceiling.

## 8. Phasing

| Phase | Scope | Estimate | Exit |
|---|---|---|---|
| A | tiered merging, compression, coverage-bounded eviction, coverage checker | 3–5 days | archives logarithmic; no unsearchable window under sustained ingest; disk ≤ OpenSearch |
| B | profiles + typed/timeseries archives, `_source`, exact totals, range pruning, hot writer capacity | 1–2 weeks | ranges/aggregations over typed topics correct; query and freshness targets |
| — | per-archive result cache, archive verification, reindex | 1 week | dashboards' repeated aggregations near-free; repair path |
| C | live merged index per partition | 2–4 weeks | only if A + B miss targets |

## 9. Risks and open questions

- Merging larger archives costs I/O and CPU in the background: throttle (the
  indexer already runs at nice 10), and measure produce latency during merges.
- Time-bucketed archives multiply archive count for low-volume topics: buckets
  only for `timeseries`, and merging across adjacent buckets when tiny.
- Typed schemas reject documents with wrong-typed values: index the value in
  the catch-all and count rejections (metric) rather than drop the record.
- Leader-only counting for totals depends on fan-out leader knowledge being
  current during failover; fall back to "gte" during leadership changes.
- Rolling upgrade: old binaries must keep reading new archives during a
  rollback window; typed archives are a new layout, so gate profile use on all
  nodes supporting it (as group offsets did).

## 10. Measurement on main

The comparison harness (`tests/os_adaptor/bench/`) lives on the adaptor branch.
For this work, a harness on main: the same deterministic corpus produced over
Kafka to a searchable topic, Chronik's native `/_search` for queries (k6),
marker-document freshness, disk and RSS, at 1M and 10M records, single node and
the local 3-node cluster. `crates/chronik-search/examples/plain_tantivy_bench.rs`
stays the reference ceiling (it reads the corpus as NDJSON; the generator moves
to main with the harness).

`tests/search_bench/run.sh` (Chronik) and `tests/search_bench/opensearch.sh`
(OpenSearch 2.19.1 in Docker, explicit mapping, 4 GB heap) run the same corpus,
the same measurements and the same k6 query mix (`queries.js`; `AGGS=1` adds the
aggregations, `ONLY=<query>` runs one query type for profiling).

## 11. Status (2026-11-24)

Built, beyond Phase A and B: per-archive result cache (§6), archive
verification with automatic rebuild and a scrub (§7), reindex (§7), coverage
checker (§7), score-gated ranking, newest-first match_all walk, mimalloc.
Not built: declared typed / timeseries profiles (the fast JSON field made them
unnecessary for ranges and aggregations), Phase C, index-once-not-RF-times,
the CI perf guard.

1M documents, one node, same machine, same day (8 query clients, 60 s):

| | Chronik | OpenSearch |
|---|---|---|
| Ingest (Kafka produce vs `_bulk`) | 1.9 s, 11 CPU s | 5.1 s, 62 CPU s |
| All documents searchable, from the first sent | 4.1 s | 5.9 s |
| Searchable after acknowledgement (marker) | 1.9–2.7 s | 0.8 s |
| Memory after the run | 0.6 GB | 5.4 GB |
| Disk | 394 MB | 391 MB |
| Query rounds (6 queries each) | 62.0K | 60.3K |
| match_text p50 / p99 | 1.00 / 2.65 ms | 0.73 / 2.33 ms |
| term p50 / p99 | 1.69 / 3.78 ms | 2.80 / 5.60 ms |
| bool text + filter p50 / p99 | 1.29 / 2.98 ms | 1.44 / 3.69 ms |
| match_all p50 / p99 | 0.83 / 2.91 ms | 0.63 / 2.10 ms |
| range + terms agg p50 / p99 | 0.30 / 1.27 ms | 0.40 / 2.02 ms |
| date_histogram p50 / p99 | 1.12 / 5.58 ms | 1.03 / 3.01 ms |

Freshness after acknowledgement is bounded by hot indexing on one partition
(8 writer threads, half the cores, stemming analyzer): Chronik acknowledges
writes before indexing them, OpenSearch after.

10M documents, one partition, same harness (`AGGS=1`). Chronik with the
archive fixes and default settings (archives unmerged at query time, 48 of
them):

| | Chronik | OpenSearch |
|---|---|---|
| Ingest | **21.3 s**, 260 CPU s | 33.6 s, 300 CPU s |
| Marker searchable after it was sent | 22.4 s | **2.5 s** |
| Every record searchable, after the marker | 125 s (181 s before the archive fixes) | **immediately** |
| Memory after the run | **2.9 GB** | 7.7 GB |
| Disk | 3.5 GB | **2.7 GB** |
| Query rounds (60 s) | 7.3K | **14.2K** |
| match_text p50 / p99 | 10.3 / 21.1 ms | **0.95 / 2.3 ms** |
| term p50 / p99 | 17.3 / 28.6 ms | 17.6 / 26.5 ms |
| bool text + filter p50 / p99 | 12.3 / 23.0 ms | **4.6 / 6.9 ms** |
| match_all p50 / p99 | 1.6 / 6.3 ms | **0.7 / 1.6 ms** |
| range + terms agg p50 / p99 | 0.56 / 3.2 ms | 0.53 / 1.7 ms |
| date_histogram p50 / p99 | 20.6 / 31.1 ms | **7.1 / 11.6 ms** |

Open at 10M:

- **Burst coverage.** A partition taking records faster than it archives
  them (~80K/s per partition: 1.9 s of Tantivy per 210K-record segment,
  segments one after another) exceeds the hot index's hard cap
  (`CHRONIK_HOT_TEXT_HARD_MAX_DOCS`, 1M) and the excess is unsearchable until
  archived; `chronik_search_unsearchable_offsets` reports it. Options: archive
  segments of one partition concurrently, size the cap by memory, or slow
  produce acknowledgements while a partition is over it.
- **Query latency.** Not explained by archive count: with the archives merged
  to 5, match_text p50 was 15.7 ms (worse). Needs a profile at this size.

### Burst coverage (2026-11-24, later)

Fixed in two layers, measured on the 10M one-partition burst:

| | Archived one segment at a time | Pipelined + indexer wake (default) | + `searchable.throttle.enabled` |
|---|---|---|---|
| Ingest | 21 s | 21–31 s | 95 s |
| Every record searchable, after the marker | 181 s | 54–57 s | 2 s |
| Answers right after the burst | short | short | exact |
| Acknowledged records ever unsearchable | up to ~5M | up to ~5M, for under a minute | none (coverage checks: 0) |

- Archiving indexed every record twice and built one segment at a time;
  segments are now prepared ahead in parallel and published in order
  (truncation epoch guards prepared work). ~230K records/s per partition.
- A partition whose hot index is half full of unarchived records wakes the
  indexer and has its active segment sealed.
- Throttle (opt-in, per topic): produce waits before writing until the
  partition's pending records (held, queued, admitted) plus the batch fit in
  80% of the hot cap; admission is reserved atomically, since a pipelining
  client sends many requests at once.
- The hot index forgot its archived watermark on any late record, which
  under load left archived records untrimmable until the hard cap evicted
  them as unarchived; topic deletion and truncation now reset it instead.

Tried and not kept: the hot index on disk (no-fsync Tantivy directory, cap
10M) closed the gap without throttling (0 coverage warnings, every record
searchable ~1 s after the marker) but the index reached 12.5 GB (~1.25 KB a
record: value, headers and JSON field are all indexed and stored), kept
growing after archiving caught up (trimming only marks deletions; merging
10M docs does not keep up), memory doubled and queries ran 6x slower. It
needs reclaim once the archive covers a partition (rebuild rather than
merge) and faster hot ingestion (it lagged 25 s) before it can be a default.

### Query latency at 10M (2026-11-25, branch perf/search-query-latency)

Same corpus, machine and day; 10M records, one partition, 48 archives
(unmerged), 8 query clients for 60 s, `AGGS=1`:

| | Chronik before | Chronik after | OpenSearch 2.19.1 |
|---|---|---|---|
| Query rounds | 7.3K | **18.4K** | 14.3K |
| match_text p50 | 9.9 ms | 4.2 ms | **0.96 ms** |
| term p50 | 17.0 ms | **6.9 ms** | 18.0 ms |
| bool text + filter p50 | 12.0 ms | 7.1 ms | **4.6 ms** |
| match_all p50 | 1.5 ms | 1.25 ms | **0.65 ms** |
| range + terms agg p50 | 0.54 ms | **0.47 ms** | 0.53 ms |
| date_histogram p50 | 21.6 ms | **4.4 ms** | 6.7 ms |
| Memory | 3.1 GB | **3.1 GB** | 7.2 GB |

What changed: archives are ranked on their rank columns and only the page's
documents are read; a floor (the hot index's page, then the best so far)
lets later archives pass over documents that cannot place; archived rank
columns are opened once; aggregation partials merge pairwise, the merged
share of an unchanged archive set is cached, and final aggregations are
written straight from Tantivy's result. Answers are identical.

Still behind on a single text term: OpenSearch scores it with block-max
WAND and visits a few blocks. Tantivy's block-max pruning cannot be used on
Chronik's JSON field: Tantivy keeps no fieldnorms for JSON fields and writes
zero block maxima, so `for_each_pruning` skips nearly every match (8 of 5,000
in a minimal index). Using it needs block maxima written for unnormed fields
(a Tantivy change) and archives rebuilt (`POST /admin/reindex`).

### Scoring with the topic's statistics, and Block-WAND (2026-11-25)

Each Tantivy index used to score with its own BM25 statistics, so the same
message scored differently in every archive and "newest first among equal
scores" never held across indexes: on 10M records, `timeout` ranked the newest
20,136 matches (6.43448) below a million-offset-older archive's (6.44069), and
`level: ERROR` answered with records 3.5M offsets old. Every index a search
reads now scores with the summed statistics of all of them (`corpus_stats.rs`;
Elasticsearch's `dfs_query_then_fetch`), per-segment values cached for good
(segments never change). Ranked searches prune with Block-WAND again, on the
Tantivy fork (docs/TANTIVY_FORK.md), and a ranked search with no floor yet
runs its newest archive alone first so the others start with one.

10M records, 48 archives (merging off), 8 VUs, 60 s, p50:

| | before | Block-WAND | + topic statistics |
|---|---|---|---|
| rounds | 18,377 | 20,012 | 24,721 |
| match_text | 4.24 ms | 3.48 ms | 3.17 ms |
| term | 6.89 ms | 5.75 ms | 3.20 ms |
| bool | 7.13 ms | 6.84 ms | 6.34 ms |
| match_all | 1.25 ms | 1.21 ms | 0.68 ms |

OpenSearch on the same corpus and day: 14,250 rounds; match_text 0.96 ms,
bool 4.56 ms, match_all 0.65 ms. What remains is per-archive overhead: each of
48 archives builds a scorer and looks up its terms per query.
Test: `tests/cluster/e2e/search_scoring_across_archives.sh` (fails on the
previous binary: the oldest copies ranked first).

### Archives highest offset first (2026-11-25)

Merging the 48 archives of the 10M benchmark into 3 (the production default)
made searches slower, not faster: match_all 0.68 -> 27 ms, term 3.2 -> 12 ms.
Equal scores rank newest first, but archives held records oldest first, and
Tantivy reads matches in document order, so every later tie beat the ones
held and a 3.3M-record archive was read whole. These 10M records span about
30 seconds (330K records a second), so even sorting by timestamp left each
second oldest first.

Archives are now written with Tantivy index sorting on `_offset` descending
(`tantivy_segment::archive_settings`); merges keep it, and runs of the two
orders merge separately. A document later in such a segment has a lower
offset than every earlier one, so it beats an equal-scoring one only with a
newer second; per block of 1,024 documents, the newest second from there to
the end bounds that. Once nothing later can place, ties stop being read (the
collector's threshold becomes strict, so Block-WAND skips them) and match_all
stops walking. The merged archives here have 18 late-stamped records in 3.7M
(concurrent producers); they cost only their blocks. Older archives keep
working as before; a plain `POST /admin/reindex` rewrites them.

10M records, 3 merged archives, 8 VUs, 60 s, p50 (answers identical):

| | merged, oldest first | merged, highest offset first | OpenSearch |
|---|---|---|---|
| rounds | 6,482 | 36,777 | 14,250 |
| match_text | 5.31 ms | 0.44 ms | 0.96 ms |
| term | 10.13 ms | 0.42 ms | 17.96 ms |
| bool | 12.90 ms | 7.43 ms | 4.56 ms |
| match_all | 35.67 ms | 0.26 ms | 0.65 ms |
| range + terms agg | 0.34 ms | 0.17 ms | 0.53 ms |
| date_histogram | 4.06 ms | 3.33 ms | 6.70 ms |

Still behind: bool (a text match AND a filter term): the intersection seeks
through postings with a single-level skip list (quickwit-oss/tantivy#3287).
