# Distributed Query Layer — Architecture & Roadmap

## Overview

Chronik's Distributed Query Layer enables any node in a cluster to serve complete query results by routing requests to the nodes that own the relevant data. Every production distributed search system (Elasticsearch, Milvus, Qdrant, CockroachDB) implements this pattern. This document describes Chronik's implementation.

## Problem

In a 3-node cluster, each node indexes data for its locally-owned partitions:
- **Tantivy** (full-text search): per-partition indexes at `{data_dir}/tantivy_indexes/{topic}/{partition}/`
- **HNSW** (vector search): per-partition indexes keyed by `TopicPartitionKey(topic, partition, model_id)`
- **Parquet** (SQL): per-partition files at `parquet/{topic}/partition={id}/`

A query to any single node returns only that node's local partition data (~1/3 of total for a 3-partition topic). Clients must implement their own scatter-gather to get complete results.

## Architecture

```
                         K8s Service (round-robin)
                                  │
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
           Node 1              Node 2              Node 3
        (coordinator)       (coordinator)       (coordinator)
              │                   │                   │
     Any node can coordinate — whoever receives the request
              │
    ┌─────────┴─────────┐
    ▼                   ▼
 QueryRouter         QueryRouter
 (partition map)     (partition map)
    │                   │
    ├── Local partitions: execute directly (zero network hop)
    ├── Remote partitions: HTTP POST to owner node (with X-Chronik-Depth: 1)
    └── Merge results (two-level reduction: each node top-K, then coordinator top-K)
```

### Key Design Principles

| Principle | Implementation | Inspired By |
|-----------|---------------|-------------|
| **Any-node coordination** | K8s Service round-robins; every node has QueryRouter | ES, Qdrant, CockroachDB |
| **Partition-aware routing** | Route to nodes owning partitions, not blind broadcast | ES ARS, ksqlDB key routing |
| **Two-level reduction** | Each node returns top-K locally, coordinator merges | ES query phase, Milvus multi-level |
| **Computation pushdown** | HNSW/Tantivy/DataFusion execute where data lives | CockroachDB DistSQL, TiDB Coprocessor |
| **Graceful degradation** | Peer timeout → partial results + warning | ES `_shards.failed` pattern |
| **Loop prevention** | `X-Chronik-Depth: 1` header on forwarded requests | Standard scatter-gather pattern |

### Query Flow

```
1. Client sends POST /_vector/wands/search to Node 2 (via K8s Service)
2. Node 2's QueryRouter checks: is this a forwarded request? (X-Chronik-Depth header)
   → No header → this is the coordinator
3. QueryRouter looks up partition map for topic "wands":
   - Partition 0: leader=Node 1
   - Partition 1: leader=Node 2 (local!)
   - Partition 2: leader=Node 3
4. Execute locally: search partition 1's HNSW index → local top-K results
5. Fan out in parallel:
   - HTTP POST to Node 1:6092/_vector/wands/search (X-Chronik-Depth: 1) → Node 1 returns its top-K
   - HTTP POST to Node 3:6092/_vector/wands/search (X-Chronik-Depth: 1) → Node 3 returns its top-K
6. Node 1 and Node 3 see X-Chronik-Depth: 1 → execute locally only, no further fan-out
7. Coordinator (Node 2) merges all results:
   - Dedup by (partition, offset)
   - Sort by score descending
   - Truncate to requested K
8. Return merged response to client
```

### Load Balancing

**Coordination distributes naturally.** K8s Service round-robins across pods:
- Request A → Node 1 coordinates (local + fan to 2,3)
- Request B → Node 2 coordinates (local + fan to 1,3)
- Request C → Node 3 coordinates (local + fan to 1,2)

Each node does the same total work: 1 local execution + serve (N-1) forwarded requests + coordinate some requests. No single node is a hotspot.

## Merge Algorithms

### Vector Search (`/_vector/{topic}/search`)

```
Input: local VectorSearchResponse + Vec<peer VectorSearchResponse>
1. Collect all VectorSearchResultItems from all responses
2. Deduplicate by (partition, offset) — keep entry with highest score
3. Sort by score descending
4. Truncate to K
5. Sum total_vectors across all responses
Output: merged VectorSearchResponse
```

### Hybrid Search (`/_vector/{topic}/hybrid`)

