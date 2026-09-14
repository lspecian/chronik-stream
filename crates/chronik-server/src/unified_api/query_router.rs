//! Distributed Query Router for cluster-mode query fan-out.
//!
//! Implements partition-aware scatter-gather for all unified API query endpoints.
//! When a query arrives at any node, the QueryRouter:
//! 1. Executes locally for this node's partitions
//! 2. Forwards to peer nodes that own other partitions (with X-Chronik-Depth: 1)
//! 3. Merges results and returns unified response
//!
//! Design inspired by Elasticsearch scatter-gather, Milvus multi-level reduction,
//! and CockroachDB DistSQL patterns. See docs/DISTRIBUTED_QUERY_LAYER.md.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use reqwest::Client;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use chronik_common::metadata::traits::MetadataStore;
use chronik_config::ClusterConfig;

use super::sql_handler::SqlResponse;
use super::vector_handler::{
    HybridSearchResponse, HybridSearchResultItem, VectorSearchResponse, VectorSearchResultItem,
};

// Re-export for use in handlers
pub use chronik_search::api::{Hit, HitsInfo, SearchResponse, ShardInfo, TotalHits};

/// HTTP header for loop prevention. Forwarded requests include this header.
/// Receiving nodes with depth >= 1 execute locally only (no further fan-out).
pub const DEPTH_HEADER: &str = "X-Chronik-Depth";

