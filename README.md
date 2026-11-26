# Chronik Stream

[![CI (pull requests)](https://github.com/lspecian/chronik-stream/actions/workflows/ci.yml/badge.svg?event=pull_request)](https://github.com/lspecian/chronik-stream/actions/workflows/ci.yml)
[![Release build](https://github.com/lspecian/chronik-stream/actions/workflows/release.yml/badge.svg)](https://github.com/lspecian/chronik-stream/actions/workflows/release.yml)
[![Release](https://img.shields.io/github/v/release/lspecian/chronik-stream)](https://github.com/lspecian/chronik-stream/releases)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/lspecian/chronik-stream/pkgs/container/chronik-stream)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](https://www.rust-lang.org)

**A Kafka-compatible event log in one Rust binary that builds SQL, full-text and vector indexes from the log itself.**

Produce with any Kafka client. Consume with any Kafka client. Query the same records with SQL (DataFusion over Arrow/Parquet), BM25 full-text search (Tantivy) or vector similarity (HNSW), without a sink connector in between.

> Chronik is a young project with one main maintainer. Read [Status and limitations](#status-and-limitations) before you rely on it.

---

## The Problem

Modern real-time architectures require stitching together many systems:

```
Producer -> Kafka -> Flink/Spark -> Data Warehouse -> BI Tools
                 |-> Kafka Connect -> Elasticsearch -> Search API
                 |-> Custom ETL -> Vector DB -> AI/Agent API
                 |-> S3 Sink -> Athena/Trino -> Ad-hoc Queries
```

Each arrow is a failure point, a latency penalty, and an operational burden. Data is copied, reformatted, and re-indexed across systems that each serve one purpose.

## The Chronik Approach

Chronik treats the event log as the source of truth and derives query capabilities from it, in the same process, right after the produce is acknowledged:

```
Producer -> Chronik
               |
               |-> WAL (durability, Kafka consumers)
               |
               |  (background, after the ack)
               |-> Arrow/Parquet (SQL via DataFusion)
               |-> Tantivy (full-text search, BM25)
               |-> HNSW (vector embeddings, semantic search)
```

There is no pipeline between the log and the query engines; they are the same system. The cost is freshness: indexes trail the log by about a second (search) to about a minute (typed SQL columns) in the default configuration. See [When data becomes queryable](#how-it-works-internally).

```
┌─────────────────────────────────────────────────────────────┐
│                        Chronik Stream                        │
│                                                              │
│  Kafka Protocol (9092)          Unified API (6092)           │
│  ├─ Produce / Fetch             ├─ /_sql    (DataFusion)     │
│  ├─ Consumer Groups             ├─ /_search (BM25/Tantivy)   │
│  ├─ Transactions                ├─ /_vector (HNSW)           │
│  └─ Admin APIs                  ├─ /_query  (Orchestrator)   │
│                                  └─ /admin   (Cluster ops)   │
│                                                              │
│  One binary. One log. Multiple query modalities.             │
└─────────────────────────────────────────────────────────────┘
```

For event data, Chronik covers the common case of Kafka plus a sink into ClickHouse, Elasticsearch or a vector database, in one binary. It is not a general-purpose OLAP database or a search cluster.

## Per-Topic Capability Model

Features are orthogonal and enabled per topic. A topic with no features enabled is a pure Kafka topic. Each capability adds a background indexing path; the produce path does not change.

```bash
# Pure Kafka — no extras, just streaming
kafka-topics.sh --bootstrap-server localhost:9092 --create --topic logs

# Capabilities are topic configs. Stock kafka-topics.sh refuses config names
# Kafka itself does not define ("Unknown topic config name"), so create the
# topic, then set them with kafka-configs.sh (any AdminClient can also pass
# them directly at create time; see the Quick Start).
alter() { kafka-configs.sh --bootstrap-server localhost:9092 --alter \
            --entity-type topics --entity-name "$1" --add-config "$2"; }

# SQL analytics — enables /_sql queries
kafka-topics.sh --bootstrap-server localhost:9092 --create --topic orders
alter orders columnar.enabled=true

# Full-text search — enables /_search queries
kafka-topics.sh --bootstrap-server localhost:9092 --create --topic documents
alter documents searchable=true

# Vector search — enables /_vector semantic queries
kafka-topics.sh --bootstrap-server localhost:9092 --create --topic support-tickets
alter support-tickets 'vector.enabled=true,vector.embedding.provider=openai'

# All capabilities on a single topic
kafka-topics.sh --bootstrap-server localhost:9092 --create --topic events
alter events 'columnar.enabled=true,searchable=true,vector.enabled=true'
```

A topic with nothing enabled has no indexing overhead.

### What Gets Enabled

| Capability | Config | Query Endpoint | Engine | Latency |
|------------|--------|----------------|--------|---------|
| **Streaming** | always on | Kafka Fetch (9092) | WAL | < 1ms |
| **SQL** | `columnar.enabled=true` | `POST /_sql` | DataFusion + Arrow/Parquet | 0-200ms |
| **Search** | `searchable=true` | `POST /_search`, `POST /{topic}/_search` | Tantivy (BM25) | 1-500ms |
| **Vector** | `vector.enabled=true` | `POST /_vector/{topic}/search` | HNSW + embeddings | 5-100ms |
| **Orchestrator** | automatic | `POST /_query` | RRF multi-backend fusion | 10-500ms |

All HTTP endpoints take JSON and require `Content-Type: application/json` (without it they answer `415`).

### How It Works Internally

All indexing happens asynchronously after the produce ack:

```
Producer -> WAL (group-commit fsync) -> Produce Ack
               |                        (acks=all in a cluster: after
               |                         min.insync replicas hold it)
               v  (background, async)
          WalIndexer
               |
    ┌──────────┼──────────┐
    v          v          v
 Tantivy    Parquet    Embeddings
 (search)   (SQL)      (vector)
```

The ack waits for the WAL group commit, whose batch window is set by `CHRONIK_WAL_PROFILE` (2 ms by default, up to 100 ms). Note that Kafka does not fsync per ack by default, so per-ack latency is not directly comparable.

When data becomes queryable (single node, default configuration):

- **Kafka Fetch**: immediately.
- **Full-text search**: about 1 second for small writes, served from the in-memory hot text index. After large bursts it takes longer: in the 10M-record benchmark below, a marker record sent after the burst was searchable after ~22 s, and every record after ~55 s.
- **SQL, envelope columns** (`_topic`, `_partition`, `_offset`, `_timestamp`, `_key`, `_value`, …): immediately.
- **SQL, typed columns inferred from JSON values** (`city`, `amount`, …): after the first Parquet flush, about 60 seconds. The WAL indexer runs on a fixed 30-second interval; there is no setting to shorten it today.
- **Vector search**: 30-90 seconds (includes the embedding API call).

## Architecture Comparison

### What Chronik Can Stand In For

| Capability | Traditional Stack | Chronik |
|------------|-------------------|---------|
| Event streaming | Apache Kafka | Built-in (Kafka wire protocol; see [compatibility](#kafka-compatibility)) |
| Stream processing | Flink / Spark Streaming | Not provided. Ad-hoc SQL over the log only: no windows, state or stream joins |
| Data warehouse | ClickHouse / BigQuery | Parquet files per topic, queried with DataFusion |
| Full-text search | Elasticsearch / OpenSearch | Tantivy (BM25) with an Elasticsearch-style query API |
| Vector search | Pinecone / Weaviate / Qdrant | HNSW with pluggable embeddings |
| ETL pipelines | Kafka Connect / Debezium / dbt | Not needed for indexing into the built-in engines |
| Object storage | S3 sink connector | Native S3/GCS/Azure tiered storage |

### What Chronik Does Not Replace

Chronik is not a general-purpose OLAP database, a graph database, a stream processor or a batch processing framework. It is purpose-built for event data: append-only logs where records arrive in time order and are queried by content, time range, or semantic similarity.

## Quick Start

### 1. Start a server

**Binary** (Linux x86_64 shown; also `linux-arm64` and `darwin-arm64`):

```bash
curl -L https://github.com/lspecian/chronik-stream/releases/latest/download/chronik-server-linux-amd64.tar.gz | tar xz
./chronik-server start
```

By default the server binds `0.0.0.0` and advertises the machine's hostname to Kafka clients. For access from other machines, pass `--advertise <host-or-ip>`. Data goes to `./data` (`--data-dir` to change).

**Docker:**

```bash
docker run -d --name chronik \
  -p 9092:9092 -p 6092:6092 \
  -v chronik-data:/data \
  -e CHRONIK_ADVERTISE=localhost \
  ghcr.io/lspecian/chronik-stream:latest start
```

`CHRONIK_ADVERTISE` (or `--advertise`) is required in Docker: without it, clients are sent the container's hostname and cannot connect. Image tags are the version **without** a `v`: `:2.20.3`, `:2.20`, `:latest`.

**From source** (Rust 1.94+):

```bash
git clone https://github.com/lspecian/chronik-stream.git
cd chronik-stream
cargo build --release --bin chronik-server
./target/release/chronik-server start
```

### 2. Create a topic, produce, consume

This uses [kafka-python](https://pypi.org/project/kafka-python/) (`pip install kafka-python`). The topic is created with SQL and search enabled; capabilities are ordinary topic configs passed by the AdminClient.

```python
import json
from kafka import KafkaAdminClient, KafkaProducer, KafkaConsumer
from kafka.admin import NewTopic

admin = KafkaAdminClient(bootstrap_servers="localhost:9092")
admin.create_topics([NewTopic("orders", num_partitions=1, replication_factor=1,
    topic_configs={"columnar.enabled": "true", "searchable": "true"})])

producer = KafkaProducer(bootstrap_servers="localhost:9092",
                         value_serializer=lambda v: json.dumps(v).encode())
for user_id, amount, city, note in [
    (1, 99.50, "Berlin", "late delivery, customer asked for refund"),
    (2, 12.00, "Lisbon", "gift wrap requested"),
    (3, 45.25, "Berlin", "refund issued after damaged package"),
]:
    producer.send("orders", {"user_id": user_id, "amount": amount, "city": city, "note": note})
producer.flush()

consumer = KafkaConsumer("orders", bootstrap_servers="localhost:9092",
                         auto_offset_reset="earliest", consumer_timeout_ms=3000)
for msg in consumer:
    print("consumed:", msg.value)
```

### 3. Search it (about a second later)

```bash
curl -s -X POST http://localhost:6092/orders/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"match": {"value": "refund"}}}'
```

Returns the two matching records, ranked by BM25. `_source.value` holds the record value as a string.

### 4. Query it with SQL

Envelope columns are queryable immediately:

```bash
curl -s -X POST http://localhost:6092/_sql \
  -H 'Content-Type: application/json' \
  -d '{"query": "SELECT _offset, CAST(_value AS VARCHAR) AS value FROM orders ORDER BY _offset"}'
```

Columns inferred from the JSON values (`city`, `amount`, …) appear after the first Parquet flush, about **60 seconds** after the produce. Until then this query fails with `No field named city`:

```bash
curl -s -X POST http://localhost:6092/_sql \
  -H 'Content-Type: application/json' \
  -d '{"query": "SELECT city, COUNT(*) AS orders, AVG(amount) AS avg_amount FROM orders GROUP BY city ORDER BY orders DESC"}'
```

```json
{"columns":["city","orders","avg_amount"],"rows":[{"city":"Berlin","orders":2,"avg_amount":72.375},{"city":"Lisbon","orders":1,"avg_amount":12.0}],"row_count":2,"execution_time_ms":4,"truncated":false}
```

## Status and Limitations

Stated up front, so you don't have to dig for them:

- **One main maintainer.** No third-party production users that I know of. Releases are frequent; see [CHANGELOG.md](CHANGELOG.md).
- **Durability history.** Replication correctness bugs were fixed recently, and you should know about them:
  - Until PR #29 (August 2026), `acks=1` and `acks=all` replicated nothing in clusters; the published throughput figures from that time are withdrawn.
  - Until **v2.16.6 (2026-11-15)**, `acks=all` could acknowledge writes that no follower held (a timed-out quorum wait followed by an idempotent retry was reported as success).
  - Until **v2.16.8 (2026-11-15)**, a follower stuck behind the leader could stay in the ISR, be elected, and lose records acknowledged at `acks=all`.
  - Each entry in the [CHANGELOG](CHANGELOG.md) describes the bug and how it was reproduced (stock `kafka-verifiable-producer`, local 3-node cluster).
- **No Jepsen or independent verification.** Correctness testing is the Rust test suite in CI plus local end-to-end cluster scripts (`tests/cluster`, `tests/e2e`), including kill and failover cases. Exactly-once (idempotent producer, transactions) is implemented and tested with stock clients, not formally verified.
- **Indexes trail the log.** Search is about 1 s behind for small writes and tens of seconds after large bursts; typed SQL columns about 60 s. See [When data becomes queryable](#how-it-works-internally).
- **Kafka coverage is a subset.** No KIP-848 consumer protocol, delegation tokens or partition reassignment; ACL requests are answered but not enforced. Kafka Streams and Kafka Connect are not validated. See [Kafka Compatibility](#kafka-compatibility).
- **SQL is query-time DataFusion**, not stream processing: no windowed aggregations or stream joins.
- **Indexing shares the broker process** on a bounded background thread pool (`CHRONIK_BACKGROUND_THREADS`). It is not isolated the way separate clusters are.
- **Search builds against a Tantivy fork** (0.27 plus an upstream Block-WAND fix) until upstream releases it. See [docs/TANTIVY_FORK.md](docs/TANTIVY_FORK.md).
- **Vector search needs an embedding provider** (OpenAI or an external endpoint); embedding calls add latency and cost.
- **Default-off features.** Agent memory (`/memory/v1/*`) and the ontology layer are experimental. They are compiled only with `--features memory` (the Docker image and the `-gnu` release tarballs; not the static `linux-amd64`/`linux-arm64` tarballs or a default `cargo build`), and are switched on at runtime (`CHRONIK_MEMORY_ENABLED`, `CHRONIK_ONTOLOGY_ENABLED=true`). See [Optional add-ons](#optional-add-ons).
- **Benchmarks have caveats.** Cluster streaming throughput is withdrawn pending re-measurement. The search comparison below is one node, one partition, one run, on a synthetic corpus. Read the caveats next to each table.

## Benchmarks and Validation

### Search Latency vs OpenSearch (v2.20.2, 2026-11-26)

p50 latency under load, from [PR #111](https://github.com/lspecian/chronik-stream/pull/111):

| p50 | Chronik v2.20.2 | OpenSearch 2.19.1 |
|---|---|---|
| query rounds in 60 s | **61,425** | 14,250 |
| match (text) | **0.55 ms** | 0.96 ms |
| term | **0.44 ms** | 17.96 ms |
| bool (text + filter) | **0.44 ms** | 4.56 ms |
| match_all | **0.26 ms** | 0.65 ms |
| date_histogram | **4.64 ms** | 6.70 ms |

**Method:** 10M synthetic log records, one Chronik node, one partition, 3 merged archives; 8 virtual users (k6) for 60 s; same machine and same day as OpenSearch 2.19.1 in Docker with an explicit mapping and a 4 GB heap.

**Caveats:**

- One run, one machine. The hardware was not recorded.
- Synthetic corpus (a 2,000-word vocabulary, many score ties). Chronik's newest-first early termination benefits most on exactly this shape.
- Chronik's archives were merged (3) and written newest-first; OpenSearch was **not** force-merged.
- The same few query bodies repeat for 60 s. Chronik caches per-archive count/aggregation results (`CHRONIK_SEARCH_RESULT_CACHE_BYTES`, 64 MB default); OpenSearch's request cache only applies to `size: 0`.
- **Freshness is the trade-off.** After the 10M burst, a marker record was searchable after ~22 s on Chronik vs ~2.5 s on OpenSearch. Every Chronik record was searchable 54–57 s after the burst by default (about 2 s with the opt-in `searchable.throttle.enabled` topic config, at ~4.5× slower ingest).

**Reproduce:** the scripts are in [tests/search_bench](tests/search_bench) (`gen.sh` builds the deterministic corpus, `run.sh` runs Chronik, `opensearch.sh` runs OpenSearch, `queries.js` is the k6 query mix). They need `KAFKA_HOME`, `kcat`, `jq` and Docker. The method and intermediate tables are in [docs/design/SEARCH_PROFILES_AND_ARCHIVE_TIER.md](docs/design/SEARCH_PROFILES_AND_ARCHIVE_TIER.md) §11. A one-command repro with recorded hardware and raw results is coming.

### Search Quality (WANDS, v2.4.0, 2026-03-04)

| System | NDCG@10 | Notes |
|--------|---------|-------|
| **Chronik BM25** (Tantivy) | **0.5927** | v2.4.0, 3-node cluster, run 2026-03-04 |
| Elasticsearch BM25 | ~0.50-0.56 | Published baselines, not run by us |

Measured on the [WANDS dataset](https://github.com/wayfair/WANDS) (42,994 products, 480 queries). This compares against published Elasticsearch numbers, **not** a same-machine run, and ranking has changed since v2.4.0. A hybrid (BM25 + vector) figure is omitted: in that run the vector leg returned empty results, so it measured BM25 alone. See [docs/DATASET_VALIDATION_REPORT.md](docs/DATASET_VALIDATION_REPORT.md).

### Scale Validation (Amazon Reviews, v2.4.0, historical)

| Dataset | Records | Text Search p50 | SQL p50 | Errors |
|---------|---------|-----------------|---------|--------|
| Amazon Appliances | 2.1M | 2.8ms @ 1000 VUs | 43ms | 0.00% |
| Amazon Grocery | 14.3M | 8.7ms @ 1000 VUs | 360ms | 0.00% |
| Amazon Electronics | 43.9M | 112ms @ 1000 VUs | 185ms | 0.00% |

Run on v2.4.0 (March 2026), before the replication fixes above, on a 3-node bare-metal cluster (Dell Xeon E5-2667v4, 32 cores, 256 GB RAM, 1 GbE, MicroK8s). Kept for history; not re-measured on current releases.

### Streaming Throughput

> ⚠️ **Withdrawn pending re-measurement (2026-08-12).** The cluster figures
> previously published here were measured on v2.2.25, which carried a bug
> (fixed in PR #29) that made `acks=1` and `acks=all` replicate **nothing**.
> The cluster was reporting `isr:[1,2,3]` while holding a single copy, so those
> numbers describe a 3-node cluster doing one node's work. They are not
> comparable to anything measured after the fix, and are not reproduced here.
>
> Re-measurement against the rebuilt replication path is a release gate — see
> [docs/ROADMAP_REPLICATION.md](docs/ROADMAP_REPLICATION.md). The historical
> report is kept, with the same caveat, in
> [BARE_METAL_PERFORMANCE.md](BARE_METAL_PERFORMANCE.md).

## Use Cases

### Operational Analytics
Instead of Kafka + a sink + ClickHouse for ad-hoc questions over event data: produce via the Kafka protocol, query with SQL.

```bash
# Produce events from your application (any Kafka client), to a topic
# created with columnar.enabled=true. Typed columns appear after the first
# Parquet flush (~60 s); envelope columns are queryable immediately.
curl -X POST http://localhost:6092/_sql \
  -H 'Content-Type: application/json' \
  -d '{"query": "SELECT city, COUNT(*), AVG(amount) FROM orders GROUP BY city"}'
```

### Real-Time Search
Instead of Kafka + Kafka Connect + Elasticsearch: records on a `searchable=true` topic are searchable about a second after the ack.

```bash
# Full-text search on one topic (Elasticsearch-style query DSL)
curl -X POST http://localhost:6092/support-tickets/_search \
  -H 'Content-Type: application/json' \
  -d '{"query": {"match": {"value": "login timeout error"}}}'
```

### Semantic Retrieval
Instead of Kafka + custom ETL + a vector database: records on a `vector.enabled=true` topic are embedded and indexed for semantic retrieval.

```bash
# Semantic search — "what happened recently that's similar to this?"
curl -X POST http://localhost:6092/_vector/incidents/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "database connection pool exhausted", "k": 10}'

# Hybrid search — combine keyword precision with semantic recall
curl -X POST http://localhost:6092/_vector/incidents/hybrid \
  -H 'Content-Type: application/json' \
  -d '{"query": "OOM kill postgres", "k": 10, "vector_weight": 0.7}'
```

### Multi-Modal Retrieval
Use the query orchestrator to search across modalities and topics in a single request:

```bash
curl -X POST http://localhost:6092/_query \
  -H 'Content-Type: application/json' \
  -d '{
    "query": "payment failures in EU",
    "topics": ["orders", "support-tickets", "incidents"],
    "backends": ["search", "vector"],
    "k": 20
  }'
```

Results are fused via Reciprocal Rank Fusion (RRF) and returned as a single ranked list.

## SQL API

Query Kafka topics with SQL. The topic needs `columnar.enabled=true`, set at create time by an AdminClient (see the [Quick Start](#2-create-a-topic-produce-consume)) or afterwards with `kafka-configs.sh` (see [Per-Topic Capability Model](#per-topic-capability-model)). There is no HTTP endpoint for creating topics.

```bash
# Unified view (hot + cold)
curl -X POST http://localhost:6092/_sql \
  -H 'Content-Type: application/json' \
  -d '{"query": "SELECT city, COUNT(*) AS cnt, AVG(amount) FROM orders GROUP BY city ORDER BY cnt DESC LIMIT 10"}'

# Recent records still in the in-memory hot buffer (envelope columns only)
curl -X POST http://localhost:6092/_sql \
  -H 'Content-Type: application/json' \
  -d '{"query": "SELECT _partition, _offset, CAST(_value AS VARCHAR) AS value FROM orders_hot LIMIT 10"}'

# Historical data (Parquet files), with typed JSON columns
curl -X POST http://localhost:6092/_sql \
  -H 'Content-Type: application/json' \
  -d '{"query": "SELECT * FROM orders_cold WHERE _partition = 0 LIMIT 10"}'

# List tables
curl http://localhost:6092/_sql/tables
```

JSON message values are inferred into typed columns (Int64, Float64, Boolean, Utf8) when records are written to Parquet. No UDFs or schema definitions needed.

**Tables per topic:**

| Table | Source | Columns |
|-------|--------|---------|
| `{topic}_hot` | In-memory Arrow buffer (recent records) | Envelope only (`_topic`, `_partition`, `_offset`, `_timestamp`, `_timestamp_type`, `_key`, `_value`) |
| `{topic}_cold` | Parquet files (written by the indexer, first flush ~60 s) | Envelope + columns inferred from JSON values |
| `{topic}` | Unified view (hot + cold) | Envelope + inferred columns once a cold table exists |

See [docs/COLUMNAR_STORAGE_GUIDE.md](docs/COLUMNAR_STORAGE_GUIDE.md) for full SQL reference.

## Cluster Mode

Chronik supports multi-node clusters with Raft consensus for metadata and leadership, replication with `min_insync_replicas`, and online node add/remove. Minimum 3 nodes for quorum.

```bash
# Start a 3-node cluster (one terminal per node)
./chronik-server start --config cluster-node1.toml
./chronik-server start --config cluster-node2.toml
./chronik-server start --config cluster-node3.toml
```

```toml
# cluster-node1.toml
node_id = 1
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
kafka = "node1:9092"
wal = "node1:9291"
raft = "node1:5001"

[[peers]]
id = 2
kafka = "node2:9092"
wal = "node2:9291"
raft = "node2:5001"

[[peers]]
id = 3
kafka = "node3:9092"
wal = "node3:9291"
raft = "node3:5001"
```

**Cluster management:**

```bash
# Add a node
./chronik-server cluster add-node 4 --kafka node4:9092 --wal node4:9291 --raft node4:5001 --config cluster.toml

# Check status
./chronik-server cluster status --config cluster.toml

# Remove a node gracefully
./chronik-server cluster remove-node 4 --config cluster.toml
```

See [docs/RUNNING_A_CLUSTER.md](docs/RUNNING_A_CLUSTER.md) for the complete cluster guide.

## Optional Add-ons

- **Ontology layer** (v2.13.0+, experimental, off by default): define typed objects and relationships over your streams and query them with `get_object` (with point-in-time `as_of`), `traverse`/`related`/`neighbors`, and an MCP tools server. Needs a `--features memory` build and `CHRONIK_ONTOLOGY_ENABLED=true`. Authored with the `chronik` CLI. See [docs/ONTOLOGY_SDK.md](docs/ONTOLOGY_SDK.md).
- **Agent memory** (`/memory/v1/*`, experimental, off by default): needs a `--features memory` build. See [docs/ROADMAP_AGENT_MEMORY.md](docs/ROADMAP_AGENT_MEMORY.md).

## Kafka Compatibility

Chronik implements the Kafka wire protocol. The versions below are what the broker advertises in `ApiVersions` (from `supported_api_versions()` in `crates/chronik-protocol/src/parser.rs`):

| API | Versions | API | Versions |
|-----|----------|-----|----------|
| Produce | v0-v9 | ApiVersions | v0-v4 |
| Fetch | v0-v13 | CreateTopics | v0-v7 |
| ListOffsets | v0-v7 | DeleteTopics | v0-v3 |
| Metadata | v0-v12 | CreatePartitions | v0-v1 |
| OffsetCommit | v0-v8 | DeleteRecords | v0-v1 |
| OffsetFetch | v0-v8 | DescribeConfigs | v0-v4 |
| OffsetDelete | v0 | AlterConfigs | v0-v2 |
| OffsetForLeaderEpoch | v0 | IncrementalAlterConfigs | v0 |
| FindCoordinator | v0-v4 | DescribeCluster | v1 |
| JoinGroup | v0-v9 | DescribeLogDirs | v0 |
| SyncGroup | v0-v5 | DescribeClientQuotas | v0-v1 (no quotas) |
| Heartbeat | v0-v4 | InitProducerId | v0-v4 |
| LeaveGroup | v0-v5 | AddPartitionsToTxn | v0-v3 |
| DescribeGroups | v0-v5 | AddOffsetsToTxn | v0-v3 |
| ListGroups | v0-v4 | EndTxn | v0-v3 |
| DeleteGroups | v0-v1 | TxnOffsetCommit | v0-v3 |
| SaslHandshake | v0-v1 | DescribeAcls / CreateAcls / DeleteAcls | v0-v1 (not enforced) |
| SaslAuthenticate | v0-v2 | Describe/AlterUserScramCredentials | v0 |

ACL requests are answered with `SECURITY_DISABLED`; there is no authorizer. Broker-to-broker APIs (LeaderAndIsr, StopReplica, UpdateMetadata, ControlledShutdown, WriteTxnMarkers) are listed for client compatibility but rejected.

**Not implemented:** the KIP-848 consumer group protocol (`ConsumerGroupHeartbeat`), delegation tokens, partition reassignment, `ElectLeaders`, `AlterClientQuotas`, `DescribeProducers`/`DescribeTransactions`/`ListTransactions`. These are not advertised, so clients treat them as unsupported.

**Clients:** exercised in local end-to-end scripts with the Java CLI tools (`kafka-topics`, `kafka-configs`, `kafka-console-*`, `kafka-verifiable-producer`), kafka-python and librdkafka-based clients (rdkafka, confluent-kafka). CI runs the Rust test suite, not external clients. See [docs/kafka-client-compatibility.md](docs/kafka-client-compatibility.md).

Chronik also provides a Confluent-compatible **Schema Registry** (Avro, JSON Schema, Protobuf) on the admin API port. See [docs/SCHEMA_REGISTRY.md](docs/SCHEMA_REGISTRY.md).

## Configuration Reference

### Commands

```bash
chronik-server start [OPTIONS]          # Start server (auto-detects single-node or cluster)
chronik-server cluster <SUBCOMMAND>     # Manage cluster (add-node, remove-node, status)
chronik-server version                  # Display version info
chronik-server compact <SUBCOMMAND>     # Manage WAL compaction
```

### Key Environment Variables

```bash
# Server
CHRONIK_DATA_DIR              # Data directory (default: ./data)
CHRONIK_ADVERTISE             # Address advertised to Kafka clients (REQUIRED for Docker)
CHRONIK_UNIFIED_API_PORT      # Unified API port (default: 6092)
RUST_LOG                      # Log level: error, warn, info, debug, trace

# Performance
CHRONIK_WAL_PROFILE           # WAL batch interval: low (2ms, default), medium (10ms), high (50ms), ultra (100ms)
CHRONIK_PRODUCE_PROFILE       # Flush profile: low-latency, balanced, high-throughput, extreme
CHRONIK_BACKGROUND_THREADS    # Background indexing pool size

# SQL / Columnar
CHRONIK_HOT_BUFFER_ENABLED    # Enable in-memory SQL queries (default: true)
CHRONIK_HOT_BUFFER_MAX_RECORDS  # Max records per partition in hot buffer (default: 100000)

# Vector / Embeddings
OPENAI_API_KEY                # OpenAI API key for vector embeddings
CHRONIK_EMBEDDING_PROVIDER    # Embedding provider: openai, external

# Object Storage (tiered storage)
OBJECT_STORE_BACKEND          # Backend: s3, gcs, azure, local (default: local)
S3_BUCKET                     # S3 bucket name
S3_REGION                     # AWS region
S3_ENDPOINT                   # Custom endpoint (e.g., MinIO)

# Cluster
CHRONIK_ADMIN_API_KEY         # Admin API authentication (REQUIRED for production clusters)
```

## Security

```bash
# SASL authentication (SCRAM-SHA-512, SCRAM-SHA-256, PLAIN). Off by default;
# there are no default users.
CHRONIK_SASL_ENABLED=true \
CHRONIK_SASL_USERS='myuser:securepass123,app:app-secret' \
  ./chronik-server start

# Cluster mode additionally needs a shared inter-broker secret (same on every
# broker, 16+ chars) so followers can authenticate to leaders. Without it a
# SASL-required node refuses to start. Generate it ONCE (e.g. openssl rand -hex 32)
# and give every broker that same value.
CHRONIK_SASL_ENABLED=true \
CHRONIK_SASL_USERS='myuser:securepass123' \
CHRONIK_INTER_BROKER_SECRET='<the same 64-hex-char value on every broker>' \
  ./chronik-server start --config cluster.toml
```

The Kubernetes operator generates the inter-broker secret for SASL clusters.

Admin API and Schema Registry support API key and HTTP Basic Auth respectively. See [docs/ADMIN_API_SECURITY.md](docs/ADMIN_API_SECURITY.md).

## Performance Tuning

### WAL Profiles

The WAL uses the `low` profile (2 ms batch window) unless overridden:

```bash
CHRONIK_WAL_PROFILE=low        # Default; containers, small VMs (2ms batch)
CHRONIK_WAL_PROFILE=medium     # Typical servers (10ms batch)
CHRONIK_WAL_PROFILE=high       # Dedicated servers (50ms batch)
CHRONIK_WAL_PROFILE=ultra      # Maximum throughput (100ms batch)
```

### Producer Flush Profiles

| Profile | Flush Interval | Buffer | Use Case |
|---------|----------------|--------|----------|
| `low-latency` | 10ms | 16MB | Real-time analytics, instant messaging |
| `balanced` | 100ms | 32MB | General-purpose workloads |
| `high-throughput` | 500ms | 128MB | Data pipelines, ETL, batch processing |
| `extreme` | 2000ms | 512MB | Bulk ingestion, data migrations |

## Project Structure

```
chronik-stream/
├── crates/
│   ├── chronik-server/       # Unified server binary
│   ├── chronik-protocol/     # Kafka wire protocol
│   ├── chronik-wal/          # Write-Ahead Log, group commit, recovery
│   ├── chronik-storage/      # Storage abstraction, segment I/O, WalIndexer
│   ├── chronik-columnar/     # Arrow/Parquet, DataFusion SQL, HNSW vector search
│   ├── chronik-embeddings/   # Embedding providers (OpenAI, external)
│   ├── chronik-search/       # Tantivy full-text search
│   ├── chronik-query/        # Query processing
│   ├── chronik-common/       # Shared types, metadata traits
│   ├── chronik-raft/         # Raft consensus
│   ├── chronik-raft-bridge/  # Raft integration bridge
│   ├── chronik-scram/        # SCRAM authentication
│   ├── chronik-operator/     # Kubernetes operator (ChronikCluster CRD)
│   ├── chronik-monitoring/   # Prometheus metrics
│   ├── chronik-config/       # Configuration management
│   ├── chronik-memory/       # Agent memory (optional, --features memory)
│   ├── chronik-ontology/     # Ontology layer (optional, --features memory)
│   ├── chronik-cli/          # `chronik` CLI
│   ├── chronik-bench/        # Benchmarking tool
│   └── chronik-perf/         # Performance tooling
├── tests/                    # Integration and end-to-end tests, benchmarks
├── docs/                     # Guides and references
└── .github/workflows/        # CI/CD
```

## Project Status

Chronik Stream is in active development by one main maintainer. See [Status and limitations](#status-and-limitations) for what is and isn't proven, the [releases page](https://github.com/lspecian/chronik-stream/releases) for the current version, and [CHANGELOG.md](CHANGELOG.md) for release history.

**Roadmap priorities:**
- Re-measure cluster streaming throughput on the rebuilt replication path
- Cross-encoder reranking for search quality (Phase 10)
- Query governance and policy controls (Phase 11)

See [docs/ROADMAP_RELEVANCE_ENGINE.md](docs/ROADMAP_RELEVANCE_ENGINE.md) and [docs/ROADMAP_REPLICATION.md](docs/ROADMAP_REPLICATION.md).

## Documentation

| Guide | Description |
|-------|-------------|
| [CHANGELOG.md](CHANGELOG.md) | Release history |
| [docs/RUNNING_A_CLUSTER.md](docs/RUNNING_A_CLUSTER.md) | Cluster setup guide |
| [docs/COLUMNAR_STORAGE_GUIDE.md](docs/COLUMNAR_STORAGE_GUIDE.md) | SQL and Parquet storage |
| [docs/SEARCHABLE_TOPICS.md](docs/SEARCHABLE_TOPICS.md) | Full-text search |
| [docs/VECTOR_SEARCH_GUIDE.md](docs/VECTOR_SEARCH_GUIDE.md) | Vector search and embeddings |
| [docs/SCHEMA_REGISTRY.md](docs/SCHEMA_REGISTRY.md) | Confluent-compatible Schema Registry |
| [docs/API_REFERENCE.md](docs/API_REFERENCE.md) | Unified REST API reference |
| [docs/kafka-client-compatibility.md](docs/kafka-client-compatibility.md) | Kafka client compatibility |
| [docs/DATASET_VALIDATION_REPORT.md](docs/DATASET_VALIDATION_REPORT.md) | Scale and quality validation |
| [docs/DISASTER_RECOVERY.md](docs/DISASTER_RECOVERY.md) | Disaster recovery: metadata images and the object-store archive (`CHRONIK_DR_RESTORE`) |
| [docs/ADMIN_API_SECURITY.md](docs/ADMIN_API_SECURITY.md) | Admin API security |
| [docs/TANTIVY_FORK.md](docs/TANTIVY_FORK.md) | Why search builds against a Tantivy fork |
| [BASELINE_PERFORMANCE.md](BASELINE_PERFORMANCE.md) | Performance benchmarks |
| [BARE_METAL_PERFORMANCE.md](BARE_METAL_PERFORMANCE.md) | Cluster throughput testing (historical, withdrawn figures) |

## Contributing

Contributions are welcome. Please feel free to submit a Pull Request.

## License

Apache License 2.0. See [LICENSE](LICENSE) for details.