```
Input: local HybridSearchResponse + Vec<peer HybridSearchResponse>
1. Each node has already done local RRF (vector + text → score)
2. Collect all HybridSearchResultItems
3. Deduplicate by (partition, offset) — keep highest score
4. Sort by score descending
5. Truncate to K
6. Sum vector_results_count and text_results_count
Output: merged HybridSearchResponse
```

### SQL Query (`/_sql`)

```
Input: local SqlResponse + Vec<peer SqlResponse>
1. Use column schema from local response
2. Union all rows from all responses
3. Truncate to limit
Output: merged SqlResponse

KNOWN LIMITATION: Aggregation queries (COUNT, SUM, AVG, GROUP BY)
cannot be correctly merged by simple row union. Aggregations return
per-node partial results. Correct distributed aggregation requires
pushdown + coordinator-side re-aggregation (future work).
```

### ES-Compatible Search (`/_search`)

```
Input: local SearchResponse + Vec<peer SearchResponse>
1. Collect all hits from all responses
2. Sort by _score descending
3. Truncate to size
4. Sum total hits and shard counts
Output: merged SearchResponse
```

## Peer Discovery

### Static Discovery (Phase 1-3)

Peer URLs computed at startup from `ClusterConfig`:

```rust
// ClusterConfig.peers contains all nodes with their addresses
// Unified API port read from CHRONIK_UNIFIED_API_PORT env var (default: 6092)
let api_port = std::env::var("CHRONIK_UNIFIED_API_PORT")
    .unwrap_or_else(|_| "6092".to_string())
    .parse::<u16>().unwrap_or(6092);
for peer in config.peers {
    if peer.id != self_node_id {
        let host = peer.kafka.split(':').next();  // Extract host from "host:9092"
        peer_urls.push(format!("http://{}:{}", host, api_port));
    }
}
```

### Partition Map

Built from `metadata_store.get_partition_assignments(topic)`:

```rust
// PartitionAssignment { topic, partition, leader_id, replicas: Vec<u64> }
// Available via DashMap (lock-free O(1) reads)
```

**Refresh strategy:** Cache with 30-second TTL. Invalidate on topology changes (node add/remove). For Phase 1, refresh before each fan-out call. For Phase 3, background refresh task.

## Configuration

| Environment Variable | Default | Description |
|---------------------|---------|-------------|
| `CHRONIK_QUERY_TIMEOUT_SECS` | `5` | HTTP timeout for peer requests |
| `CHRONIK_QUERY_FANOUT_ENABLED` | `true` | Enable/disable distributed queries |

## Phased Implementation

### Phase 1: QueryRouter Core + Vector Search — COMPLETE

- [x] `query_router.rs` — QueryRouter struct, PeerNode, PartitionMap, QueryRouterConfig
- [x] Peer discovery from ClusterConfig (host from kafka addr, port from `CHRONIK_UNIFIED_API_PORT` env var)
- [x] `fan_out_post<Req, Resp>()` — generic parallel HTTP POST with `X-Chronik-Depth: 1`
- [x] `is_forwarded_request()` — depth check for loop prevention
- [x] `merge_vector_responses()` — dedup by (partition, offset), best score, sort, truncate
- [x] `merge_hybrid_responses()` — same pattern for RRF-fused hybrid results
- [x] `needs_fan_out()` helper in vector_handler.rs
- [x] Fan-out wired into `search()`, `search_by_vector()`, `hybrid_search()`
- [x] UnifiedApiState.query_router field + `.with_query_router()` builder
- [x] QueryRouter initialized in `run_cluster_mode()` in main.rs
- [x] 8 unit tests: depth check, dedup, truncation, peer URL computation
- [x] 903 workspace tests passing

### Phase 2: SQL Fan-Out + ES Search Fan-Out — COMPLETE

SQL fan-out:
- [x] `merge_sql_responses()` — union rows, truncate to limit
- [x] `all_peers()` method on QueryRouter for global (non-topic-scoped) queries
- [x] Fan-out wired into `execute_sql()` handler
- [x] Unit tests for SQL merge with union + truncation