/// Check if this request was forwarded from another node (should not fan out further).
pub fn is_forwarded_request(headers: &HeaderMap) -> bool {
    headers
        .get(DEPTH_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
        .map(|depth| depth >= 1)
        .unwrap_or(false)
}

/// A peer node in the cluster.
#[derive(Debug, Clone)]
pub struct PeerNode {
    pub node_id: u64,
    pub url: String,
}

/// Partition-to-node mapping for routing queries to the right nodes.
#[derive(Debug, Default)]
pub struct PartitionMap {
    /// topic -> partition -> leader_node_id
    pub leaders: HashMap<String, HashMap<u32, u64>>,
    /// topic -> partition -> all replica node IDs
    pub replicas: HashMap<String, HashMap<u32, Vec<u64>>>,
}

/// Per-peer performance metrics for Adaptive Replica Selection (ARS).
///
/// Tracks EWMA response latency and outstanding (in-flight) request count.
/// Used to score peers and prefer faster/less-loaded replicas when multiple
/// replicas own the same partition. Inspired by Elasticsearch's ARS.
///
/// All operations are lock-free using atomics (CAS loops for EWMA updates).
pub struct PeerMetrics {
    /// EWMA response latency in microseconds.
    /// Updated via CAS loop on each response. Initial value: 50ms.
    latency_ewma_us: AtomicU64,
    /// Number of currently in-flight requests to this peer.
    outstanding: AtomicU64,
}

impl PeerMetrics {
    /// Create new metrics with default EWMA of 50ms.
    fn new() -> Self {
        Self {
            latency_ewma_us: AtomicU64::new(50_000), // 50ms in microseconds
            outstanding: AtomicU64::new(0),
        }
    }

    /// Increment outstanding request count before sending a request.
    fn request_start(&self) {
        self.outstanding.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement outstanding and record response latency via EWMA.
    /// EWMA formula: new = alpha * latest + (1-alpha) * old, alpha = 0.3
    /// Integer math: (3 * latest + 7 * old) / 10
    fn request_end(&self, latency: Duration) {
        self.outstanding.fetch_sub(1, Ordering::Relaxed);
        let latency_us = latency.as_micros() as u64;
        loop {
            let old = self.latency_ewma_us.load(Ordering::Relaxed);
            let new = (3 * latency_us + 7 * old) / 10;
            if self
                .latency_ewma_us
                .compare_exchange_weak(old, new, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Record a failed request: decrement outstanding and penalize EWMA.
    /// Doubles the EWMA to make this peer less preferred (capped at 10s).
    fn request_failed(&self) {
        self.outstanding.fetch_sub(1, Ordering::Relaxed);
        loop {
            let old = self.latency_ewma_us.load(Ordering::Relaxed);
            let new = old.saturating_mul(2).min(10_000_000); // Cap at 10s
            if self
                .latency_ewma_us
                .compare_exchange_weak(old, new, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Compute ARS score: latency_ewma * (1 + outstanding / 10).
    /// Lower score = preferred peer.
    pub fn ars_score(&self) -> f64 {
        let latency_us = self.latency_ewma_us.load(Ordering::Relaxed) as f64;
        let outstanding = self.outstanding.load(Ordering::Relaxed) as f64;
        latency_us * (1.0 + outstanding / 10.0)
    }

    /// Get current EWMA latency in microseconds (for monitoring/debugging).
    pub fn latency_ewma_us(&self) -> u64 {
        self.latency_ewma_us.load(Ordering::Relaxed)
    }

    /// Get current outstanding request count (for monitoring/debugging).
    pub fn outstanding(&self) -> u64 {
        self.outstanding.load(Ordering::Relaxed)
    }
}

/// Configuration for query routing behavior.
#[derive(Debug, Clone)]
pub struct QueryRouterConfig {
    /// HTTP request timeout for peer calls
    pub timeout: Duration,
    /// Maximum fan-out depth (prevents loops)
    pub max_depth: u32,
}

impl Default for QueryRouterConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(
                std::env::var("CHRONIK_QUERY_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(30),
            ),
            max_depth: 1,
        }
    }
}

/// Distributed query router for cluster-mode scatter-gather.
///
/// Created once at startup from ClusterConfig and stored in UnifiedApiState.
/// In single-node mode, no QueryRouter is created (field is None).
///
/// Includes Adaptive Replica Selection (ARS) metrics for preferring
/// faster/less-loaded peers when multiple replicas own the same partition.
#[derive(Clone)]
pub struct QueryRouter {
    /// HTTP client (connection-pooled, reused across requests)
    client: Client,
    /// All peer nodes in the cluster (excluding self)
    peers: Vec<PeerNode>,
    /// This node's ID
    self_node_id: u64,
    /// Partition-to-node mapping (refreshed from metadata store)
    partition_map: Arc<RwLock<PartitionMap>>,
    /// Configuration
    config: QueryRouterConfig,
    /// Per-peer ARS metrics (EWMA latency + outstanding request count).
    /// Shared via Arc so spawned fan-out tasks can update metrics.
    peer_metrics: Arc<HashMap<u64, PeerMetrics>>,
}

impl QueryRouter {
    /// Create a QueryRouter from cluster configuration.
    ///
    /// Computes peer unified API URLs from ClusterConfig peers:
    /// - Extract host from each peer's `kafka` address (e.g., "192.0.2.1" from "192.0.2.1:9092")
    /// - Port from CHRONIK_UNIFIED_API_PORT env var (default: 6092)
    /// - In K8s each pod has its own IP, all use the same port
    /// - Skip self (peer.id == config.node_id)
    pub fn new(cluster_config: &ClusterConfig) -> Self {
        let self_node_id = cluster_config.node_id;
        let config = QueryRouterConfig::default();

        // The unified-API port is per-peer, not cluster-global. `main.rs`
        // computes its own bind port as `6091 + node_id`, so each cluster
        // node ends up on a different port (6092 / 6093 / 6094 / ...).
        // For multi-host production deployments this is fine because the
        // env-overridable base port is the same across hosts; for single-
        // host test clusters (`tests/cluster/`) every peer must have its
        // own port or they all collide on the same URL — which silently
        // hides a third of the data on every distributed search and was
        // the root cause of the 4-of-18 zero-result items in the
        // LongMemEval-S pilot.
        //
        // Resolution order, per peer:
        //   1. `CHRONIK_UNIFIED_API_PORT_NODE_<id>` env override (rare)
        //   2. `CHRONIK_UNIFIED_API_PORT` if the env is set explicitly
        //      AND we're in a single-host config (kafka host matches
        //      ourselves) — same as before, preserves prod behaviour
        //   3. `6091 + peer.id`, matching `main.rs:unified_api_config.port`
        //
        // Production single-port deployments are unchanged because every
        // peer's id maps to the same `6092` when there's only one node
        // bound per host (id-1's port == 6092 on host A; id-2's port ==
        // 6093 on host B is moot since the URL uses host B's name).
        let env_port: Option<u16> = std::env::var("CHRONIK_UNIFIED_API_PORT")
            .ok()
            .and_then(|v| v.parse().ok());

        let peers: Vec<PeerNode> = cluster_config
            .peers
            .iter()
            .filter(|peer| peer.id != self_node_id)
            .map(|peer| {
                let host = peer
                    .kafka
                    .rsplit_once(':')
                    .map(|(h, _)| h)
                    .unwrap_or("localhost");
                let port = std::env::var(format!("CHRONIK_UNIFIED_API_PORT_NODE_{}", peer.id))
                    .ok()
                    .and_then(|v| v.parse::<u16>().ok())
                    .or(env_port)
                    .unwrap_or(6091 + peer.id as u16);
                PeerNode {
                    node_id: peer.id,
                    url: format!("http://{}:{}", host, port),
                }
            })
            .collect();

        info!(
            self_node_id = self_node_id,
            peer_count = peers.len(),
            peers = ?peers.iter().map(|p| &p.url).collect::<Vec<_>>(),
            "QueryRouter initialized"
        );

        let client = Client::builder()
            .timeout(config.timeout)
            .pool_max_idle_per_host(4)
            .build()
            .expect("Failed to create reqwest client for QueryRouter");

        let peer_metrics: HashMap<u64, PeerMetrics> = peers
            .iter()
            .map(|p| (p.node_id, PeerMetrics::new()))
            .collect();

        info!(
            peer_metrics_count = peer_metrics.len(),
            "ARS metrics initialized for peers"
        );

        Self {
            client,
            peers,
            self_node_id,
            partition_map: Arc::new(RwLock::new(PartitionMap::default())),
            config,
            peer_metrics: Arc::new(peer_metrics),
        }
    }

    /// Refresh the partition map from the metadata store.
    ///
    /// Called before fan-out to ensure routing is current.
    /// Uses `get_partition_assignments()` which is lock-free (DashMap).
    pub async fn refresh_partition_map(&self, metadata_store: &dyn MetadataStore, topic: &str) {
        match metadata_store.get_partition_assignments(topic).await {
            Ok(assignments) => {
                let mut map = self.partition_map.write().await;
                // Build new leader/replica maps, then insert (avoids double mutable borrow)
                let mut new_leaders = HashMap::new();
                let mut new_replicas = HashMap::new();
                for assignment in &assignments {
                    new_leaders.insert(assignment.partition, assignment.leader_id);
                    new_replicas.insert(assignment.partition, assignment.replicas.clone());
                }
                map.leaders.insert(topic.to_string(), new_leaders);
                map.replicas.insert(topic.to_string(), new_replicas);
                debug!(
                    topic = topic,
                    partitions = assignments.len(),
                    "Partition map refreshed"
                );
            }
            Err(e) => {
                warn!(topic = topic, error = %e, "Failed to refresh partition map, using cached");
            }
        }
    }

    /// Get the optimal set of peer nodes to query for a topic using ARS.
    ///
    /// For each partition NOT available locally, selects the replica peer
    /// with the lowest ARS score (fastest + least loaded). This minimizes
    /// fan-out while ensuring all partitions are covered.
    ///
    /// Falls back to all peers if partition map is empty.
    pub async fn nodes_for_topic(&self, topic: &str) -> Vec<&PeerNode> {
        let map = self.partition_map.read().await;

        if let Some(replicas) = map.replicas.get(topic) {
            if replicas.is_empty() {
                // Partition map exists but empty — fall through to all peers
                return self.peers.iter().collect();
            }

            let mut selected_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

            for partition_replicas in replicas.values() {
                // Skip partitions available locally (handled by local execution)
                if partition_replicas.contains(&self.self_node_id) {
                    continue;
                }

                // Pick the peer with lowest ARS score among replicas
                let best_peer_id = partition_replicas
                    .iter()
                    .filter(|&&nid| nid != self.self_node_id)
                    .min_by(|&&a, &&b| {
                        let score_a = self
                            .peer_metrics
                            .get(&a)
                            .map(|m| m.ars_score())
                            .unwrap_or(f64::MAX);
                        let score_b = self
                            .peer_metrics
                            .get(&b)
                            .map(|m| m.ars_score())
                            .unwrap_or(f64::MAX);
                        score_a
                            .partial_cmp(&score_b)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });

                if let Some(&peer_id) = best_peer_id {
                    selected_ids.insert(peer_id);
                }
            }

            // Return selected peers — may be empty if all partitions are local
            if !selected_ids.is_empty() {
                debug!(
                    topic = topic,
                    selected_peers = ?selected_ids,
                    total_peers = self.peers.len(),
                    "ARS selected peers for topic"
                );
            }
            return self
                .peers
                .iter()
                .filter(|p| selected_ids.contains(&p.node_id))
                .collect();
        }

        // Fallback: no partition info for topic — return all peers
        self.peers.iter().collect()
    }

    /// Check if all partitions for a topic are local (no fan-out needed).
    ///
    /// This is true when every partition has this node as a replica,
    /// meaning local execution returns complete results.
    pub async fn all_partitions_local(&self, topic: &str) -> bool {
        let map = self.partition_map.read().await;
        if let Some(replicas) = map.replicas.get(topic) {
            if replicas.is_empty() {
                return false; // No partition info, assume remote data exists
            }
            replicas
                .values()
                .all(|partition_replicas| partition_replicas.contains(&self.self_node_id))
        } else {
            false // No partition info for topic
        }
    }

    /// Check if the partition map has been populated (at least one topic).
    ///
    /// Used to avoid repeated RwLock writes under high concurrency — only
    /// refresh if the map is empty (not yet populated on first query).
    pub async fn has_partition_map(&self) -> bool {
        let map = self.partition_map.read().await;
        !map.replicas.is_empty()
    }

    /// Check if all partitions for ALL known topics are local (no fan-out needed).
    ///
    /// Returns true when every partition of every topic in the partition map
    /// has this node as a replica.
    ///
    /// # Do not use this to decide whether to skip a query fan-out
    ///
    /// It answers "am I a replica of everything", which at RF=node_count is
    /// always true and says nothing about whether this node can *answer* for
    /// everything. `/_sql` used it that way and returned partial,
    /// non-deterministic results (#22): a node's cold table is built from
    /// Parquet segment metadata registered by each partition's leader, filtered
    /// by whether those paths resolve on the local filesystem, so what any one
    /// node can see is a subset that varies by node.
    ///
    /// Use [`Self::leads_all_partitions`] instead — leadership partitions the
    /// work exactly once, which is what a distributed read needs.
    pub async fn all_topics_local(&self) -> bool {
        let map = self.partition_map.read().await;
        if map.replicas.is_empty() {
            return false;
        }
        for replicas in map.replicas.values() {
            for partition_replicas in replicas.values() {
                if !partition_replicas.contains(&self.self_node_id) {
                    return false;
                }
            }
        }
        true
    }

    /// Does this node lead every partition of every known topic?
    ///
    /// The safe condition for skipping a query fan-out: if this node leads
    /// everything, no peer can hold a partition it does not already answer for,
    /// so there is nothing to merge and nothing to double-count. True on a
    /// single node; false on any real cluster, where leadership is spread.
    pub async fn leads_all_partitions(&self) -> bool {
        let map = self.partition_map.read().await;
        if map.leaders.is_empty() {
            return false;
        }
        for leaders in map.leaders.values() {
            for leader in leaders.values() {
                if *leader != self.self_node_id {
                    return false;
                }
            }
        }
        true
    }

    /// Partitions of `topic` this node leads, and therefore answers for.
    ///
    /// `None` means "no opinion — serve everything", which is the correct answer
    /// when the topic is absent from the partition map: a topic that has not been
    /// mapped yet must not silently return zero rows, because a query that
    /// under-reports looks like data loss while one that over-reports on a
    /// single-node deployment is merely the pre-existing behaviour.
    pub async fn led_partitions(&self, topic: &str) -> Option<HashSet<i32>> {
        let map = self.partition_map.read().await;
        let leaders = map.leaders.get(topic)?;
        Some(
            leaders
                .iter()
                .filter(|(_, leader)| **leader == self.self_node_id)
                .map(|(partition, _)| *partition as i32)
                .collect(),
        )
    }

    /// Refresh partition maps for all topics from the metadata store.
    ///
    /// Used by SQL handler before checking `all_topics_local()` to ensure
    /// the partition map is current.
    pub async fn refresh_all_partition_maps(&self, metadata_store: &dyn MetadataStore) {
        if let Ok(topics) = metadata_store.list_topics().await {
            for topic in &topics {
                self.refresh_partition_map(metadata_store, &topic.name)
                    .await;
            }
        }
    }

    /// Get all peer nodes (for global queries like SQL that need all nodes).
    pub fn all_peers(&self) -> Vec<&PeerNode> {
        self.peers.iter().collect()
    }

    /// Get ARS scores for all peers (for monitoring and debugging).
    ///
    /// Returns (node_id, ars_score, latency_ewma_ms, outstanding) tuples.
    pub fn peer_ars_scores(&self) -> Vec<(u64, f64, f64, u64)> {
        self.peers
            .iter()
            .map(|p| {
                let (score, latency_ms, outstanding) = self
                    .peer_metrics
                    .get(&p.node_id)
                    .map(|m| {
                        (
                            m.ars_score(),
                            m.latency_ewma_us() as f64 / 1000.0,
                            m.outstanding(),
                        )
                    })
                    .unwrap_or((0.0, 0.0, 0));
                (p.node_id, score, latency_ms, outstanding)
            })
            .collect()
    }

    /// Fan out a POST request to target peer nodes and collect responses.
    ///
    /// - Sends the serialized request body to `{peer_url}{path}` with X-Chronik-Depth: 1
    /// - All peer requests run in parallel via tokio::spawn
    /// - Graceful degradation: failed peers are logged but don't fail the request
    /// - ARS metrics: tracks outstanding requests and records response latency
    /// - Returns Vec of successful peer responses
    pub async fn fan_out_post<Req, Resp>(
        &self,
        path: &str,
        request_body: &Req,
        target_nodes: &[&PeerNode],
    ) -> Vec<Resp>
    where
        Req: serde::Serialize + Send + Sync,
        Resp: serde::de::DeserializeOwned + Send + 'static,
    {
        if target_nodes.is_empty() {
            return Vec::new();
        }

        let body = match serde_json::to_vec(request_body) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "Failed to serialize fan-out request body");
                return Vec::new();
            }
        };

        let mut handles = Vec::with_capacity(target_nodes.len());

        for peer in target_nodes {
            let url = format!("{}{}", peer.url, path);
            let client = self.client.clone();
            let body = body.clone();
            let peer_id = peer.node_id;
            let metrics = self.peer_metrics.clone();

            // Track request start (ARS)
            if let Some(m) = metrics.get(&peer_id) {
                m.request_start();
            }

            let handle = tokio::spawn(async move {
                let start = Instant::now();
                let result = client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header(DEPTH_HEADER, "1")
                    .body(body)
                    .send()
                    .await;

                match result {
                    Ok(resp) if resp.status().is_success() => match resp.json::<Resp>().await {
                        Ok(parsed) => {
                            // Record success latency (ARS)
                            if let Some(m) = metrics.get(&peer_id) {
                                m.request_end(start.elapsed());
                            }
                            debug!(
                                peer_id = peer_id,
                                url = %url,
                                latency_ms = start.elapsed().as_millis(),
                                "Peer response received"
                            );
                            Some(parsed)
                        }
                        Err(e) => {
                            if let Some(m) = metrics.get(&peer_id) {
                                m.request_failed();
                            }
                            warn!(peer_id = peer_id, url = %url, error = %e, "Failed to parse peer response");
                            None
                        }
                    },
                    Ok(resp) => {
                        if let Some(m) = metrics.get(&peer_id) {
                            m.request_failed();
                        }
                        warn!(peer_id = peer_id, url = %url, status = %resp.status(), "Peer returned error");
                        None
                    }
                    Err(e) => {
                        if let Some(m) = metrics.get(&peer_id) {
                            m.request_failed();
                        }
                        warn!(peer_id = peer_id, url = %url, error = %e, "Peer request failed");
                        None
                    }
                }
            });

            handles.push(handle);
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Some(resp)) => results.push(resp),
                Ok(None) => {} // Already logged
                Err(e) => warn!(error = %e, "Peer task panicked"),
            }
        }

        results
    }
}

// ============================================================================
// Merge Functions — one per response type
// ============================================================================

/// Merge VectorSearchResponse results from multiple nodes.
///
/// Strategy: dedup by (partition, offset), keep highest score, sort, truncate to k.
/// Sum total_vectors across all nodes.
pub fn merge_vector_responses(
    local: VectorSearchResponse,
    peers: Vec<VectorSearchResponse>,
    k: usize,
) -> VectorSearchResponse {
    let mut best: HashMap<(i32, i64), VectorSearchResultItem> = HashMap::new();
    let mut total_vectors = local.total_vectors;
    let mut any_reranked = local.reranked;
    let model = local.model.clone();
    let execution_time_ms = local.execution_time_ms;

    // Insert local results
    for item in local.results {
        let key = (item.partition, item.offset);
        best.entry(key)
            .and_modify(|existing| {
                if item.score > existing.score {
                    *existing = item.clone();
                }
            })
            .or_insert(item);
    }

    // Insert peer results
    for peer_resp in peers {
        total_vectors += peer_resp.total_vectors;
        any_reranked = any_reranked || peer_resp.reranked;
        for item in peer_resp.results {
            let key = (item.partition, item.offset);
            best.entry(key)
                .and_modify(|existing| {
                    if item.score > existing.score {
                        *existing = item.clone();
                    }
                })
                .or_insert(item);
        }
    }

    let mut merged: Vec<VectorSearchResultItem> = best.into_values().collect();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(k);

    VectorSearchResponse {
        count: merged.len(),
        results: merged,
        total_vectors,
        execution_time_ms,
        reranked: any_reranked,
        model,
    }
}

/// Merge HybridSearchResponse results from multiple nodes.
///
/// Each node returns locally RRF-fused results. The coordinator deduplicates
/// by (partition, offset), keeps the highest score, sorts, and truncates.
pub fn merge_hybrid_responses(
    local: HybridSearchResponse,
    peers: Vec<HybridSearchResponse>,
    k: usize,
) -> HybridSearchResponse {
    let mut best: HashMap<(i32, i64), HybridSearchResultItem> = HashMap::new();
    let mut total_vector = local.vector_results_count;
    let mut total_text = local.text_results_count;
    let mut any_reranked = local.reranked;
    let model = local.model.clone();
    let execution_time_ms = local.execution_time_ms;

    for item in local.results {
        let key = (item.partition, item.offset);
        best.entry(key)
            .and_modify(|existing| {
                if item.score > existing.score {
                    *existing = item.clone();
                }
            })
            .or_insert(item);
    }

    for peer_resp in peers {
        total_vector += peer_resp.vector_results_count;
        total_text += peer_resp.text_results_count;
        any_reranked = any_reranked || peer_resp.reranked;
        for item in peer_resp.results {
            let key = (item.partition, item.offset);
            best.entry(key)
                .and_modify(|existing| {
                    if item.score > existing.score {
                        *existing = item.clone();
                    }
                })
                .or_insert(item);
        }
    }

    let mut merged: Vec<HybridSearchResultItem> = best.into_values().collect();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(k);

    HybridSearchResponse {
        count: merged.len(),
        results: merged,
        vector_results_count: total_vector,
        text_results_count: total_text,
        execution_time_ms,
        reranked: any_reranked,
        model,
    }
}

/// Merge SqlResponse results from multiple nodes.
///
/// Strategy: union all rows, truncate to limit.
/// KNOWN LIMITATION: aggregation queries (COUNT, SUM, GROUP BY) return
/// per-node partial results and cannot be correctly merged by row union.
pub fn merge_sql_responses(
    local: SqlResponse,
    peers: Vec<SqlResponse>,
    limit: usize,
    post: &PostMerge,
) -> SqlResponse {
    // #42: a projection (e.g. SELECT *) fanned across nodes can return zero rows
    // on the coordinator when the matching partitions live on peers — leaving
    // local.columns empty even though the peer rows are fully keyed. Carry the
    // first non-empty column set (schema order preserved) so the merged response
    // always describes its rows, instead of `columns: []` with populated `rows`.
    let mut columns = local.columns.clone();
    let execution_time_ms = local.execution_time_ms;
    let mut all_rows = local.rows;

    for peer_resp in peers {
        if columns.is_empty() && !peer_resp.columns.is_empty() {
            columns = peer_resp.columns.clone();
        }
        all_rows.extend(peer_resp.rows);
    }

    // Sort and limit across the union, before the transport cap: each node only
    // ordered and limited its own partitions, so this is where the query's
    // ORDER BY and LIMIT actually get applied (#55).
    apply_post_merge(&mut all_rows, post);

    let truncated = all_rows.len() > limit;
    all_rows.truncate(limit);
    let row_count = all_rows.len();

    SqlResponse {
        columns,
        rows: all_rows,
        row_count,
        execution_time_ms,
        truncated,
    }
}

/// How a fanned-out query's per-node results may be combined.
///
/// Concatenation is right for a projection — each node now serves a disjoint set
/// of partitions, so the rows do not overlap. It is **wrong** for an aggregate:
/// `SELECT COUNT(*)` fanned across three nodes returns three partial counts, and
/// concatenating them produces three rows where the caller asked for one number.
#[derive(Debug, PartialEq)]
pub enum SqlMerge {
    /// Rows from each node are disjoint; append them, then finish the query's
    /// `ORDER BY` / `LIMIT` over the combined rows.
    Concatenate(PostMerge),
    /// One row per node; combine column by column with these functions, in the
    /// order the columns appear.
    ScalarAggregate(Vec<AggMerge>),
    /// Grouped results: re-aggregate by group key across nodes. Runs parallel to
    /// the response columns — `None` is a group key, `Some(op)` an aggregate.
    GroupedAggregate {
        how: Vec<Option<AggMerge>>,
        post: PostMerge,
    },
    /// Cannot be combined without changing the query. Better to say so than to
    /// return a number that looks right.
    Unsupported(String),
}

/// What the coordinator still owes the query once every node's rows are in.
///
/// A fanned-out `ORDER BY ... LIMIT n` is not finished when the rows arrive:
/// each node sorted and limited only its own partitions, so the union is n rows
/// per node in per-node order. Sorting the union and taking n is what makes it
/// the answer — without it `ORDER BY id DESC LIMIT 5` over three nodes returned
/// 15 rows and its first five were not the top five (#55).
///
/// Taking the top n globally from each node's own top n is sound: a row in the
/// global top n is in its own node's top n, so no candidate is missing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PostMerge {
    /// Output columns to sort by, outermost first. Empty means the query did
    /// not ask for an order.
    pub order_by: Vec<OrderKey>,
    /// The query's own `LIMIT`, to apply once over the combined rows.
    pub limit: Option<usize>,
}