ES-compatible search fan-out:
- [x] `SearchFanoutState` with both `Arc<SearchApi>` and `Arc<QueryRouter>`
- [x] `search_all_fanout()` / `search_index_fanout()` wrapper handlers in `search_handler.rs`
- [x] `search_router_with_fanout()` creates combined router (fan-out search + original CRUD)
- [x] `Clone` derives on `SearchRequest` and all nested query DSL types
- [x] Cluster mode in `main.rs` auto-upgrades to fan-out search router when QueryRouter available
- [x] `merge_search_responses()` merges by `_score`, sums shard counts (implemented in Phase 1)
- [x] 903 workspace tests passing

### Phase 3: Partition-Aware Routing — COMPLETE

- [x] `refresh_partition_map()` — reads `metadata_store.get_partition_assignments(topic)` (DashMap, lock-free)
- [x] `nodes_for_topic()` — returns only peers owning partitions for the topic (falls back to all peers if map empty)
- [x] `all_partitions_local()` — skip fan-out when every partition has this node as a replica
- [x] Vector/hybrid handlers call `needs_fan_out()` which refreshes partition map before routing
- [x] SQL fan-out uses `all_peers()` (SQL queries are global, not topic-scoped)

### Phase 4: Adaptive Replica Selection — COMPLETE

- [x] `PeerMetrics` struct with lock-free EWMA latency tracking (CAS loops, no mutexes)
- [x] `AtomicU64` outstanding request counting per peer
- [x] EWMA formula: `new = 0.3 * latest + 0.7 * old` (integer math: `(3 * latest + 7 * old) / 10`)
- [x] ARS score: `latency_ewma * (1 + outstanding / 10)` — lower = preferred
- [x] Failure penalty: double EWMA on failure (capped at 10s), makes failing peers less preferred
- [x] `nodes_for_topic()` upgraded: for each non-local partition, selects replica with lowest ARS score
- [x] Minimizes fan-out: queries only the minimum peer set needed to cover all partitions
- [x] All-local optimization: when all partitions have local replica, returns empty (no fan-out)
- [x] `fan_out_post()` tracks: increment outstanding before request, record latency on success, penalize on failure
- [x] `peer_ars_scores()` method for monitoring (returns node_id, score, latency_ms, outstanding)
- [x] Fallback to all peers when partition map not yet populated
- [x] 11 unit tests: EWMA calculation, convergence, ARS scoring, failure penalty, fast recovery, peer selection
- [x] 914 workspace tests passing

## Performance Expectations

| Metric | Single Node | With Fan-Out (3 nodes) | With ARS |
|--------|-------------|----------------------|----------|
| Vector search latency | ~50ms | ~80ms (+30ms network) | ~70ms (fewer peers) |
| Text search latency | ~10ms | ~40ms (+30ms network) | ~30ms (fewer peers) |
| SQL query latency | ~50ms | ~80ms (+30ms network) | ~80ms (global, all peers) |
| Results completeness | ~33% | 100% | 100% |
| Network per query | 0 | 2 × (request + response) | 1-2 × (minimum peers) |

Latency overhead = max(peer_RTT), not sum, since peers are queried in parallel.

**ARS benefits**: When RF > 1, multiple peers own the same partitions. ARS selects the fastest/least-loaded peer for each partition, reducing total fan-out and preferring responsive peers. Failing peers are automatically deprioritized (EWMA penalty) and recover as they respond successfully.

## Known Limitations

1. **SQL aggregations**: COUNT/SUM/AVG/GROUP BY return per-node partial results. Correct distributed aggregation requires pushdown + coordinator-side re-aggregation (future work).
2. **Partition map staleness**: Refreshed before each fan-out from metadata store (DashMap, lock-free). After topology changes (node add/remove), queries may briefly route to old leaders. Graceful degradation: peer returns error, coordinator uses available results.
3. **No query cancellation**: If coordinator times out waiting for a peer, the peer's query continues to completion (wasted work). Future: add cancellation tokens.
4. **SQL fan-out is global**: SQL queries fan out to all peers regardless of which topics they reference. Parsing SQL to extract table names for partition-aware routing is future work.
5. **Vector fan-out disabled with RF=N (N=cluster size)**: `all_partitions_local()` checks partition assignments, not vector index availability. With RF=3 on a 3-node cluster, all partitions are assigned to all nodes, so fan-out is skipped. However, vector indices (HNSW) are only built on the node that processed the WAL segment, meaning each node has vectors for ~1/N of partitions. Fix needed: check VectorIndexManager for actual vector presence per partition, not just partition assignment. Discovered during DV-1 WANDS evaluation (2026-03-04).