/// One `ORDER BY` term, resolved to the name of a column in the response.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    pub column: String,
    pub descending: bool,
    pub nulls_first: bool,
}

/// How one aggregate column combines across nodes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggMerge {
    Sum,
    Min,
    Max,
}

/// Every aggregate the query engine can evaluate, lower-cased, aliases included.
///
/// Read from DataFusion's own registry rather than kept by hand. The previous
/// matcher recognised four names and treated every other function as a row-wise
/// expression, so `median`, `stddev`, `approx_percentile_cont`, `array_agg` and
/// the rest were fanned out and their per-node results simply concatenated —
/// three rows where the caller asked for one number, returned as `200 OK`
/// (issue #55). A hand-kept list is how that happens again the next time
/// DataFusion gains an aggregate.
fn aggregate_function_names() -> &'static std::collections::HashSet<String> {
    static NAMES: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        use chronik_columnar::datafusion::functions_aggregate::all_default_aggregate_functions;
        let mut set = std::collections::HashSet::new();
        for f in all_default_aggregate_functions() {
            set.insert(f.name().to_ascii_lowercase());
            for alias in f.aliases() {
                set.insert(alias.to_ascii_lowercase());
            }
        }
        set
    })
}

/// What one projected column needs from the merge.
#[derive(Debug, Clone, PartialEq)]
enum ColumnMerge {
    /// Carries no aggregate: a group key, or an ordinary projected value.
    RowWise,
    /// A whole-column aggregate that combines exactly across disjoint inputs.
    Mergeable(AggMerge),
    /// Anything else, with the reason to hand back to the caller.
    Refuse(String),
}

/// Classify one projected expression for merging.
///
/// The rule is deliberately narrow: a column merges only when the expression is
/// *exactly* one mergeable aggregate. An aggregate buried inside arithmetic or
/// another call cannot be merged from its per-node results — `SUM(x)*2` fanned
/// across three nodes gives three doubled partials, and no combination of them
/// recovers the answer without re-deriving it from `SUM(x)` first.
fn classify_projection(expr: &SqlExprRef<'_>) -> ColumnMerge {
    use chronik_columnar::datafusion::sql::sqlparser::ast::Expr as SqlExpr;

    let calls = scan_aggregate_calls(expr.0);
    if calls.is_empty() {
        return ColumnMerge::RowWise;
    }

    if let Some(AggCall::Window(name)) = calls.iter().find(|c| matches!(c, AggCall::Window(_))) {
        return ColumnMerge::Refuse(format!(
            "{name}() OVER (...) is evaluated over one node's partitions only, so every node \
             restarts the window — ROW_NUMBER() returns 1, 2, 3 once per node."
        ));
    }

    if calls.len() > 1 {
        return ColumnMerge::Refuse(
            "an expression combining more than one aggregate cannot be merged across nodes: \
             each node returns its own partial of every aggregate, and the expression was \
             already applied to them."
                .to_string(),
        );
    }

    // Exactly one aggregate. It has to *be* the column, not sit inside it.
    match expr.0 {
        SqlExpr::Function(f) if f.over.is_none() && is_aggregate_call(f) => {
            let name = function_bare_name(f);
            if function_is_distinct(f) {
                return ColumnMerge::Refuse(format!(
                    "{name}(DISTINCT ...) cannot be merged across nodes: each node de-duplicates \
                     only its own partitions, so a value held by two nodes is counted twice."
                ));
            }
            match name.as_str() {
                "count" | "sum" => ColumnMerge::Mergeable(AggMerge::Sum),
                "min" => ColumnMerge::Mergeable(AggMerge::Min),
                "max" => ColumnMerge::Mergeable(AggMerge::Max),
                "avg" | "mean" => ColumnMerge::Refuse(
                    "AVG cannot be merged across nodes: averaging per-node averages is only \
                     correct when every node holds the same number of rows. Use SUM(x) and \
                     COUNT(x) as separate columns and divide in the client."
                        .to_string(),
                ),
                other => ColumnMerge::Refuse(format!(
                    "{other}(...) cannot be merged across nodes: it is not derivable from the \
                     per-node results, which would otherwise be returned one row per node."
                )),
            }
        }
        _ => ColumnMerge::Refuse(
            "an aggregate wrapped in an expression cannot be merged across nodes: the wrapping \
             was already applied to each node's partial. Project the bare aggregate and apply \
             the expression in the client."
                .to_string(),
        ),
    }
}

/// An aggregate or window call found inside an expression.
enum AggCall {
    Aggregate,
    Window(String),
}

/// Newtype so the helpers above can take a borrowed `Expr` without dragging the
/// sqlparser import into every signature.
struct SqlExprRef<'a>(&'a chronik_columnar::datafusion::sql::sqlparser::ast::Expr);

fn function_bare_name(f: &chronik_columnar::datafusion::sql::sqlparser::ast::Function) -> String {
    f.name
        .0
        .last()
        .map(|i| i.value.to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_aggregate_call(f: &chronik_columnar::datafusion::sql::sqlparser::ast::Function) -> bool {
    aggregate_function_names().contains(&function_bare_name(f))
}

fn function_is_distinct(f: &chronik_columnar::datafusion::sql::sqlparser::ast::Function) -> bool {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        DuplicateTreatment, FunctionArguments,
    };
    matches!(&f.args, FunctionArguments::List(list)
        if list.duplicate_treatment == Some(DuplicateTreatment::Distinct))
}

/// Find every aggregate and window call anywhere inside an expression.
///
/// Walks the whole tree rather than looking at the outermost node, because the
/// outermost node is exactly what `ROUND(AVG(x), 2)` hides behind: that query
/// used to slip past the AVG refusal and return one average per node.
fn scan_aggregate_calls(
    expr: &chronik_columnar::datafusion::sql::sqlparser::ast::Expr,
) -> Vec<AggCall> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{visit_expressions, Expr as SqlExpr};
    use std::ops::ControlFlow;

    let mut found = Vec::new();
    let _: ControlFlow<()> = visit_expressions(expr, |e| {
        if let SqlExpr::Function(f) = e {
            if f.over.is_some() {
                found.push(AggCall::Window(function_bare_name(f)));
            } else if is_aggregate_call(f) {
                found.push(AggCall::Aggregate);
            }
        }
        ControlFlow::Continue(())
    });
    found
}

/// Whether anything in this query is an aggregate or window call.
fn query_has_aggregate(query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query) -> bool {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{visit_expressions, Expr as SqlExpr};
    use std::ops::ControlFlow;

    let mut found = false;
    let _: ControlFlow<()> = visit_expressions(query, |e| {
        if let SqlExpr::Function(f) = e {
            if f.over.is_some() || is_aggregate_call(f) {
                found = true;
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    });
    found
}

/// Decide how to merge, from the query itself.
///
/// Deliberately conservative: anything not recognised as safely combinable is
/// [`SqlMerge::Unsupported`] rather than guessed at, because the alternative is
/// a plausible wrong number returned as `200 OK`. The previous version defaulted
/// the other way — an unrecognised projection was assumed to be row-wise — and
/// that default is what made `median`, `SUM(x)/COUNT(x)`, `COUNT(DISTINCT x)`
/// and `ROUND(AVG(x),2)` all answer wrongly and silently (#55).
pub fn sql_merge_strategy(sql: &str) -> SqlMerge {
    use chronik_columnar::datafusion::sql::parser::{DFParser, Statement};
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        GroupByExpr, SelectItem, SetExpr, Statement as AstStatement,
    };

    let Ok(statements) = DFParser::parse_sql(sql) else {
        return SqlMerge::Concatenate(PostMerge::default()); // unparseable here; the engine will reject it
    };
    let Some(Statement::Statement(ast)) = statements.front() else {
        return SqlMerge::Concatenate(PostMerge::default());
    };
    let AstStatement::Query(query) = ast.as_ref() else {
        return SqlMerge::Concatenate(PostMerge::default());
    };

    let SetExpr::Select(select) = query.body.as_ref() else {
        // UNION / INTERSECT / EXCEPT. Each operand was evaluated per node, so an
        // aggregate inside one of them arrives as partials that the set
        // operation has already been applied to.
        if query_has_aggregate(query) {
            return SqlMerge::Unsupported(
                "a set operation over aggregates cannot be merged across nodes: each node \
                 contributes its own partial of every branch."
                    .to_string(),
            );
        }
        if let SetExpr::SetOperation { set_quantifier, .. } = query.body.as_ref() {
            use chronik_columnar::datafusion::sql::sqlparser::ast::SetQuantifier;
            if !matches!(set_quantifier, SetQuantifier::All) {
                return SqlMerge::Unsupported(
                    "UNION/INTERSECT/EXCEPT de-duplicate, and each node can only de-duplicate \
                     its own partitions, so a row held by two nodes survives twice."
                        .to_string(),
                );
            }
        }
        return SqlMerge::Concatenate(PostMerge::default());
    };

    let grouped = match &select.group_by {
        GroupByExpr::Expressions(exprs, _) => !exprs.is_empty(),
        GroupByExpr::All(_) => true,
    };

    // A DISTINCT or GROUP BY inside a subquery has already been applied to each
    // node's own partitions before the outer query ran, so the outer result is
    // built on per-node de-duplication. `SELECT COUNT(*) FROM (SELECT DISTINCT
    // sev FROM t)` returned 9 on a three-node cluster where the answer was 3.
    if query_has_nested_dedup(query) {
        return SqlMerge::Unsupported(
            "a DISTINCT or GROUP BY inside a subquery cannot be merged across nodes: each node \
             de-duplicates only its own partitions, so the outer query counts the same value \
             once per node."
                .to_string(),
        );
    }

    // Two different topics cannot be combined by fanning out. Each node holds
    // its own partitions of both, and rows pair up by value, not by partition —
    // so a pair whose halves lead on different nodes is never formed anywhere.
    // Measured: a join of two 900-row topics on a shared id returned 316.
    //
    // A topic joined to itself is safe, because both sides of any pair are on
    // the same node by construction; that is why the self-join came back right
    // and hid this.
    if distinct_relation_count(query) > 1 {
        return SqlMerge::Unsupported(
            "a query over more than one topic cannot be merged across nodes: rows pair up by \
             value while the nodes are split by partition, so a pair whose sides lead on \
             different nodes is never matched. Query one topic per request and combine in the \
             client."
                .to_string(),
        );
    }

    // Classify every projected column.
    let mut columns: Vec<ColumnMerge> = Vec::new();
    for item in &select.projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) => expr,
            SelectItem::ExprWithAlias { expr, .. } => expr,
            // `*` is never an aggregate.
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                columns.push(ColumnMerge::RowWise);
                continue;
            }
        };
        columns.push(classify_projection(&SqlExprRef(expr)));
    }

    if let Some(ColumnMerge::Refuse(why)) =
        columns.iter().find(|c| matches!(c, ColumnMerge::Refuse(_)))
    {
        return SqlMerge::Unsupported(why.clone());
    }

    // A HAVING clause filters groups on each node's partial counts.
    if select.having.is_some() {
        return SqlMerge::Unsupported(
            "HAVING cannot be applied across nodes: each node would filter groups on its own \
             partial aggregate before the groups are combined."
                .to_string(),
        );
    }

    let any_agg = columns
        .iter()
        .any(|c| matches!(c, ColumnMerge::Mergeable(_)));

    if grouped || select.distinct.is_some() {
        // A LIMIT was applied by each node to its own groups, so a key that is
        // large overall but small on one node can be cut there before the groups
        // are combined — and the merged count is then short by whatever was cut.
        // Repairing that means running the peers without the LIMIT, which is a
        // different query from the one the caller sent.
        if query.limit.is_some() || query.offset.is_some() {
            return SqlMerge::Unsupported(
                "GROUP BY with LIMIT/OFFSET cannot be merged across nodes: each node applies the \
                 limit to its own groups, so a group that is small on one node can be dropped \
                 there before the groups are combined. Remove the LIMIT and page in the client."
                    .to_string(),
            );
        }

        let Some(order_by) = resolve_order_by(query, select) else {
            return SqlMerge::Unsupported(
                "ORDER BY cannot be applied across nodes unless every term is a projected \
                 column: the groups are ranked only once they have been combined, and the \
                 coordinator can only sort by values the query returned."
                    .to_string(),
            );
        };

        return SqlMerge::GroupedAggregate {
            how: columns
                .iter()
                .map(|c| match c {
                    ColumnMerge::Mergeable(op) => Some(*op),
                    _ => None,
                })
                .collect(),
            post: PostMerge {
                order_by,
                limit: None,
            },
        };
    }

    if !any_agg {
        // Each node applied the query's own OFFSET to its own rows, so the rows
        // it skipped are not the rows the query meant to skip. Unlike LIMIT,
        // that cannot be repaired from what came back — the skipped rows are
        // simply absent.
        if query.offset.is_some() {
            return SqlMerge::Unsupported(
                "OFFSET cannot be applied across nodes: each node skips that many of its own \
                 rows, so the rows the query meant to skip are missing from every node's \
                 result. Order and page in the client, or query a single node."
                    .to_string(),
            );
        }

        let Some(order_by) = resolve_order_by(query, select) else {
            return SqlMerge::Unsupported(
                "ORDER BY cannot be applied across nodes unless every term is a projected \
                 column: each node sorted only its own partitions, and the coordinator can \
                 only re-sort by values the query actually returned."
                    .to_string(),
            );
        };

        return SqlMerge::Concatenate(PostMerge {
            order_by,
            limit: query_limit(query),
        });
    }

    // Mixing aggregates with bare columns without GROUP BY is not valid SQL the
    // engine would accept, but if it did there is no sound merge.
    if columns.iter().any(|c| matches!(c, ColumnMerge::RowWise)) {
        return SqlMerge::Unsupported(
            "mixing aggregate and non-aggregate columns cannot be merged across nodes.".to_string(),
        );
    }

    SqlMerge::ScalarAggregate(
        columns
            .into_iter()
            .map(|c| match c {
                ColumnMerge::Mergeable(op) => op,
                _ => unreachable!("refusals and row-wise columns are handled above"),
            })
            .collect(),
    )
}

/// How many distinct real topics the query reads.
///
/// CTE names are excluded: `WITH a AS (SELECT ... FROM t) SELECT ... FROM a`
/// reads one topic, and counting `a` as a second would refuse a query that
/// merges correctly today.
fn distinct_relation_count(
    query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query,
) -> usize {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        visit_relations, Query, Visit, Visitor,
    };
    use std::collections::HashSet;
    use std::ops::ControlFlow;

    // Names bound by a WITH clause anywhere in the query.
    struct CteNames(HashSet<String>);
    impl Visitor for CteNames {
        type Break = ();
        fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
            if let Some(with) = q.with.as_ref() {
                for cte in &with.cte_tables {
                    self.0.insert(cte.alias.name.value.to_ascii_lowercase());
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut ctes = CteNames(HashSet::new());
    let _ = query.visit(&mut ctes);

    let mut names: HashSet<String> = HashSet::new();
    let _: ControlFlow<()> = visit_relations(query, |name| {
        let text = name
            .0
            .last()
            .map(|i| i.value.to_ascii_lowercase())
            .unwrap_or_default();
        if !ctes.0.contains(&text) {
            names.insert(text);
        }
        ControlFlow::Continue(())
    });
    names.len()
}
/// Whether any subquery de-duplicates: `DISTINCT`, `GROUP BY`, or a set
/// operation that is not `UNION ALL`.
///
/// Only nesting matters here — the outermost query's own `DISTINCT`/`GROUP BY`
/// is handled directly — so the outermost `Query` is skipped by depth.
fn query_has_nested_dedup(
    query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query,
) -> bool {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        GroupByExpr, Query, SetExpr, SetQuantifier, Visit, Visitor,
    };
    use std::ops::ControlFlow;

    struct Scan {
        depth: usize,
        found: bool,
    }
    impl Visitor for Scan {
        type Break = ();
        fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
            self.depth += 1;
            if self.depth == 1 {
                return ControlFlow::Continue(()); // the outer query, handled elsewhere
            }
            match q.body.as_ref() {
                SetExpr::Select(s) => {
                    let grouped = match &s.group_by {
                        GroupByExpr::Expressions(e, _) => !e.is_empty(),
                        GroupByExpr::All(_) => true,
                    };
                    if s.distinct.is_some() || grouped {
                        self.found = true;
                        return ControlFlow::Break(());
                    }
                }
                SetExpr::SetOperation { set_quantifier, .. } => {
                    if !matches!(set_quantifier, SetQuantifier::All) {
                        self.found = true;
                        return ControlFlow::Break(());
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _q: &Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
    }

    let mut scan = Scan {
        depth: 0,
        found: false,
    };
    let _ = query.visit(&mut scan);
    scan.found
}

/// The query's own `LIMIT`, if it is a plain number.
fn query_limit(query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query) -> Option<usize> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{Expr as SqlExpr, Value};
    match query.limit.as_ref()? {
        SqlExpr::Value(Value::Number(n, _)) => n.parse().ok(),
        _ => None,
    }
}
/// Resolve the query's `ORDER BY` terms to output column names.
///
/// Returns `None` when a term cannot be tied to a column that is actually in
/// the response — the coordinator only has the returned rows, so it cannot sort
/// by a value the query did not project.
fn resolve_order_by(
    query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query,
    select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select,
) -> Option<Vec<OrderKey>> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{Expr as SqlExpr, SelectItem, Value};

    let Some(order_by) = query.order_by.as_ref() else {
        return Some(Vec::new());
    };
    let exprs = &order_by.exprs;

    // Output name of each projected column, and the expression text it came
    // from, so `ORDER BY <same expression>` resolves as well as `ORDER BY alias`.
    let mut names: Vec<String> = Vec::new();
    let mut by_expr_text: HashMap<String, String> = HashMap::new();
    for item in &select.projection {
        match item {
            SelectItem::ExprWithAlias { expr, alias } => {
                names.push(alias.value.clone());
                by_expr_text.insert(expr.to_string(), alias.value.clone());
            }
            SelectItem::UnnamedExpr(expr) => {
                let text = expr.to_string();
                names.push(text.clone());
                by_expr_text.insert(text, expr.to_string());
            }
            // A wildcard's columns are not known from the SQL alone.
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => return None,
        }
    }

    let mut keys = Vec::new();
    for term in exprs {
        let column = match &term.expr {
            SqlExpr::Identifier(id) => {
                let want = id.value.clone();
                if names.iter().any(|n| n.eq_ignore_ascii_case(&want)) {
                    want
                } else {
                    by_expr_text.get(&term.expr.to_string())?.clone()
                }
            }
            // ORDER BY 2 — an ordinal into the projection.
            SqlExpr::Value(Value::Number(n, _)) => {
                let idx: usize = n.parse().ok()?;
                names.get(idx.checked_sub(1)?)?.clone()
            }
            other => by_expr_text.get(&other.to_string())?.clone(),
        };
        let descending = term.asc == Some(false);
        keys.push(OrderKey {
            // DataFusion and Postgres both default to NULLS LAST ascending and
            // NULLS FIRST descending; follow that unless the query says.
            nulls_first: term.nulls_first.unwrap_or(descending),
            descending,
            column,
        });
    }
    Some(keys)
}

/// Order two JSON values the way SQL would.
///
/// Values of different types are grouped rather than interleaved, so a column
/// that came back mixed still produces a stable, explicable order instead of a
/// comparison that depends on which row arrived first.
fn compare_json(a: &serde_json::Value, b: &serde_json::Value) -> std::cmp::Ordering {
    use serde_json::Value as J;
    use std::cmp::Ordering;

    fn rank(v: &J) -> u8 {
        match v {
            J::Bool(_) => 0,
            J::Number(_) => 1,
            J::String(_) => 2,
            J::Array(_) => 3,
            J::Object(_) => 4,
            J::Null => 5,
        }
    }

    match (a, b) {
        (J::Number(x), J::Number(y)) => x
            .as_f64()
            .unwrap_or(f64::NAN)
            .partial_cmp(&y.as_f64().unwrap_or(f64::NAN))
            .unwrap_or(Ordering::Equal),
        (J::String(x), J::String(y)) => x.cmp(y),
        (J::Bool(x), J::Bool(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

/// Finish a fanned-out query over the combined rows: sort, then limit.
pub fn apply_post_merge(rows: &mut Vec<HashMap<String, serde_json::Value>>, post: &PostMerge) {
    use serde_json::Value as J;
    use std::cmp::Ordering;

    if !post.order_by.is_empty() {
        rows.sort_by(|a, b| {
            for key in &post.order_by {
                let l = a.get(&key.column).unwrap_or(&J::Null);
                let r = b.get(&key.column).unwrap_or(&J::Null);
                let (l_null, r_null) = (l.is_null(), r.is_null());
                let ord = match (l_null, r_null) {
                    (true, true) => Ordering::Equal,
                    (true, false) => {
                        if key.nulls_first {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (false, true) => {
                        if key.nulls_first {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    (false, false) => {
                        let c = compare_json(l, r);
                        if key.descending {
                            c.reverse()
                        } else {
                            c
                        }
                    }
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
    }

    if let Some(limit) = post.limit {
        rows.truncate(limit);
    }
}
/// Combine one row per node into a single row of aggregates.
///
/// Each node computed its aggregate over the partitions it leads, so the parts
/// are disjoint and combining them is exact — a summed COUNT is the true count,
/// not an estimate.
pub fn merge_scalar_aggregate(
    local: SqlResponse,
    peers: Vec<SqlResponse>,
    how: &[AggMerge],
) -> SqlResponse {
    // Same self-consistency guard as the projection merge (#42): if the
    // coordinator produced no local aggregate row its columns are empty, so adopt
    // the first peer's column set — otherwise the aggregate loop below has nothing
    // to fold over.
    let mut columns = local.columns.clone();
    let execution_time_ms = local.execution_time_ms;

    let mut rows: Vec<HashMap<String, serde_json::Value>> = Vec::new();
    rows.extend(local.rows);
    for peer in peers {
        if columns.is_empty() && !peer.columns.is_empty() {
            columns = peer.columns.clone();
        }
        rows.extend(peer.rows);
    }

    let mut merged: HashMap<String, serde_json::Value> = HashMap::new();
    for (idx, column) in columns.iter().enumerate() {
        let op = how.get(idx).copied().unwrap_or(AggMerge::Sum);
        let mut acc: Option<serde_json::Value> = None;

        for row in &rows {
            let Some(value) = row.get(column) else {
                continue;
            };
            if value.is_null() {
                continue;
            }
            acc = Some(match acc {
                None => value.clone(),
                Some(current) => combine_numbers(&current, value, op),
            });
        }

        merged.insert(column.clone(), acc.unwrap_or(serde_json::Value::Null));
    }

    SqlResponse {
        columns,
        rows: vec![merged],
        row_count: 1,
        execution_time_ms,
        truncated: false,
    }
}

/// Combine grouped results from every node by re-aggregating on the group key.
///
/// Each node grouped only the partitions it leads, so a key present on three
/// nodes arrives as three partial groups. Summing the counts of matching keys —
/// and taking the extremum of MIN/MAX — reconstructs the group exactly, because
/// the nodes' inputs are disjoint.
///
/// `how` runs parallel to the response columns: `None` marks a group key,
/// `Some(op)` an aggregate and the way it combines.
pub fn merge_grouped_aggregate(
    local: SqlResponse,
    peers: Vec<SqlResponse>,
    how: &[Option<AggMerge>],
    post: &PostMerge,
    limit: usize,
) -> SqlResponse {
    // Same self-consistency guard as the other merges (#42): a coordinator that
    // matched no rows describes no columns, so adopt the first peer's schema.
    let mut columns = local.columns.clone();
    let execution_time_ms = local.execution_time_ms;

    let mut all_rows = local.rows;
    for peer in peers {
        if columns.is_empty() && !peer.columns.is_empty() {
            columns = peer.columns.clone();
        }
        all_rows.extend(peer.rows);
    }

    let key_columns: Vec<&String> = columns
        .iter()
        .enumerate()
        .filter(|(i, _)| how.get(*i).copied().flatten().is_none())
        .map(|(_, c)| c)
        .collect();

    // Group key -> merged row. Insertion order is kept so that a query without
    // ORDER BY still returns something stable rather than hash order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();

    for row in all_rows {
        // The rendered key values identify the group; JSON values are not
        // hashable, and rendering keeps 1 and "1" distinct.
        let identity = key_columns
            .iter()
            .map(|c| {
                row.get(c.as_str())
                    .unwrap_or(&serde_json::Value::Null)
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\u{1f}");

        match groups.get_mut(&identity) {
            None => {
                order.push(identity.clone());
                groups.insert(identity, row);
            }
            Some(existing) => {
                for (idx, column) in columns.iter().enumerate() {
                    let Some(op) = how.get(idx).copied().flatten() else {
                        continue; // a key column: already equal by construction
                    };
                    let Some(incoming) = row.get(column) else {
                        continue;
                    };
                    if incoming.is_null() {
                        continue;
                    }
                    let merged = match existing.get(column) {
                        Some(current) if !current.is_null() => {
                            combine_numbers(current, incoming, op)
                        }
                        _ => incoming.clone(),
                    };
                    existing.insert(column.clone(), merged);
                }
            }
        }
    }

    let mut rows: Vec<HashMap<String, serde_json::Value>> = order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .collect();

    // The query's ORDER BY ranks the combined groups, not each node's own.
    apply_post_merge(&mut rows, post);

    let truncated = rows.len() > limit;
    rows.truncate(limit);
    let row_count = rows.len();

    SqlResponse {
        columns,
        rows,
        row_count,
        execution_time_ms,
        truncated,
    }
}
/// Combine two JSON numbers. Non-numeric values keep the accumulator, since
/// there is no meaningful sum of two strings.
fn combine_numbers(
    a: &serde_json::Value,
    b: &serde_json::Value,
    op: AggMerge,
) -> serde_json::Value {
    // Integers stay integers: a summed COUNT rendered as 60.0 is a worse answer
    // than 60, and JSON has no separate integer type to fall back on.
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        let out = match op {
            AggMerge::Sum => x.saturating_add(y),
            AggMerge::Min => x.min(y),
            AggMerge::Max => x.max(y),
        };
        return serde_json::Value::from(out);
    }

    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        let out = match op {
            AggMerge::Sum => x + y,
            AggMerge::Min => x.min(y),
            AggMerge::Max => x.max(y),
        };
        return serde_json::Number::from_f64(out)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
    }

    a.clone()
}

/// Merge Elasticsearch-compatible SearchResponse results from multiple nodes.
///
/// Strategy: merge hits by _score, sum total hits and shard counts.
pub fn merge_search_responses(
    local: SearchResponse,
    peers: Vec<SearchResponse>,
    size: usize,
) -> SearchResponse {
    let mut all_hits = local.hits.hits;
    let mut total_value = local.hits.total.value;
    let mut shards_total = local._shards.total;
    let mut shards_successful = local._shards.successful;
    let mut shards_failed = local._shards.failed;

    for peer_resp in peers {
        total_value += peer_resp.hits.total.value;
        shards_total += peer_resp._shards.total;
        shards_successful += peer_resp._shards.successful;
        shards_failed += peer_resp._shards.failed;
        all_hits.extend(peer_resp.hits.hits);
    }

    // Deduplicate by (_index, _id), keeping highest score.
    // With RF=N, the same document exists on multiple nodes and would
    // otherwise appear N times in merged results, crowding out unique hits.
    let mut best: HashMap<(String, String), Hit> = HashMap::new();
    for hit in all_hits {
        let key = (hit._index.clone(), hit._id.clone());
        let score = hit._score.unwrap_or(0.0);
        match best.get(&key) {
            Some(existing) if existing._score.unwrap_or(0.0) >= score => {}
            _ => {
                best.insert(key, hit);
            }
        }
    }
    let mut all_hits: Vec<Hit> = best.into_values().collect();

    // Sort by score descending
    all_hits.sort_by(|a, b| {
        let sa = a._score.unwrap_or(0.0);
        let sb = b._score.unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    all_hits.truncate(size);

    let max_score = all_hits.first().and_then(|h| h._score);

    SearchResponse {
        took: local.took,
        timed_out: false,
        _shards: ShardInfo {
            total: shards_total,
            successful: shards_successful,
            skipped: 0,
            failed: shards_failed,
        },
        hits: HitsInfo {
            total: TotalHits {
                value: total_value,
                relation: "eq".to_string(),
            },
            max_score,
            hits: all_hits,
        },
        aggregations: local.aggregations,
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_forwarded_request_with_header() {
        let mut headers = HeaderMap::new();
        headers.insert(DEPTH_HEADER, "1".parse().unwrap());
        assert!(is_forwarded_request(&headers));
    }

    #[test]
    fn test_is_forwarded_request_high_depth() {
        let mut headers = HeaderMap::new();
        headers.insert(DEPTH_HEADER, "5".parse().unwrap());
        assert!(is_forwarded_request(&headers));
    }

    #[test]
    fn test_is_forwarded_request_without_header() {
        let headers = HeaderMap::new();
        assert!(!is_forwarded_request(&headers));
    }

    #[test]
    fn test_is_forwarded_request_depth_zero() {
        let mut headers = HeaderMap::new();
        headers.insert(DEPTH_HEADER, "0".parse().unwrap());
        assert!(!is_forwarded_request(&headers));
    }

    #[test]
    fn test_merge_vector_responses_dedup() {
        let local = VectorSearchResponse {
            results: vec![
                VectorSearchResultItem {
                    partition: 0,
                    offset: 100,
                    score: 0.9,
                    text_preview: None,
                },
                VectorSearchResultItem {
                    partition: 0,
                    offset: 200,
                    score: 0.8,
                    text_preview: None,
                },
            ],
            count: 2,
            total_vectors: 1000,
            execution_time_ms: 10,
            reranked: false,
            model: None,
        };

        let peer = VectorSearchResponse {
            results: vec![
                // Duplicate (0, 100) with lower score — should be dropped
                VectorSearchResultItem {
                    partition: 0,
                    offset: 100,
                    score: 0.7,
                    text_preview: None,
                },
                // New result from different partition
                VectorSearchResultItem {
                    partition: 1,
                    offset: 50,
                    score: 0.85,
                    text_preview: None,
                },
            ],
            count: 2,
            total_vectors: 2000,
            execution_time_ms: 15,
            reranked: false,
            model: None,
        };

        let merged = merge_vector_responses(local, vec![peer], 10);
        assert_eq!(merged.results.len(), 3); // 3 unique (partition, offset) pairs
        assert_eq!(merged.total_vectors, 3000); // 1000 + 2000
        assert_eq!(merged.results[0].score, 0.9); // Highest score first
        assert_eq!(merged.results[0].partition, 0);
        assert_eq!(merged.results[0].offset, 100);
        // (0, 100) kept score 0.9 (not 0.7)
    }

    #[test]
    fn test_merge_vector_responses_truncation() {
        let local = VectorSearchResponse {
            results: vec![
                VectorSearchResultItem {
                    partition: 0,
                    offset: 1,
                    score: 0.9,
                    text_preview: None,
                },
                VectorSearchResultItem {
                    partition: 0,
                    offset: 2,
                    score: 0.8,
                    text_preview: None,
                },
            ],
            count: 2,
            total_vectors: 100,
            execution_time_ms: 5,
            reranked: false,
            model: None,
        };

        let peer = VectorSearchResponse {
            results: vec![VectorSearchResultItem {
                partition: 1,
                offset: 1,
                score: 0.85,
                text_preview: None,
            }],
            count: 1,
            total_vectors: 200,
            execution_time_ms: 8,
            reranked: false,
            model: None,
        };

        let merged = merge_vector_responses(local, vec![peer], 2); // k=2
        assert_eq!(merged.results.len(), 2);
        assert_eq!(merged.results[0].score, 0.9);
        assert_eq!(merged.results[1].score, 0.85);
    }

    #[test]
    fn test_merge_hybrid_responses() {
        let local = HybridSearchResponse {
            results: vec![HybridSearchResultItem {
                partition: 0,
                offset: 10,
                score: 0.5,
                vector_rank: Some(1),
                text_rank: Some(2),
                text_preview: None,
            }],
            count: 1,
            vector_results_count: 5,
            text_results_count: 3,
            execution_time_ms: 20,
            reranked: false,
            model: None,
        };

        let peer = HybridSearchResponse {
            results: vec![HybridSearchResultItem {
                partition: 1,
                offset: 20,
                score: 0.7,
                vector_rank: Some(1),
                text_rank: None,
                text_preview: None,
            }],
            count: 1,
            vector_results_count: 8,
            text_results_count: 2,
            execution_time_ms: 25,
            reranked: false,
            model: None,
        };

        let merged = merge_hybrid_responses(local, vec![peer], 10);
        assert_eq!(merged.results.len(), 2);
        assert_eq!(merged.results[0].score, 0.7); // Higher score first
        assert_eq!(merged.vector_results_count, 13); // 5 + 8
        assert_eq!(merged.text_results_count, 5); // 3 + 2
    }

    #[test]
    fn test_merge_sql_responses_union() {
        let local = SqlResponse {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![{
                let mut row = HashMap::new();
                row.insert("id".to_string(), serde_json::json!(1));
                row.insert("name".to_string(), serde_json::json!("Alice"));
                row
            }],
            row_count: 1,
            execution_time_ms: 10,
            truncated: false,
        };

        let peer = SqlResponse {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![{
                let mut row = HashMap::new();
                row.insert("id".to_string(), serde_json::json!(2));
                row.insert("name".to_string(), serde_json::json!("Bob"));
                row
            }],
            row_count: 1,
            execution_time_ms: 12,
            truncated: false,
        };

        let merged = merge_sql_responses(local, vec![peer], 100, &PostMerge::default());
        assert_eq!(merged.row_count, 2);
        assert_eq!(merged.rows.len(), 2);
        assert!(!merged.truncated);
    }

    #[test]
    fn test_merge_sql_responses_truncation() {
        let local = SqlResponse {
            columns: vec!["id".to_string()],
            rows: vec![
                {
                    let mut row = HashMap::new();
                    row.insert("id".to_string(), serde_json::json!(1));
                    row
                },
                {
                    let mut row = HashMap::new();
                    row.insert("id".to_string(), serde_json::json!(2));
                    row
                },
            ],
            row_count: 2,
            execution_time_ms: 5,
            truncated: false,
        };

        let peer = SqlResponse {
            columns: vec!["id".to_string()],
            rows: vec![{
                let mut row = HashMap::new();
                row.insert("id".to_string(), serde_json::json!(3));
                row
            }],
            row_count: 1,
            execution_time_ms: 8,
            truncated: false,
        };

        let merged = merge_sql_responses(local, vec![peer], 2, &PostMerge::default()); // limit=2
        assert_eq!(merged.row_count, 2);
        assert!(merged.truncated);
    }

    // =========================================================================
    // ARS (Adaptive Replica Selection) Tests
    // =========================================================================

    #[test]
    fn test_peer_metrics_ewma_default() {
        let m = PeerMetrics::new();
        assert_eq!(m.latency_ewma_us(), 50_000); // 50ms default
        assert_eq!(m.outstanding(), 0);
    }

    #[test]
    fn test_peer_metrics_ewma_update() {
        let m = PeerMetrics::new();
        m.request_start();
        assert_eq!(m.outstanding(), 1);

        // Record a 10ms response
        m.request_end(Duration::from_millis(10));
        // EWMA: (3 * 10_000 + 7 * 50_000) / 10 = (30_000 + 350_000) / 10 = 38_000
        assert_eq!(m.latency_ewma_us(), 38_000);
        assert_eq!(m.outstanding(), 0);
    }

    #[test]
    fn test_peer_metrics_ars_score_no_outstanding() {
        let m = PeerMetrics::new();
        // 50ms, 0 outstanding → score = 50_000 * (1 + 0/10) = 50_000
        assert_eq!(m.ars_score(), 50_000.0);
    }

    #[test]
    fn test_peer_metrics_ars_score_with_outstanding() {
        let m = PeerMetrics::new();
        for _ in 0..10 {
            m.request_start();
        }
        // 50ms, 10 outstanding → score = 50_000 * (1 + 10/10) = 100_000
        assert_eq!(m.ars_score(), 100_000.0);
    }

    #[test]
    fn test_peer_metrics_failure_penalty() {
        let m = PeerMetrics::new();
        m.request_start();
        m.request_failed();
        // Penalty: double EWMA → 100_000
        assert_eq!(m.latency_ewma_us(), 100_000);
        assert_eq!(m.outstanding(), 0);
    }

    #[test]
    fn test_peer_metrics_failure_cap() {
        let m = PeerMetrics::new();
        // Repeatedly fail to hit the 10s cap
        for _ in 0..30 {
            m.request_start();
            m.request_failed();
        }
        // Should be capped at 10_000_000 (10s)
        assert_eq!(m.latency_ewma_us(), 10_000_000);
    }

    #[test]
    fn test_peer_metrics_ewma_convergence() {
        let m = PeerMetrics::new();
        // Simulate 20 responses at 20ms — EWMA should converge toward 20ms
        for _ in 0..20 {
            m.request_start();
            m.request_end(Duration::from_millis(20));
        }
        let ewma = m.latency_ewma_us();
        // After 20 iterations with alpha=0.3, should be within ~2ms of 20ms
        assert!(
            ewma > 19_000 && ewma < 22_000,
            "EWMA should converge to ~20ms, got {}us",
            ewma
        );
    }

    #[test]
    fn test_peer_metrics_fast_recovery() {
        let m = PeerMetrics::new();
        // Fail once (EWMA → 100ms)
        m.request_start();
        m.request_failed();
        assert_eq!(m.latency_ewma_us(), 100_000);

        // 10 fast responses at 10ms should recover
        for _ in 0..10 {
            m.request_start();
            m.request_end(Duration::from_millis(10));
        }
        let ewma = m.latency_ewma_us();
        // Should have recovered significantly from 100ms toward 10ms
        assert!(
            ewma < 40_000,
            "EWMA should recover toward 10ms after fast responses, got {}us",
            ewma
        );
    }

    #[tokio::test]
    async fn test_ars_selects_fastest_peer() {
        // Create a QueryRouter with 3 peers (self=2, peers=[1, 3])
        let config = ClusterConfig {
            enabled: true,
            node_id: 2,
            data_dir: "/tmp".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![
                chronik_config::NodeConfig {
                    id: 1,
                    kafka: "192.0.2.1:9092".to_string(),
                    wal: "192.0.2.1:9291".to_string(),
                    raft: "192.0.2.1:5001".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
                chronik_config::NodeConfig {
                    id: 2,
                    kafka: "192.0.2.2:9093".to_string(),
                    wal: "192.0.2.2:9292".to_string(),
                    raft: "192.0.2.2:5002".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
                chronik_config::NodeConfig {
                    id: 3,
                    kafka: "192.0.2.3:9094".to_string(),
                    wal: "192.0.2.3:9293".to_string(),
                    raft: "192.0.2.3:5003".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
            ],
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        let router = QueryRouter::new(&config);

        // Set up partition map: partition 0 is only on nodes 1 and 3 (not self=2)
        {
            let mut map = router.partition_map.write().await;
            let mut replicas = HashMap::new();
            replicas.insert(0_u32, vec![1_u64, 3_u64]); // Not on node 2
            replicas.insert(1_u32, vec![2_u64, 3_u64]); // On node 2 (local)
            map.replicas.insert("test_topic".to_string(), replicas);
        }

        // Make node 1 "slow" (high latency) and node 3 "fast" (low latency)
        if let Some(m) = router.peer_metrics.get(&1) {
            for _ in 0..10 {
                m.request_start();
                m.request_end(Duration::from_millis(100)); // 100ms
            }
        }
        if let Some(m) = router.peer_metrics.get(&3) {
            for _ in 0..10 {
                m.request_start();
                m.request_end(Duration::from_millis(5)); // 5ms
            }
        }

        let selected = router.nodes_for_topic("test_topic").await;
        // Should select only node 3 (fastest) for partition 0
        // Partition 1 is local, so no peer needed for it
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].node_id, 3);
    }

    #[tokio::test]
    async fn test_ars_all_local_no_fanout() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 2,
            data_dir: "/tmp".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![
                chronik_config::NodeConfig {
                    id: 1,
                    kafka: "192.0.2.1:9092".to_string(),
                    wal: "192.0.2.1:9291".to_string(),
                    raft: "192.0.2.1:5001".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
                chronik_config::NodeConfig {
                    id: 2,
                    kafka: "192.0.2.2:9093".to_string(),
                    wal: "192.0.2.2:9292".to_string(),
                    raft: "192.0.2.2:5002".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
                chronik_config::NodeConfig {
                    id: 3,
                    kafka: "192.0.2.3:9094".to_string(),
                    wal: "192.0.2.3:9293".to_string(),
                    raft: "192.0.2.3:5003".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
            ],
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        let router = QueryRouter::new(&config);

        // All partitions include self (node 2) — no fan-out needed
        {
            let mut map = router.partition_map.write().await;
            let mut replicas = HashMap::new();
            replicas.insert(0_u32, vec![1_u64, 2_u64, 3_u64]);
            replicas.insert(1_u32, vec![2_u64, 3_u64, 1_u64]);
            replicas.insert(2_u32, vec![3_u64, 1_u64, 2_u64]);
            map.replicas.insert("test_topic".to_string(), replicas);
        }

        let selected = router.nodes_for_topic("test_topic").await;
        // All partitions are local — empty selection (no peers needed)
        assert!(
            selected.is_empty(),
            "Should not select any peers when all partitions are local"
        );
    }

    #[test]
    fn test_peer_ars_scores_reporting() {
        let config = ClusterConfig {
            enabled: true,
            node_id: 2,
            data_dir: "/tmp".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![
                chronik_config::NodeConfig {
                    id: 1,
                    kafka: "192.0.2.1:9092".to_string(),
                    wal: "192.0.2.1:9291".to_string(),
                    raft: "192.0.2.1:5001".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
                chronik_config::NodeConfig {
                    id: 2,
                    kafka: "192.0.2.2:9093".to_string(),
                    wal: "192.0.2.2:9292".to_string(),
                    raft: "192.0.2.2:5002".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
                chronik_config::NodeConfig {
                    id: 3,
                    kafka: "192.0.2.3:9094".to_string(),
                    wal: "192.0.2.3:9293".to_string(),
                    raft: "192.0.2.3:5003".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
            ],
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        let router = QueryRouter::new(&config);
        let scores = router.peer_ars_scores();
        assert_eq!(scores.len(), 2); // Peers 1 and 3 (self=2 excluded)

        // Initial scores should be (50_000, 50.0ms, 0 outstanding)
        for (node_id, score, latency_ms, outstanding) in &scores {
            assert!(*node_id == 1 || *node_id == 3);
            assert_eq!(*score, 50_000.0);
            assert_eq!(*latency_ms, 50.0);
            assert_eq!(*outstanding, 0);
        }
    }

    #[test]
    fn test_peer_url_computation() {
        // Simulate ClusterConfig peers
        let config = ClusterConfig {
            enabled: true,
            node_id: 2,
            data_dir: "/tmp".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![
                chronik_config::NodeConfig {
                    id: 1,
                    kafka: "192.0.2.1:9092".to_string(),
                    wal: "192.0.2.1:9291".to_string(),
                    raft: "192.0.2.1:5001".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
                chronik_config::NodeConfig {
                    id: 2,
                    kafka: "192.0.2.2:9093".to_string(),
                    wal: "192.0.2.2:9292".to_string(),
                    raft: "192.0.2.2:5002".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
                chronik_config::NodeConfig {
                    id: 3,
                    kafka: "192.0.2.3:9094".to_string(),
                    wal: "192.0.2.3:9293".to_string(),
                    raft: "192.0.2.3:5003".to_string(),
                    replication: None,
                    addr: None,
                    raft_port: None,
                },
            ],
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };

        // Defensively isolate this test from stray env vars that may have
        // been set by a previous test in the suite (the new resolution
        // order in `QueryRouter::new` consults env first).
        std::env::remove_var("CHRONIK_UNIFIED_API_PORT");
        std::env::remove_var("CHRONIK_UNIFIED_API_PORT_NODE_1");
        std::env::remove_var("CHRONIK_UNIFIED_API_PORT_NODE_3");

        let router = QueryRouter::new(&config);
        assert_eq!(router.peers.len(), 2); // Excludes self (node 2)
        assert_eq!(router.peers[0].node_id, 1);
        // Per-peer port = 6091 + peer.id, matches main.rs's bind logic.
        // Previously this asserted 6092 for every peer — that was the bug
        // that hid a third of the data on every distributed search in
        // multi-port single-host configs (and was wrong in multi-host
        // configs too, just less visibly because hosts differ).
        assert_eq!(router.peers[0].url, "http://192.0.2.1:6092");
        assert_eq!(router.peers[1].node_id, 3);
        assert_eq!(router.peers[1].url, "http://192.0.2.3:6094");
        assert_eq!(router.self_node_id, 2);
    }

    #[test]
    fn test_peer_url_respects_per_node_env_override() {
        // Operator escape hatch: per-peer port via CHRONIK_UNIFIED_API_PORT_NODE_{id}
        // overrides the offset-derived default. Useful when a deployment
        // moves a node to a non-default port.
        //
        // Uses a high node-id (99) that no other test references, so it's
        // safe to run in parallel without `serial_test` — the env var name
        // contains the node id and won't clash with the default-port test.
        std::env::set_var("CHRONIK_UNIFIED_API_PORT_NODE_99", "16099");
        let config = ClusterConfig {
            enabled: true,
            node_id: 1,
            data_dir: "/tmp".to_string(),
            replication_factor: 3,
            min_insync_replicas: 2,
            peers: vec![chronik_config::NodeConfig {
                id: 99,
                kafka: "remote-host:9092".to_string(),
                wal: "remote-host:9291".to_string(),
                raft: "remote-host:5001".to_string(),
                replication: None,
                addr: None,
                raft_port: None,
            }],
            bind: None,
            advertise: None,
            gossip: None,
            auto_recover: true,
        };
        let router = QueryRouter::new(&config);
        assert_eq!(router.peers.len(), 1);
        assert_eq!(router.peers[0].url, "http://remote-host:16099");
        std::env::remove_var("CHRONIK_UNIFIED_API_PORT_NODE_99");
    }
}

#[cfg(test)]
mod partition_ownership_tests {
    use super::*;

    /// A router with a known partition map and no peers to talk to.
    fn router_with(self_node_id: u64, leaders: &[(&str, &[(u32, u64)])]) -> QueryRouter {
        let mut map = PartitionMap::default();
        for (topic, entries) in leaders {
            let per_topic: HashMap<u32, u64> = entries.iter().copied().collect();
            // Replicas: everyone replicates everything (RF = node_count), which
            // is the configuration #22 is about.
            let replicas: HashMap<u32, Vec<u64>> =
                entries.iter().map(|(p, _)| (*p, vec![1, 2, 3])).collect();
            map.leaders.insert(topic.to_string(), per_topic);
            map.replicas.insert(topic.to_string(), replicas);
        }

        QueryRouter {
            client: Client::new(),
            peers: Vec::new(),
            self_node_id,
            partition_map: Arc::new(RwLock::new(map)),
            config: QueryRouterConfig::default(),
            peer_metrics: Arc::new(HashMap::new()),
        }
    }

    /// #22, stated as a test: at RF=node_count every node is a replica of every
    /// partition, so the old predicate said "everything is local, skip the
    /// fan-out" — on every node. Each then answered from whatever it could see
    /// locally, which is a partial and node-dependent subset.
    #[tokio::test]
    async fn replica_based_locality_is_true_for_every_node_at_full_replication() {
        let leaders: &[(u32, u64)] = &[(0, 1), (1, 2), (2, 3)];

        for node in [1u64, 2, 3] {
            let router = router_with(node, &[("orders", leaders)]);
            assert!(
                router.all_topics_local().await,
                "node {} is a replica of every partition, which is why this predicate \
                 cannot decide whether to fan out",
                node
            );
            assert!(
                !router.leads_all_partitions().await,
                "node {} leads only one of three partitions and must fan out",
                node
            );
        }
    }

    /// The three nodes' led sets must partition the topic: every partition
    /// covered, none twice. That is what makes the merged result correct.
    #[tokio::test]
    async fn led_partitions_cover_every_partition_exactly_once() {
        let leaders: &[(u32, u64)] = &[(0, 1), (1, 2), (2, 3), (3, 1)];

        let mut seen: Vec<i32> = Vec::new();
        for node in [1u64, 2, 3] {
            let router = router_with(node, &[("orders", leaders)]);
            let led = router
                .led_partitions("orders")
                .await
                .expect("topic is mapped");
            seen.extend(led);
        }

        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![0, 1, 2, 3],
            "partitions must be covered exactly once"
        );
    }

    /// A single node leads everything, so there is nothing to merge and no
    /// reason to pay for a fan-out.
    #[tokio::test]
    async fn a_lone_node_skips_the_fan_out() {
        let router = router_with(1, &[("orders", &[(0, 1), (1, 1), (2, 1)])]);
        assert!(router.leads_all_partitions().await);
    }

    /// An unmapped topic means "no opinion — serve everything", not "serve
    /// nothing". Returning an empty set here would make a query silently
    /// under-report while the map was still being populated, which is
    /// indistinguishable from data loss.
    #[tokio::test]
    async fn an_unmapped_topic_has_no_ownership_opinion() {
        let router = router_with(1, &[("orders", &[(0, 1)])]);
        assert!(router.led_partitions("not-a-topic").await.is_none());
    }

    /// With no map at all, neither predicate may claim the node can answer
    /// alone — a fresh node must not decide to skip the fan-out before it knows
    /// anything.
    #[tokio::test]
    async fn an_empty_map_never_claims_to_lead_everything() {
        let router = router_with(1, &[]);
        assert!(!router.leads_all_partitions().await);
        assert!(!router.all_topics_local().await);
    }
}

#[cfg(test)]
mod sql_merge_tests {
    use super::*;

    fn row(pairs: &[(&str, i64)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::from(*v)))
            .collect()
    }

    fn resp(columns: &[&str], rows: Vec<HashMap<String, serde_json::Value>>) -> SqlResponse {
        SqlResponse {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            row_count: rows.len(),
            rows,
            execution_time_ms: 1,
            truncated: false,
        }
    }

    #[test]
    fn a_projection_is_concatenated() {
        assert_eq!(
            sql_merge_strategy("SELECT _offset, _value FROM orders"),
            SqlMerge::Concatenate(PostMerge::default())
        );
        assert_eq!(
            sql_merge_strategy("SELECT * FROM orders"),
            SqlMerge::Concatenate(PostMerge::default())
        );
    }

    /// #42: a fanned-out `SELECT *` can match zero rows on the coordinator (the
    /// partitions live on peers), so its `columns` is empty. The merge must
    /// recover the column set from a peer instead of returning populated `rows`
    /// with `columns: []`.
    #[test]
    fn projection_merge_recovers_columns_from_a_peer_when_local_is_empty() {
        let local = resp(&[], vec![]); // coordinator held no matching rows
        let peer = resp(
            &["_offset", "_value"],
            vec![row(&[("_offset", 7), ("_value", 42)])],
        );
        let merged = merge_sql_responses(local, vec![peer], 100, &PostMerge::default());
        assert_eq!(
            merged.columns,
            vec!["_offset", "_value"],
            "#42: merged columns must be recovered from the peer, not left empty"
        );
        assert_eq!(merged.row_count, 1);
    }

    /// Same guard on the aggregate path: an empty local column set must not
    /// silently drop the peer's aggregate.
    #[test]
    fn aggregate_merge_recovers_columns_when_local_is_empty() {
        let local = resp(&[], vec![]);
        let peer = resp(&["n"], vec![row(&[("n", 5)])]);
        let merged = merge_scalar_aggregate(local, vec![peer], &[AggMerge::Sum]);
        assert_eq!(merged.columns, vec!["n"]);
        assert_eq!(merged.rows[0].get("n"), Some(&serde_json::Value::from(5)));
    }

    #[test]
    fn count_and_sum_are_summed_min_and_max_are_extrema() {
        assert_eq!(
            sql_merge_strategy("SELECT COUNT(*) FROM orders"),
            SqlMerge::ScalarAggregate(vec![AggMerge::Sum])
        );
        assert_eq!(
            sql_merge_strategy("SELECT SUM(amount), MIN(amount), MAX(amount) FROM orders"),
            SqlMerge::ScalarAggregate(vec![AggMerge::Sum, AggMerge::Min, AggMerge::Max])
        );
    }

    /// The whole point of this analysis: three partial counts must become one
    /// total, not three rows.
    #[test]
    fn partial_counts_become_one_total() {
        let local = resp(&["c"], vec![row(&[("c", 0)])]);
        let peers = vec![
            resp(&["c"], vec![row(&[("c", 59)])]),
            resp(&["c"], vec![row(&[("c", 1)])]),
        ];

        let merged = merge_scalar_aggregate(local, peers, &[AggMerge::Sum]);
        assert_eq!(merged.row_count, 1, "a scalar aggregate returns one row");
        assert_eq!(merged.rows[0]["c"], serde_json::Value::from(60));
    }

    /// A summed COUNT must stay an integer — 60, not 60.0.
    #[test]
    fn summed_counts_stay_integers() {
        let merged = merge_scalar_aggregate(
            resp(&["c"], vec![row(&[("c", 2)])]),
            vec![resp(&["c"], vec![row(&[("c", 3)])])],
            &[AggMerge::Sum],
        );
        assert!(
            merged.rows[0]["c"].is_i64(),
            "got {:?}",
            merged.rows[0]["c"]
        );
        assert_eq!(merged.rows[0]["c"], serde_json::Value::from(5));
    }

    #[test]
    fn min_and_max_pick_extremes_not_sums() {
        let local = resp(&["lo", "hi"], vec![row(&[("lo", 5), ("hi", 5)])]);
        let peers = vec![resp(&["lo", "hi"], vec![row(&[("lo", 2), ("hi", 9)])])];
        let merged = merge_scalar_aggregate(local, peers, &[AggMerge::Min, AggMerge::Max]);
        assert_eq!(merged.rows[0]["lo"], serde_json::Value::from(2));
        assert_eq!(merged.rows[0]["hi"], serde_json::Value::from(9));
    }

    /// AVG must be refused, not approximated.
    ///
    /// The average of per-node averages equals the true average only when every
    /// node holds the same number of rows — which a partitioned topic does not
    /// guarantee. Returning a number here would be wrong in a way nobody can see.
    #[test]
    fn avg_is_refused_rather_than_approximated() {
        match sql_merge_strategy("SELECT AVG(amount) FROM orders") {
            SqlMerge::Unsupported(why) => assert!(why.contains("AVG")),
            other => panic!("AVG must not be merged: {:?}", other),
        }
    }

    /// GROUP BY is re-aggregated by key across nodes: the same key arrives once
    /// per node and the partial groups are combined.
    #[test]
    fn group_by_is_re_aggregated_by_key() {
        let SqlMerge::GroupedAggregate { how, .. } =
            sql_merge_strategy("SELECT k, COUNT(*) AS c FROM orders GROUP BY k")
        else {
            panic!("GROUP BY should now merge by key");
        };
        assert_eq!(
            how,
            vec![None, Some(AggMerge::Sum)],
            "k is a key, c a count"
        );

        // Three nodes, each having grouped only its own partitions.
        let local = resp(&["k", "c"], vec![row(&[("k", 1), ("c", 10)])]);
        let peers = vec![
            resp(
                &["k", "c"],
                vec![row(&[("k", 1), ("c", 5)]), row(&[("k", 2), ("c", 7)])],
            ),
            resp(&["k", "c"], vec![row(&[("k", 2), ("c", 3)])]),
        ];
        let merged = merge_grouped_aggregate(local, peers, &how, &PostMerge::default(), 1000);
        assert_eq!(merged.rows.len(), 2, "two distinct keys, not four partials");

        let mut counts: Vec<(i64, i64)> = merged
            .rows
            .iter()
            .map(|r| (r["k"].as_i64().unwrap(), r["c"].as_i64().unwrap()))
            .collect();
        counts.sort();
        assert_eq!(counts, vec![(1, 15), (2, 10)]);
    }

    /// DISTINCT is the same merge with every column a key, so a value held by
    /// two nodes collapses to one row instead of surviving twice.
    #[test]
    fn distinct_collapses_values_held_by_more_than_one_node() {
        let SqlMerge::GroupedAggregate { how, .. } =
            sql_merge_strategy("SELECT DISTINCT k FROM orders")
        else {
            panic!("DISTINCT should now merge by key");
        };
        assert_eq!(how, vec![None]);

        let local = resp(&["k"], vec![row(&[("k", 1)])]);
        let peers = vec![
            resp(&["k"], vec![row(&[("k", 1)]), row(&[("k", 2)])]),
            resp(&["k"], vec![row(&[("k", 1)])]),
        ];
        let merged = merge_grouped_aggregate(local, peers, &how, &PostMerge::default(), 1000);
        assert_eq!(merged.rows.len(), 2, "k=1 was held by all three nodes");
    }

    /// A per-node LIMIT can drop a group on one node before the groups are
    /// combined, which is not recoverable from what came back.
    #[test]
    fn group_by_with_a_limit_is_refused() {
        assert!(
            refusal("SELECT k, COUNT(*) AS c FROM t GROUP BY k ORDER BY c DESC LIMIT 10")
                .contains("LIMIT")
        );
    }

    /// Grouping still refuses HAVING: each node would filter on its own partial.
    #[test]
    fn group_by_with_having_is_refused() {
        assert!(
            refusal("SELECT k, COUNT(*) AS c FROM t GROUP BY k HAVING COUNT(*) > 5")
                .contains("HAVING")
        );
    }

    /// A null from a node that holds no rows must not erase another node's value.
    #[test]
    fn a_node_with_no_rows_does_not_erase_the_answer() {
        let local = resp(
            &["c"],
            vec![HashMap::from([("c".to_string(), serde_json::Value::Null)])],
        );
        let peers = vec![resp(&["c"], vec![row(&[("c", 7)])])];
        let merged = merge_scalar_aggregate(local, peers, &[AggMerge::Sum]);
        assert_eq!(merged.rows[0]["c"], serde_json::Value::from(7));
    }

    // ---- #55: everything below returned a wrong answer as 200 OK, measured on
    // a real three-node cluster over 900 rows whose true values were known.

    fn refusal(sql: &str) -> String {
        match sql_merge_strategy(sql) {
            SqlMerge::Unsupported(why) => why,
            other => panic!("expected a refusal for {sql}, got {other:?}"),
        }
    }

    /// `COUNT(DISTINCT sev)` returned **9** where the answer was 3: the matcher
    /// keyed on the function name and never looked at the DISTINCT, so three
    /// nodes' de-duplicated counts were summed.
    #[test]
    fn count_distinct_is_refused_rather_than_summed() {
        assert!(refusal("SELECT COUNT(DISTINCT sev) AS c FROM t").contains("DISTINCT"));
    }

    /// `median`, `stddev`, `approx_percentile_cont`, `array_agg` … every
    /// aggregate the old four-name matcher did not know was treated as a
    /// row-wise expression and its per-node results concatenated — one row per
    /// node where the caller asked for one number.
    #[test]
    fn aggregates_that_cannot_be_combined_are_refused() {
        for sql in [
            "SELECT median(x) AS m FROM t",
            "SELECT stddev(x) AS s FROM t",
            "SELECT approx_percentile_cont(x, 0.95) AS p FROM t",
            "SELECT approx_distinct(x) AS d FROM t",
            "SELECT array_agg(x) AS a FROM t",
            "SELECT corr(x, y) AS c FROM t",
            "SELECT first_value(x) AS f FROM t",
        ] {
            let _ = refusal(sql);
        }
    }

    /// The sharpest one: AVG was correctly refused, and wrapping it in any
    /// function walked straight past that refusal. `ROUND(AVG(n),2)` returned
    /// 8.54, 8.63, 8.31 for a column whose average was 8.5.
    #[test]
    fn wrapping_an_aggregate_does_not_evade_the_refusal() {
        assert!(refusal("SELECT ROUND(AVG(x), 2) AS a FROM t").contains("wrapped"));
        assert!(refusal("SELECT SUM(x) * 2 AS s FROM t").contains("wrapped"));
        assert!(refusal("SELECT CAST(SUM(x) AS BIGINT) AS s FROM t").contains("wrapped"));
    }

    /// `SUM(x)/COUNT(x)` is what the AVG refusal tells the caller to do, and it
    /// was itself unguarded: three nodes, three ratios, none of them the answer.
    #[test]
    fn the_avg_workaround_is_not_itself_silently_wrong() {
        let _ = refusal("SELECT SUM(x)/COUNT(x) AS r FROM t");
        let _ = refusal("SELECT SUM(x) + SUM(y) AS s FROM t");
    }

    /// `ROW_NUMBER() OVER (ORDER BY id)` returned 1,2,3,4,5 once per node.
    #[test]
    fn window_functions_are_refused() {
        assert!(refusal("SELECT ROW_NUMBER() OVER (ORDER BY ts) AS rn FROM t").contains("OVER"));
    }

    /// `SELECT COUNT(*) FROM (SELECT DISTINCT sev FROM t)` returned 9 where the
    /// answer was 3 — the de-duplication happened per node, one level down.
    #[test]
    fn a_dedup_inside_a_subquery_is_refused() {
        assert!(
            refusal("SELECT COUNT(*) AS c FROM (SELECT DISTINCT k FROM t)").contains("subquery")
        );
    }

    /// `SELECT COUNT(*) FROM t UNION ALL SELECT COUNT(*) FROM t` returned six
    /// rows of per-node partials instead of two totals.
    #[test]
    fn a_set_operation_over_aggregates_is_refused() {
        let _ = refusal("SELECT COUNT(*) AS c FROM t UNION ALL SELECT COUNT(*) AS c FROM u");
        // UNION (no ALL) de-duplicates, and each node can only de-duplicate its own.
        let _ = refusal("SELECT k FROM t UNION SELECT k FROM u");
        // …but UNION ALL of plain projections is still just concatenation.
        assert!(matches!(
            sql_merge_strategy("SELECT k FROM t UNION ALL SELECT k FROM u"),
            SqlMerge::Concatenate(_)
        ));
    }

    /// Per-node OFFSET skips the wrong rows and they cannot be recovered from
    /// what came back.
    #[test]
    fn offset_is_refused_under_fan_out() {
        assert!(refusal("SELECT k FROM t ORDER BY k LIMIT 5 OFFSET 10").contains("OFFSET"));
    }

    /// The coordinator can only sort by values the query actually returned.
    #[test]
    fn ordering_by_an_unprojected_column_is_refused() {
        assert!(refusal("SELECT k FROM t ORDER BY ts DESC LIMIT 5").contains("ORDER BY"));
        // A wildcard hides which columns exist, so it cannot be resolved either.
        assert!(refusal("SELECT * FROM t ORDER BY ts DESC LIMIT 5").contains("ORDER BY"));
    }

    /// `ORDER BY id DESC LIMIT 5` over three nodes returned 15 rows whose first
    /// five were 900, 899, 895, 889, 871 — the true top five were 900..896.
    /// The order and the limit belong to the union, not to each node.
    #[test]
    fn order_by_and_limit_are_applied_to_the_merged_rows() {
        let strategy = sql_merge_strategy("SELECT id FROM t ORDER BY id DESC LIMIT 5");
        let SqlMerge::Concatenate(post) = strategy else {
            panic!("a projection with ORDER BY should still concatenate");
        };
        assert_eq!(post.limit, Some(5));
        assert_eq!(post.order_by.len(), 1);
        assert_eq!(post.order_by[0].column, "id");
        assert!(post.order_by[0].descending);

        // Three nodes, each already sorted and limited to its own top rows.
        let local = resp(&["id"], vec![row(&[("id", 900)]), row(&[("id", 895)])]);
        let peers = vec![
            resp(&["id"], vec![row(&[("id", 899)]), row(&[("id", 893)])]),
            resp(&["id"], vec![row(&[("id", 898)]), row(&[("id", 897)])]),
        ];
        let merged = merge_sql_responses(local, peers, 1000, &post);
        let ids: Vec<i64> = merged
            .rows
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![900, 899, 898, 897, 895]);
    }

    /// A LIMIT with no ORDER BY still has to mean "at most n rows", not n per node.
    #[test]
    fn a_limit_without_an_order_still_bounds_the_merged_rows() {
        let SqlMerge::Concatenate(post) = sql_merge_strategy("SELECT id FROM t LIMIT 5") else {
            panic!("expected concatenation");
        };
        let local = resp(&["id"], vec![row(&[("id", 1)]), row(&[("id", 2)])]);
        let peers = vec![
            resp(&["id"], vec![row(&[("id", 3)]), row(&[("id", 4)])]),
            resp(&["id"], vec![row(&[("id", 5)]), row(&[("id", 6)])]),
        ];
        let merged = merge_sql_responses(local, peers, 1000, &post);
        assert_eq!(
            merged.rows.len(),
            5,
            "LIMIT 5 must not return 5 rows per node"
        );
    }

    /// Ascending order puts NULLs last, matching DataFusion and Postgres.
    #[test]
    fn nulls_sort_last_ascending_and_first_descending() {
        let mut rows = vec![
            HashMap::from([("v".to_string(), serde_json::Value::Null)]),
            HashMap::from([("v".to_string(), serde_json::Value::from(2))]),
            HashMap::from([("v".to_string(), serde_json::Value::from(1))]),
        ];
        apply_post_merge(
            &mut rows,
            &PostMerge {
                order_by: vec![OrderKey {
                    column: "v".into(),
                    descending: false,
                    nulls_first: false,
                }],
                limit: None,
            },
        );
        assert_eq!(rows[0]["v"], serde_json::Value::from(1));
        assert_eq!(rows[1]["v"], serde_json::Value::from(2));
        assert!(rows[2]["v"].is_null());
    }

    /// The aggregates that genuinely do combine must keep working.
    #[test]
    fn the_mergeable_aggregates_are_untouched() {
        assert_eq!(
            sql_merge_strategy("SELECT COUNT(*) AS c, SUM(x) AS s FROM t"),
            SqlMerge::ScalarAggregate(vec![AggMerge::Sum, AggMerge::Sum])
        );
        assert_eq!(
            sql_merge_strategy("SELECT MIN(x) AS a, MAX(x) AS b FROM t"),
            SqlMerge::ScalarAggregate(vec![AggMerge::Min, AggMerge::Max])
        );
        assert_eq!(
            sql_merge_strategy("SELECT COUNT(*) AS c FROM t WHERE sev = 'ERROR'"),
            SqlMerge::ScalarAggregate(vec![AggMerge::Sum])
        );
    }

    /// A join of two topics returned 316 where the answer was 900: rows pair up
    /// by value, the nodes are split by partition, and a pair whose sides lead
    /// on different nodes is formed nowhere.
    #[test]
    fn a_query_over_two_topics_is_refused() {
        assert!(
            refusal("SELECT COUNT(*) AS c FROM logs a JOIN side b ON a.id = b.id")
                .contains("more than one topic")
        );
        assert!(
            refusal("SELECT COUNT(*) AS c FROM logs WHERE id IN (SELECT id FROM side)")
                .contains("more than one topic")
        );
    }

    /// …but one topic joined to itself is co-located on every node, and a CTE
    /// over a single topic is still a single topic. Both merged correctly before
    /// and must keep doing so.
    #[test]
    fn one_topic_is_still_merged_even_when_named_twice() {
        assert_eq!(
            sql_merge_strategy("SELECT COUNT(*) AS c FROM logs a JOIN logs b ON a.id = b.id"),
            SqlMerge::ScalarAggregate(vec![AggMerge::Sum])
        );
        assert_eq!(
            sql_merge_strategy("WITH a AS (SELECT id FROM logs) SELECT COUNT(*) AS c FROM a"),
            SqlMerge::ScalarAggregate(vec![AggMerge::Sum])
        );
    }
}
