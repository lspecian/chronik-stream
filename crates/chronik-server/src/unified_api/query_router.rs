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
    post: &MergePlan,
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

    // Finished across the union, before the transport cap: each node ordered
    // and limited only its own partitions, so this is where the query's own
    // clauses actually get applied (#55).
    finish_rows(&mut columns, &mut all_rows, post);

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
    Concatenate(MergePlan),
    /// One row per node; combine column by column with these functions, in the
    /// order the columns appear.
    ScalarAggregate { how: Vec<AggMerge>, post: MergePlan },
    /// Grouped results: re-aggregate by group key across nodes. Runs parallel to
    /// the response columns — `None` is a group key, `Some(op)` an aggregate.
    GroupedAggregate {
        how: Vec<Option<AggMerge>>,
        post: MergePlan,
    },
    /// Cannot be combined without changing the query. Better to say so than to
    /// return a number that looks right.
    Unsupported(String),
}

/// How a fanned-out query is adapted for the nodes and finished afterwards.
///
/// Two halves of the same decision. `node_sql` is what every node runs — which
/// is not always what the caller wrote, because a clause the nodes apply to
/// their own partitions can be wrong there and right here: each node's `LIMIT 5`
/// is its own five rows, and its `OFFSET 10` skips its own ten. Those clauses
/// are removed before the query goes out and re-applied once, over the union.
///
/// The rest is what the coordinator still owes when the rows arrive. Sorting the
/// union and taking n is what makes `ORDER BY id DESC LIMIT 5` the answer —
/// without it, three nodes returned 15 rows whose first five were not the top
/// five (#55). Taking the top n from each node's own top n is sound: a row in
/// the global top n is in its own node's top n, so no candidate is missing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MergePlan {
    /// SQL for the nodes to run instead of the caller's, when they differ.
    pub node_sql: Option<String>,
    /// Output columns to sort by, outermost first. Empty means the query did
    /// not ask for an order.
    pub order_by: Vec<OrderKey>,
    /// The query's own `OFFSET`, applied once over the combined rows.
    pub offset: Option<usize>,
    /// The query's own `LIMIT`, applied once over the combined rows.
    pub limit: Option<usize>,
    /// AVG columns the nodes were asked for as a sum and a count, to divide
    /// once the partials are combined.
    pub derived: Vec<DerivedColumn>,
    /// A HAVING to apply to the combined groups, which the nodes did not run.
    pub having: Option<Predicate>,
    /// A COUNT(DISTINCT) rebuilt from the values the nodes returned.
    pub distinct_count: Option<DistinctCount>,
    /// A quantile rebuilt from the value counts the nodes returned.
    pub quantile: Option<QuantileColumn>,
    /// SQL the coordinator runs over the merged rows to rebuild the shape the
    /// nodes could not be asked for.
    pub finish_sql: Option<String>,
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
    /// `AVG(expr)`. Not combinable from per-node derived, but exactly
    /// recoverable if the nodes are asked for `SUM(expr)` and `COUNT(expr)`
    /// instead and the division is done once, here. Carries the argument so the
    /// projection can be rewritten.
    Derived(Derived, String),
    /// `COUNT(DISTINCT expr)`. Rebuilt by collecting the distinct values
    /// themselves and counting them once combined. Carries the argument.
    DistinctCount(String),
    /// An expression with aggregates inside it: the aggregates are projected
    /// for the nodes and the shape around them is rebuilt by the engine here.
    Computed {
        rewritten: String,
        inner: Vec<String>,
    },
    /// `median(x)` or `percentile_cont(x, q)`. Rebuilt from how often each
    /// value occurs. Carries the argument and the fraction.
    Quantile(String, f64),
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

    // More than one aggregate in a column is no harder than one: each is asked
    // for separately and the expression joining them is applied to the combined
    // values. `SUM(x)/COUNT(x)` is exactly this, and it is what the old AVG
    // refusal used to recommend.

    // Exactly one aggregate. It has to *be* the column, not sit inside it.
    match expr.0 {
        SqlExpr::Function(f) if f.over.is_none() && is_aggregate_call(f) => {
            let name = function_bare_name(f);
            if function_is_distinct(f) {
                // A distinct count cannot be added up — a value held by two
                // nodes would count twice — but it can be rebuilt: ask the nodes
                // for their distinct values, combine those, and count what is
                // left. That moves values rather than a number, so it only works
                // while they fit in one response; the merge checks.
                if name == "count" {
                    return match single_argument(f) {
                        Some(arg) => ColumnMerge::DistinctCount(arg),
                        None => ColumnMerge::Refuse(
                            "COUNT(DISTINCT ...) needs a single argument to collect across nodes."
                                .to_string(),
                        ),
                    };
                }
                return ColumnMerge::Refuse(format!(
                    "{name}(DISTINCT ...) cannot be merged across nodes: each node de-duplicates \
                     only its own partitions, so a value held by two nodes is counted twice."
                ));
            }
            match name.as_str() {
                "count" | "sum" => ColumnMerge::Mergeable(AggMerge::Sum),
                "min" => ColumnMerge::Mergeable(AggMerge::Min),
                "max" => ColumnMerge::Mergeable(AggMerge::Max),
                // Each of these is a fixed formula over running totals, so the
                // nodes are asked for the totals and the formula is applied once.
                "avg" | "mean" => derived_column(f, name.as_str(), Derived::Average),
                "var" | "var_samp" | "var_sample" => {
                    derived_column(f, name.as_str(), Derived::VarSample)
                }
                "var_pop" | "var_population" => {
                    derived_column(f, name.as_str(), Derived::VarPopulation)
                }
                "stddev" | "stddev_samp" => derived_column(f, name.as_str(), Derived::StddevSample),
                "stddev_pop" => derived_column(f, name.as_str(), Derived::StddevPopulation),
                // A quantile is not derivable from per-node quantiles, but it is
                // derivable from how often each value occurs — and that is just
                // counts, which do merge.
                "median" | "approx_median" => match single_argument(f) {
                    Some(arg) => ColumnMerge::Quantile(arg, 0.5),
                    None => ColumnMerge::Refuse(
                        "median needs a single argument to count across nodes.".to_string(),
                    ),
                },
                "percentile_cont" | "approx_percentile_cont" => match quantile_arguments(f) {
                    Some((arg, q)) => ColumnMerge::Quantile(arg, q),
                    None => ColumnMerge::Refuse(format!(
                        "{name} can be merged across nodes only as {name}(column, <literal \
                         fraction>): the fraction has to be known here to pick the value out of \
                         the combined counts."
                    )),
                },
                other => ColumnMerge::Refuse(format!(
                    "{other}(...) cannot be merged across nodes: it is not derivable from the \
                     per-node results, which would otherwise be returned one row per node."
                )),
            }
        }
        // An aggregate inside an expression cannot be merged from the wrapped
        // per-node values — the wrapping was already applied to each partial —
        // but the aggregates it is built from can be. So the nodes are asked for
        // those, and the wrapping is applied once, here, by the query engine
        // itself rather than by an evaluator written for the purpose.
        other => match detach_aggregates(other) {
            Some((rewritten, inner)) => ColumnMerge::Computed { rewritten, inner },
            None => ColumnMerge::Refuse(
                "an aggregate wrapped in an expression cannot be merged across nodes: the \
                 wrapping was already applied to each node's partial, and this expression is \
                 not one the coordinator can re-apply. Project the bare aggregate and apply \
                 the expression in the client."
                    .to_string(),
            ),
        },
    }
}

/// Pull the aggregates out of an expression, leaving a column reference behind.
///
/// `ROUND(AVG(x), 2)` becomes `ROUND(<name>, 2)` plus the `AVG(x)` that has to
/// be computed first. The nodes are then asked only for things that merge, and
/// the shape around them is rebuilt afterwards from the combined values.
fn detach_aggregates(
    expr: &chronik_columnar::datafusion::sql::sqlparser::ast::Expr,
) -> Option<(String, Vec<String>)> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        visit_expressions_mut, Expr as SqlExpr, Ident,
    };
    use std::ops::ControlFlow;

    let mut rewritten = expr.clone();
    let mut inner: Vec<String> = Vec::new();

    let _: ControlFlow<()> = visit_expressions_mut(&mut rewritten, |e| {
        if let SqlExpr::Function(f) = e {
            if f.over.is_none() && is_aggregate_call(f) {
                let name = format!("__chronik_inner_{}", inner.len());
                inner.push(e.to_string());
                *e = SqlExpr::Identifier(Ident::new(name));
            }
        }
        ControlFlow::Continue(())
    });

    (!inner.is_empty()).then(|| (rewritten.to_string(), inner))
}

/// Project the running totals an `AVG` or a variance is rebuilt from.
///
/// Shared by the two places that need it — a column that *is* one of these, and
/// one that has one buried inside an expression — so the two cannot drift. They
/// did: the second path used to pass the aggregate through to the nodes
/// untouched, which turned each node's own average into a group key.
fn emit_derived_helpers(
    projection: &mut Vec<chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem>,
    how: &mut Vec<Option<AggMerge>>,
    derived: &mut Vec<DerivedColumn>,
    output: &str,
    kind: Derived,
    arg: &str,
) -> Result<(), String> {
    let sum = format!("__chronik_sum_{}", derived.len());
    let count = format!("__chronik_count_{}", derived.len());
    projection.push(parse_projection_item(&format!("SUM({arg})"), &sum)?);
    projection.push(parse_projection_item(&format!("COUNT({arg})"), &count)?);
    how.push(Some(AggMerge::Sum));
    how.push(Some(AggMerge::Sum));

    let sum_sq = kind
        .needs_squares()
        .then(|| format!("__chronik_sumsq_{}", derived.len()));
    if let Some(name) = sum_sq.as_ref() {
        projection.push(parse_projection_item(
            &format!("SUM(({arg}) * ({arg}))"),
            name,
        )?);
        how.push(Some(AggMerge::Sum));
    }

    derived.push(DerivedColumn {
        output: output.to_string(),
        kind,
        sum,
        count,
        sum_sq,
    });
    Ok(())
}
/// Plan the columns whose shape has to be rebuilt after the merge.
///
/// Each aggregate that was buried inside an expression is projected on its own
/// so the nodes return something that merges, and the expression is kept aside
/// to be applied to the combined values.
///
/// The classification of each detached aggregate goes through exactly the same
/// path as a top-level one, so an `AVG` inside a `ROUND` is still carried as a
/// sum and a count rather than being averaged per node.
fn plan_computed(
    query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query,
    select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select,
    columns: &[ColumnMerge],
) -> Result<(String, Vec<Option<AggMerge>>, Vec<DerivedColumn>, String), String> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem;

    let mut projection = Vec::new();
    let mut how: Vec<Option<AggMerge>> = Vec::new();
    let mut derived = Vec::new();
    // What the coordinator projects from the merged rows, in the caller's order.
    let mut finish_projection: Vec<String> = Vec::new();

    for (idx, (item, class)) in select.projection.iter().zip(columns).enumerate() {
        let name = match item {
            SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
            SelectItem::UnnamedExpr(expr) => expr.to_string(),
            _ => return Err(UNREWRITABLE.to_string()),
        };

        let ColumnMerge::Computed { rewritten, inner } = class else {
            match class {
                // A group key, or an aggregate that merges as it stands: the
                // nodes project it unchanged and the coordinator passes it on.
                ColumnMerge::RowWise => {
                    projection.push(item.clone());
                    how.push(None);
                }
                ColumnMerge::Mergeable(op) => {
                    projection.push(item.clone());
                    how.push(Some(*op));
                }
                // An AVG or a variance beside a computed column still has to be
                // taken apart. Passing it through would have sent `AVG(x)` to
                // the nodes and then treated each node's own average as a group
                // key of its own — one row per node, quietly.
                ColumnMerge::Derived(kind, arg) => {
                    emit_derived_helpers(
                        &mut projection,
                        &mut how,
                        &mut derived,
                        &name,
                        *kind,
                        arg,
                    )?;
                }
                // These rewrite the query into something of a different shape —
                // a frequency table, or de-duplicated values — which cannot be
                // the same query as one carrying a computed column.
                ColumnMerge::Quantile(..) | ColumnMerge::DistinctCount(_) => {
                    return Err(
                        "a quantile or a distinct count cannot be merged across nodes in \
                                the same query as an aggregate wrapped in an expression: the two \
                                need the nodes to return different shapes. Ask for them \
                                separately."
                            .to_string(),
                    )
                }
                ColumnMerge::Refuse(why) => return Err(why.clone()),
                ColumnMerge::Computed { .. } => unreachable!("handled by the outer pattern"),
            }
            finish_projection.push(format!("{} AS {}", quote_ident(&name), quote_ident(&name)));
            continue;
        };

        // Name the detached aggregates apart per column, so two columns that
        // each contain `SUM(x)` do not collide on one helper.
        let mut expression = rewritten.clone();
        for (slot, aggregate) in inner.iter().enumerate() {
            let placeholder = format!("__chronik_inner_{slot}");
            let helper = format!("__chronik_inner_{idx}_{slot}");
            expression = expression.replace(&placeholder, &helper);

            match classify_projection(&SqlExprRef(&parse_expression(aggregate)?)) {
                ColumnMerge::Mergeable(op) => {
                    projection.push(parse_projection_item(aggregate, &helper)?);
                    how.push(Some(op));
                }
                ColumnMerge::Derived(kind, arg) => emit_derived_helpers(
                    &mut projection,
                    &mut how,
                    &mut derived,
                    &helper,
                    kind,
                    &arg,
                )?,
                ColumnMerge::Refuse(why) => return Err(why),
                _ => {
                    return Err(format!(
                        "{aggregate} cannot be merged across nodes inside an expression."
                    ))
                }
            }
        }

        finish_projection.push(format!("{expression} AS {}", quote_ident(&name)));
    }

    let node_sql =
        sql_with_projection(query, projection, false).ok_or_else(|| UNREWRITABLE.to_string())?;
    let finish_sql = format!(
        "SELECT {} FROM {MERGED_TABLE}",
        finish_projection.join(", ")
    );
    Ok((node_sql, how, derived, finish_sql))
}

/// The name the merged rows are registered under while the coordinator finishes
/// the query over them.
pub const MERGED_TABLE: &str = "__chronik_merged";

/// Quote an identifier so a generated column name cannot be read as syntax.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Parse a bare expression from its SQL text.
fn parse_expression(
    text: &str,
) -> Result<chronik_columnar::datafusion::sql::sqlparser::ast::Expr, String> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem;
    match parse_projection_item(text, "__chronik_expr")? {
        SelectItem::ExprWithAlias { expr, .. } => Ok(expr),
        _ => Err(UNREWRITABLE.to_string()),
    }
}
/// Plan a quantile, which the nodes answer as a frequency table.
///
/// `SELECT k, median(x) AS m FROM t GROUP BY k` becomes
/// `SELECT k, x AS <value>, COUNT(*) AS <count> FROM t GROUP BY k, x`: every
/// node says how often it holds each value, the counts are summed per value by
/// the ordinary group merge, and the quantile is read off the total here.
fn plan_quantile(
    query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query,
    select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select,
    columns: &[ColumnMerge],
) -> Result<(String, Vec<Option<AggMerge>>, QuantileColumn), String> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{GroupByExpr, SelectItem, SetExpr};

    if columns
        .iter()
        .filter(|c| matches!(c, ColumnMerge::Quantile(..)))
        .count()
        > 1
    {
        return Err(
            "only one quantile can be merged across nodes at a time: each one needs the \
                    counts of its own column. Ask for them one query at a time."
                .to_string(),
        );
    }
    if columns
        .iter()
        .any(|c| matches!(c, ColumnMerge::Mergeable(_) | ColumnMerge::Derived(..)))
    {
        return Err(
            "a quantile cannot be merged across nodes alongside another aggregate: the \
                    nodes return a count per value, and the other aggregate would be computed \
                    over those rather than over the rows."
                .to_string(),
        );
    }
    // A distinct count is neither of those, and fell through this check to be
    // treated as a group key — which put `COUNT(DISTINCT x)` into the nodes'
    // GROUP BY and made them reject the query with a message about the rewrite
    // rather than about what was asked.
    if columns
        .iter()
        .any(|c| matches!(c, ColumnMerge::DistinctCount(_)))
    {
        return Err(
            "a quantile and a COUNT(DISTINCT ...) cannot be merged across nodes in the \
                    same query: one needs a count per value from the nodes and the other needs \
                    the values themselves. Ask for them separately."
                .to_string(),
        );
    }

    let mut projection = Vec::with_capacity(select.projection.len() + 1);
    let mut how = Vec::with_capacity(select.projection.len() + 1);
    let mut group_by = Vec::new();
    let mut keys = Vec::new();
    let mut spec = None;

    for (idx, (item, class)) in select.projection.iter().zip(columns).enumerate() {
        match class {
            ColumnMerge::Quantile(arg, fraction) => {
                // Named by `alias_bare_projection` if the caller left it out,
                // so this is a guard rather than the common path.
                let SelectItem::ExprWithAlias { alias, .. } = item else {
                    return Err(
                        "a quantile has to be given a name to be merged across nodes, \
                                because the nodes are asked for value counts instead. Write \
                                median(x) AS median_x."
                            .to_string(),
                    );
                };
                let value = format!("__chronik_q_value_{idx}");
                let count = format!("__chronik_q_count_{idx}");
                projection.push(parse_projection_item(arg, &value)?);
                projection.push(parse_projection_item("COUNT(*)", &count)?);
                how.push(None); // the value is a key of the merge
                how.push(Some(AggMerge::Sum)); // its count is what combines
                group_by.push(parse_group_by_expr(arg)?);
                spec = Some(QuantileColumn {
                    output: alias.value.clone(),
                    value,
                    count,
                    keys: Vec::new(),
                    fraction: *fraction,
                });
            }
            _ => {
                let (name, expr) = match item {
                    SelectItem::ExprWithAlias { alias, expr } => (alias.value.clone(), expr),
                    SelectItem::UnnamedExpr(expr) => (expr.to_string(), expr),
                    _ => return Err(UNREWRITABLE.to_string()),
                };
                keys.push(name);
                group_by.push(expr.clone());
                projection.push(item.clone());
                how.push(None);
            }
        }
    }

    let mut spec = spec.ok_or_else(|| UNREWRITABLE.to_string())?;
    spec.keys = keys;

    let mut rewritten = query.clone();
    rewritten.order_by = None;
    rewritten.limit = None;
    rewritten.offset = None;
    strip_having(&mut rewritten);
    let SetExpr::Select(inner) = rewritten.body.as_mut() else {
        return Err(UNREWRITABLE.to_string());
    };
    inner.projection = projection;
    // Grouped by the caller's keys *and* the value, which is what turns the
    // query into a frequency table.
    inner.group_by = GroupByExpr::Expressions(group_by, Vec::new());

    Ok((rewritten.to_string(), how, spec))
}

/// Parse one GROUP BY term from its SQL text.
fn parse_group_by_expr(
    expr: &str,
) -> Result<chronik_columnar::datafusion::sql::sqlparser::ast::Expr, String> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem;
    match parse_projection_item(expr, "__chronik_group")? {
        SelectItem::ExprWithAlias { expr, .. } => Ok(expr),
        _ => Err(UNREWRITABLE.to_string()),
    }
}
/// Plan a `COUNT(DISTINCT x)`, which is answered by moving values rather than
/// counts and so rewrites the query more heavily than the others.
///
/// `SELECT k, COUNT(DISTINCT x) AS c FROM t GROUP BY k` becomes
/// `SELECT DISTINCT k, x AS <helper> FROM t`: every node returns the pairs it
/// holds, the union is de-duplicated by the ordinary group merge, and the
/// counting happens here.
fn plan_distinct_count(
    query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query,
    select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select,
    columns: &[ColumnMerge],
) -> Result<(String, Vec<Option<AggMerge>>, DistinctCount), String> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        Distinct, GroupByExpr, SelectItem, SetExpr,
    };

    // One at a time: two of them would need the values paired up, and every
    // pair of one value with another is a different question from counting
    // either of them.
    if columns
        .iter()
        .filter(|c| matches!(c, ColumnMerge::DistinctCount(_)))
        .count()
        > 1
    {
        return Err(
            "only one COUNT(DISTINCT ...) can be merged across nodes at a time; ask for \
                    them one query at a time."
                .to_string(),
        );
    }

    // Anything else aggregated alongside it would be computed over the
    // de-duplicated values rather than the rows.
    if columns
        .iter()
        .any(|c| matches!(c, ColumnMerge::Mergeable(_) | ColumnMerge::Derived(..)))
    {
        return Err(
            "COUNT(DISTINCT ...) cannot be merged across nodes alongside another \
                    aggregate: the nodes return their distinct values, and the other aggregate \
                    would be computed over those rather than over the rows."
                .to_string(),
        );
    }

    let mut projection = Vec::with_capacity(select.projection.len());
    let mut how = Vec::with_capacity(select.projection.len());
    let mut keys = Vec::new();
    let mut spec = None;

    for (idx, (item, class)) in select.projection.iter().zip(columns).enumerate() {
        match class {
            ColumnMerge::DistinctCount(arg) => {
                // Named by `alias_bare_projection` if the caller left it out,
                // so this is a guard rather than the common path.
                let SelectItem::ExprWithAlias { alias, .. } = item else {
                    return Err(
                        "COUNT(DISTINCT ...) has to be given a name to be merged across \
                                nodes, because the nodes are asked for the values instead. Write \
                                COUNT(DISTINCT x) AS distinct_x."
                            .to_string(),
                    );
                };
                let value = format!("__chronik_distinct_{idx}");
                projection.push(parse_projection_item(arg, &value)?);
                how.push(None); // a key of the de-duplicating merge, not an aggregate
                spec = Some(DistinctCount {
                    output: alias.value.clone(),
                    value,
                    keys: Vec::new(),
                });
            }
            _ => {
                let name = match item {
                    SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                    SelectItem::UnnamedExpr(expr) => expr.to_string(),
                    _ => return Err(UNREWRITABLE.to_string()),
                };
                keys.push(name);
                projection.push(item.clone());
                how.push(None);
            }
        }
    }

    let mut spec = spec.ok_or_else(|| UNREWRITABLE.to_string())?;
    spec.keys = keys;

    // The nodes de-duplicate their own rows; the merge de-duplicates across
    // them. The grouping is gone because the counting has moved here.
    let mut rewritten = query.clone();
    rewritten.order_by = None;
    rewritten.limit = None;
    rewritten.offset = None;
    strip_having(&mut rewritten);
    let SetExpr::Select(inner) = rewritten.body.as_mut() else {
        return Err(UNREWRITABLE.to_string());
    };
    inner.projection = projection;
    inner.distinct = Some(Distinct::Distinct);
    inner.group_by = GroupByExpr::Expressions(Vec::new(), Vec::new());

    Ok((rewritten.to_string(), how, spec))
}
/// Rewrite the projection so the nodes return what an AVG can be rebuilt from.
///
/// `AVG(x) AS a` becomes `SUM(x) AS <helper>, COUNT(x) AS <helper>`; the two
/// helpers merge by summing, and the division happens once over the totals.
/// Everything else is passed through untouched.
///
/// Returns the new projection, the merge op per resulting column, and the
/// derived to finish afterwards.
#[allow(clippy::type_complexity)]
fn rewrite_projection_for_derived(
    select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select,
    columns: &[ColumnMerge],
) -> Result<
    (
        Vec<chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem>,
        Vec<Option<AggMerge>>,
        Vec<DerivedColumn>,
    ),
    String,
> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem;

    let mut projection = Vec::with_capacity(select.projection.len());
    let mut how = Vec::with_capacity(select.projection.len());
    let mut derived = Vec::new();

    for (idx, (item, class)) in select.projection.iter().zip(columns).enumerate() {
        let ColumnMerge::Derived(kind, arg) = class else {
            projection.push(item.clone());
            how.push(match class {
                ColumnMerge::Mergeable(op) => Some(*op),
                _ => None,
            });
            continue;
        };

        // The response has to describe the query the caller wrote, and without
        // an alias there is no name to put the average back under — the engine
        // would have generated one from an expression that is no longer there.
        //
        // `alias_bare_projection` supplies that name before this runs, so this
        // is a guard on the planner rather than something a caller can hit by
        // leaving the alias out.
        let SelectItem::ExprWithAlias { alias, .. } = item else {
            return Err(
                "this aggregate has to be given a name to be merged across nodes, because the \
                 nodes are asked for the totals it is built from instead. Write AVG(x) AS \
                 avg_x."
                    .to_string(),
            );
        };

        let sum = format!("__chronik_sum_{idx}");
        let count = format!("__chronik_count_{idx}");
        projection.push(parse_projection_item(&format!("SUM({arg})"), &sum)?);
        projection.push(parse_projection_item(&format!("COUNT({arg})"), &count)?);
        how.push(Some(AggMerge::Sum));
        how.push(Some(AggMerge::Sum));

        // A variance also needs the squares. Summing them is exact and needs no
        // second pass: the nodes never have to see each other's rows.
        let sum_sq = kind
            .needs_squares()
            .then(|| format!("__chronik_sumsq_{idx}"));
        if let Some(name) = sum_sq.as_ref() {
            projection.push(parse_projection_item(
                &format!("SUM(({arg}) * ({arg}))"),
                name,
            )?);
            how.push(Some(AggMerge::Sum));
        }

        derived.push(DerivedColumn {
            output: alias.value.clone(),
            kind: *kind,
            sum,
            count,
            sum_sq,
        });
    }

    Ok((projection, how, derived))
}

/// Build one `SELECT` item by parsing it, rather than assembling the AST by
/// hand: the parser is the thing that decides what these nodes look like.
fn parse_projection_item(
    expr: &str,
    alias: &str,
) -> Result<chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem, String> {
    use chronik_columnar::datafusion::sql::parser::{DFParser, Statement};
    use chronik_columnar::datafusion::sql::sqlparser::ast::{SetExpr, Statement as AstStatement};

    let sql = format!("SELECT {expr} AS {alias} FROM __chronik_rewrite");
    let statements =
        DFParser::parse_sql(&sql).map_err(|e| format!("could not rewrite AVG: {e}"))?;
    let Some(Statement::Statement(ast)) = statements.front() else {
        return Err("could not rewrite AVG".to_string());
    };
    let AstStatement::Query(query) = ast.as_ref() else {
        return Err("could not rewrite AVG".to_string());
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err("could not rewrite AVG".to_string());
    };
    select
        .projection
        .first()
        .cloned()
        .ok_or_else(|| "could not rewrite AVG".to_string())
}

/// The caller's query with a replacement projection and no ordering clauses.
///
/// `keep_order` has the same meaning as in [`sql_without_ordering`].
fn sql_with_projection(
    query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query,
    projection: Vec<chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem>,
    keep_order: bool,
) -> Option<String> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::SetExpr;

    let mut rewritten = query.clone();
    if !keep_order {
        rewritten.order_by = None;
    }
    rewritten.limit = None;
    rewritten.offset = None;
    strip_having(&mut rewritten);
    let SetExpr::Select(select) = rewritten.body.as_mut() else {
        return None;
    };
    select.projection = projection;
    Some(rewritten.to_string())
}

/// Classify a function whose value is a formula over running totals.
fn derived_column(
    f: &chronik_columnar::datafusion::sql::sqlparser::ast::Function,
    name: &str,
    kind: Derived,
) -> ColumnMerge {
    match single_argument(f) {
        Some(arg) => ColumnMerge::Derived(kind, arg),
        // `AVG(*)` and the like: nothing to total up.
        None => ColumnMerge::Refuse(format!(
            "{name} cannot be merged across nodes unless it has a single argument, which is \
             what the nodes are asked to total."
        )),
    }
}
/// The single argument of a function call, rendered back to SQL.
fn single_argument(
    f: &chronik_columnar::datafusion::sql::sqlparser::ast::Function,
) -> Option<String> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        FunctionArg, FunctionArgExpr, FunctionArguments,
    };
    let FunctionArguments::List(list) = &f.args else {
        return None;
    };
    if list.args.len() != 1 {
        return None;
    }
    match &list.args[0] {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.to_string()),
        FunctionArg::Named {
            arg: FunctionArgExpr::Expr(e),
            ..
        } => Some(e.to_string()),
        _ => None,
    }
}

/// The column and the fraction of a `percentile_cont(x, 0.95)` call.
fn quantile_arguments(
    f: &chronik_columnar::datafusion::sql::sqlparser::ast::Function,
) -> Option<(String, f64)> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        FunctionArg, FunctionArgExpr, FunctionArguments,
    };
    let FunctionArguments::List(list) = &f.args else {
        return None;
    };
    if list.args.len() != 2 {
        return None;
    }
    let expr_of = |a: &FunctionArg| match a {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
        | FunctionArg::Named {
            arg: FunctionArgExpr::Expr(e),
            ..
        } => Some(e.clone()),
        _ => None,
    };
    let column = expr_of(&list.args[0])?.to_string();
    let fraction = json_literal(&expr_of(&list.args[1])?)?.as_f64()?;
    // A fraction outside [0, 1] picks no value; let the engine reject it.
    (0.0..=1.0)
        .contains(&fraction)
        .then_some((column, fraction))
}

/// A quantile rebuilt from how often each value occurs.
///
/// Nodes cannot combine their own quantiles — the median of three medians is
/// not the median — but they can each say how many times they hold each value,
/// and counts do combine. Adding those up gives the exact distribution, and the
/// quantile follows from walking it.
///
/// Exact, then, rather than estimated, and the traffic is one row per *distinct
/// value* rather than per row: a severity or status-code column is a handful of
/// rows however many billion records it summarises. A column of distinct floats
/// is the case this does not suit, and there the merge says so rather than
/// guessing — that is where a t-digest would earn its keep.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantileColumn {
    /// The column the caller asked for.
    pub output: String,
    /// Helper columns holding each value and how often it occurs.
    pub value: String,
    pub count: String,
    /// The group keys the quantile is taken within; empty for the whole topic.
    pub keys: Vec<String>,
    /// Which quantile: 0.5 for a median.
    pub fraction: f64,
}

/// Collapse counted values into the quantile the caller asked for.
fn finish_quantile(
    columns: &mut Vec<String>,
    rows: &mut Vec<HashMap<String, serde_json::Value>>,
    spec: &QuantileColumn,
) {
    // Gather (value, count) per group, keeping the key values for the output.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, (HashMap<String, serde_json::Value>, Vec<(f64, f64)>)> =
        HashMap::new();

    for row in rows.iter() {
        let identity = spec
            .keys
            .iter()
            .map(|k| row.get(k).unwrap_or(&serde_json::Value::Null).to_string())
            .collect::<Vec<_>>()
            .join("\u{1f}");

        let entry = groups.entry(identity.clone()).or_insert_with(|| {
            order.push(identity);
            let keys = spec
                .keys
                .iter()
                .map(|k| {
                    (
                        k.clone(),
                        row.get(k).cloned().unwrap_or(serde_json::Value::Null),
                    )
                })
                .collect();
            (keys, Vec::new())
        });

        // NULLs are not part of the distribution, as in any SQL quantile.
        let (Some(value), Some(count)) = (
            row.get(&spec.value).and_then(serde_json::Value::as_f64),
            row.get(&spec.count).and_then(serde_json::Value::as_f64),
        ) else {
            continue;
        };
        if count > 0.0 {
            entry.1.push((value, count));
        }
    }

    *rows = order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .map(|(mut out, counted)| {
            out.insert(
                spec.output.clone(),
                quantile_of(counted, spec.fraction)
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
            );
            out
        })
        .collect();

    *columns = spec
        .keys
        .iter()
        .cloned()
        .chain(std::iter::once(spec.output.clone()))
        .collect();
}

/// The continuous quantile of a distribution given as (value, count) pairs.
///
/// Continuous, meaning it interpolates between the two values the fraction
/// falls between rather than snapping to one of them — the same definition
/// `percentile_cont` has, and the one that makes the median of an even number
/// of rows land halfway.
fn quantile_of(mut counted: Vec<(f64, f64)>, fraction: f64) -> Option<f64> {
    if counted.is_empty() {
        return None;
    }
    counted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let total: f64 = counted.iter().map(|(_, n)| n).sum();
    if total <= 0.0 {
        return None;
    }

    // Position in the sorted run of values, as SQL's percentile_cont defines it.
    let target = fraction * (total - 1.0);
    let lower_rank = target.floor();
    let upper_rank = target.ceil();

    // Walk the cumulative counts once, picking off both ranks.
    let (mut seen, mut low, mut high) = (0.0_f64, None, None);
    for (value, count) in &counted {
        let next = seen + count;
        if low.is_none() && lower_rank < next {
            low = Some(*value);
        }
        if high.is_none() && upper_rank < next {
            high = Some(*value);
            break;
        }
        seen = next;
    }

    let low = low.or_else(|| counted.last().map(|(v, _)| *v))?;
    let high = high.or_else(|| counted.last().map(|(v, _)| *v))?;
    Some(low + (high - low) * (target - lower_rank))
}
/// A `COUNT(DISTINCT x)` carried across the fan-out as the values themselves.
///
/// Counts cannot be added up when they are distinct counts — a value held by
/// two nodes would be counted twice — so the nodes are asked for their distinct
/// values instead, the union is de-duplicated, and what is left is counted.
///
/// That ships values rather than a number, so it is only exact while every
/// node's values fit in one response. When one does not, the merge says so
/// rather than returning a count that is quietly short.
#[derive(Debug, Clone, PartialEq)]
pub struct DistinctCount {
    /// The column the caller asked for.
    pub output: String,
    /// The helper column holding the values.
    pub value: String,
    /// The group keys the count is taken within; empty for a whole-topic count.
    pub keys: Vec<String>,
}

/// Collapse de-duplicated values into the counts the caller asked for.
fn finish_distinct_count(
    columns: &mut Vec<String>,
    rows: &mut Vec<HashMap<String, serde_json::Value>>,
    spec: &DistinctCount,
) {
    let mut order: Vec<String> = Vec::new();
    let mut counted: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();

    for row in rows.iter() {
        let identity = spec
            .keys
            .iter()
            .map(|k| row.get(k).unwrap_or(&serde_json::Value::Null).to_string())
            .collect::<Vec<_>>()
            .join("\u{1f}");

        // SQL does not count NULL as a distinct value.
        let counts = !row
            .get(&spec.value)
            .map(serde_json::Value::is_null)
            .unwrap_or(true);

        let entry = counted.entry(identity.clone()).or_insert_with(|| {
            order.push(identity);
            let mut group: HashMap<String, serde_json::Value> = spec
                .keys
                .iter()
                .map(|k| {
                    (
                        k.clone(),
                        row.get(k).cloned().unwrap_or(serde_json::Value::Null),
                    )
                })
                .collect();
            group.insert(spec.output.clone(), serde_json::Value::from(0));
            group
        });

        if counts {
            let now = entry
                .get(&spec.output)
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
                + 1;
            entry.insert(spec.output.clone(), serde_json::Value::from(now));
        }
    }

    *rows = order
        .into_iter()
        .filter_map(|k| counted.remove(&k))
        .collect();
    *columns = spec
        .keys
        .iter()
        .cloned()
        .chain(std::iter::once(spec.output.clone()))
        .collect();
}
/// A column the nodes cannot combine, carried across the fan-out as the running
/// totals it *can* be rebuilt from.
///
/// An average is a sum over a count. A variance is those two plus the sum of
/// squares, since `E[x²] - E[x]²` needs nothing else — which makes both exact
/// here, not estimates: no sketch, no sampling, the same number a single node
/// holding all the rows would have produced.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedColumn {
    /// The column the caller asked for.
    pub output: String,
    /// What to rebuild from the totals.
    pub kind: Derived,
    /// The helper columns the nodes were asked for instead.
    pub sum: String,
    pub count: String,
    /// Only variance and standard deviation need the squares.
    pub sum_sq: Option<String>,
}

/// What a [`DerivedColumn`] rebuilds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Derived {
    Average,
    /// Sample variance and its root: the `n - 1` denominator, which is what
    /// plain `var` and `stddev` mean in this engine as in most.
    VarSample,
    StddevSample,
    /// Population variance and its root: the `n` denominator.
    VarPopulation,
    StddevPopulation,
}

impl Derived {
    fn needs_squares(self) -> bool {
        !matches!(self, Derived::Average)
    }

    /// Rebuild the value from the combined totals.
    fn combine(self, sum: f64, sum_sq: f64, count: f64) -> Option<f64> {
        if count == 0.0 {
            return None;
        }
        match self {
            Derived::Average => Some(sum / count),
            Derived::VarPopulation | Derived::StddevPopulation => {
                // Clamped at zero: with floating point, a column of identical
                // values can land a hair below it and a negative variance is
                // worse than a zero one.
                let var = (sum_sq / count - (sum / count).powi(2)).max(0.0);
                Some(match self {
                    Derived::StddevPopulation => var.sqrt(),
                    _ => var,
                })
            }
            Derived::VarSample | Derived::StddevSample => {
                if count < 2.0 {
                    return None; // undefined for a single row, as in SQL
                }
                let var = ((sum_sq - sum * sum / count) / (count - 1.0)).max(0.0);
                Some(match self {
                    Derived::StddevSample => var.sqrt(),
                    _ => var,
                })
            }
        }
    }
}

/// Finish the derived: divide, name the result what the caller asked for, and
/// drop the helpers.
///
/// Averaging per-node derived is only right when every node holds the same
/// number of rows, which a partitioned topic does not guarantee. Summing the
/// sums and the counts and dividing once is exact regardless of how the rows
/// are spread.
pub fn finish_derived(response: &mut SqlResponse, derived: &[DerivedColumn]) {
    finish_derived_rows(&mut response.columns, &mut response.rows, derived);
}

/// The same, over the parts, for callers that have not built a response yet.
fn finish_derived_rows(
    columns: &mut Vec<String>,
    rows: &mut Vec<HashMap<String, serde_json::Value>>,
    derived: &[DerivedColumn],
) {
    if derived.is_empty() {
        return;
    }

    for row in rows.iter_mut() {
        for column in derived {
            let sum = row.get(&column.sum).and_then(serde_json::Value::as_f64);
            let count = row.get(&column.count).and_then(serde_json::Value::as_f64);
            let sum_sq = column
                .sum_sq
                .as_ref()
                .and_then(|c| row.get(c))
                .and_then(serde_json::Value::as_f64);

            // No rows contributed, so there is no value here — not zero.
            let value = match (sum, count) {
                (Some(s), Some(c)) => column
                    .kind
                    .combine(s, sum_sq.unwrap_or(0.0), c)
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            };

            row.remove(&column.sum);
            row.remove(&column.count);
            if let Some(sq) = column.sum_sq.as_ref() {
                row.remove(sq);
            }
            row.insert(column.output.clone(), value);
        }
    }

    // Put the caller's column back where the helpers were, so the response
    // describes the query that was asked rather than the one that was sent.
    let mut rebuilt = Vec::with_capacity(columns.len());
    for column in columns.drain(..) {
        match derived.iter().find(|a| a.sum == column) {
            Some(avg) => rebuilt.push(avg.output.clone()),
            None => {
                let helper = derived
                    .iter()
                    .any(|d| d.count == column || d.sum_sq.as_deref() == Some(column.as_str()));
                if !helper {
                    rebuilt.push(column);
                }
            }
        }
    }
    *columns = rebuilt;
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
        return SqlMerge::Concatenate(MergePlan::default()); // unparseable here; the engine will reject it
    };
    let Some(Statement::Statement(ast)) = statements.front() else {
        return SqlMerge::Concatenate(MergePlan::default());
    };
    let AstStatement::Query(query) = ast.as_ref() else {
        return SqlMerge::Concatenate(MergePlan::default());
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
        return SqlMerge::Concatenate(MergePlan::default());
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

    // Everything below applies the rest of the query by name over the merged
    // rows, and those names come from the SQL. The engine names an unaliased
    // column differently from the parser (`count(*)` against `COUNT(*)`), so
    // where a name is going to be used, the nodes are asked for the parser's —
    // see [`alias_bare_projection`] for what each of those lookups did when the
    // two disagreed. Queries that need no name keep the engine's own, so a
    // working query's column names do not change under it.
    let needs_names = query.order_by.is_some()
        || select.having.is_some()
        || columns.iter().any(|c| {
            matches!(
                c,
                // A derived column is rebuilt from running totals, so the name
                // it goes back under has to be known. Naming it here means a
                // bare `SELECT AVG(x) FROM t` is answered rather than refused
                // for want of an alias the caller should not have to write.
                ColumnMerge::Computed { .. }
                    | ColumnMerge::Quantile(..)
                    | ColumnMerge::DistinctCount(_)
                    | ColumnMerge::Derived(..)
            )
        });
    let base: &chronik_columnar::datafusion::sql::sqlparser::ast::Query = query;
    let normalized_query = needs_names
        .then(|| alias_bare_projection(select))
        .and_then(|(projection, changed)| {
            changed.then(|| {
                let mut rewritten = base.clone();
                if let SetExpr::Select(select) = rewritten.body.as_mut() {
                    select.projection = projection;
                }
                rewritten
            })
        });
    let normalized = normalized_query.is_some();
    let query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query =
        normalized_query.as_ref().unwrap_or(base);
    let select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select =
        match query.body.as_ref() {
            SetExpr::Select(select) => select,
            _ => unreachable!("a non-Select body returned above"),
        };

    // A `SELECT *` orders by a column the SQL never names, so the nodes keep
    // their own ORDER BY: the engine then rejects a name that is not a column,
    // rather than the coordinator sorting by a column that is not there.
    let keep_node_ordering = projection_has_wildcard(select);

    // A HAVING is meant for the combined groups, so it is resolved to the
    // columns the response will carry and applied here rather than by the nodes.
    let having =
        match &select.having {
            None => None,
            Some(expr) => {
                let resolved = projection_names(select)
                    .and_then(|(names, by_text)| resolve_having(expr, &names, &by_text));
                match resolved {
                    Some(predicate) => Some(predicate),
                    None => return SqlMerge::Unsupported(
                        "this HAVING cannot be applied across nodes: it has to compare a column \
                         the query also projects against a literal, because the groups are only \
                         whole once they reach the coordinator. Project the aggregate and filter \
                         on that column."
                            .to_string(),
                    ),
                }
            }
        };

    // An expression with an aggregate inside it: the nodes are asked for the
    // aggregates alone, and the shape around them is rebuilt over the merged
    // values by the engine itself.
    if columns
        .iter()
        .any(|c| matches!(c, ColumnMerge::Computed { .. }))
    {
        let (node_sql, how, derived, finish_sql) = match plan_computed(query, select, &columns) {
            Ok(parts) => parts,
            Err(why) => return SqlMerge::Unsupported(why),
        };
        let order_by = match resolve_order_by(query, select) {
            Some(keys) => keys,
            None => return SqlMerge::Unsupported(UNORDERABLE.to_string()),
        };
        let plan = MergePlan {
            node_sql: Some(node_sql),
            order_by,
            offset: query_offset(query),
            limit: query_limit(query),
            derived,
            having,
            distinct_count: None,
            quantile: None,
            finish_sql: Some(finish_sql),
        };
        // Grouped or not, the merge is the same shape once the aggregates are
        // projected on their own: keys combine by key, aggregates by operator.
        return SqlMerge::GroupedAggregate { how, post: plan };
    }

    // A quantile is answered as a frequency table, which reshapes the query
    // rather than adjusting it.
    if columns
        .iter()
        .any(|c| matches!(c, ColumnMerge::Quantile(..)))
    {
        let (node_sql, how, spec) = match plan_quantile(query, select, &columns) {
            Ok(parts) => parts,
            Err(why) => return SqlMerge::Unsupported(why),
        };
        let order_by = match resolve_order_by(query, select) {
            Some(keys) => keys,
            None => return SqlMerge::Unsupported(UNORDERABLE.to_string()),
        };
        return SqlMerge::GroupedAggregate {
            how,
            post: MergePlan {
                node_sql: Some(node_sql),
                order_by,
                offset: query_offset(query),
                limit: query_limit(query),
                derived: Vec::new(),
                having,
                distinct_count: None,
                quantile: Some(spec),
                finish_sql: None,
            },
        };
    }

    // A distinct count reshapes the query rather than adjusting it, so it is
    // planned on its own before the ordinary aggregate paths.
    if columns
        .iter()
        .any(|c| matches!(c, ColumnMerge::DistinctCount(_)))
    {
        let (node_sql, how, spec) = match plan_distinct_count(query, select, &columns) {
            Ok(parts) => parts,
            Err(why) => return SqlMerge::Unsupported(why),
        };
        let order_by = match resolve_order_by(query, select) {
            Some(keys) => keys,
            None => return SqlMerge::Unsupported(UNORDERABLE.to_string()),
        };
        return SqlMerge::GroupedAggregate {
            how,
            post: MergePlan {
                node_sql: Some(node_sql),
                order_by,
                offset: query_offset(query),
                limit: query_limit(query),
                derived: Vec::new(),
                having,
                distinct_count: Some(spec),
                quantile: None,
                finish_sql: None,
            },
        };
    }

    // An AVG counts: it is an aggregate that produces one row per node, even
    // though it is carried across as a sum and a count rather than merged
    // directly. Leaving it out sent `SELECT AVG(x) AS a` down the row-wise path.
    let any_agg = columns
        .iter()
        .any(|c| matches!(c, ColumnMerge::Mergeable(_) | ColumnMerge::Derived(..)));

    if grouped || select.distinct.is_some() {
        let Some(order_by) = resolve_order_by(query, select) else {
            return SqlMerge::Unsupported(
                "ORDER BY cannot be applied across nodes unless every term is a projected \
                 column: the groups are ranked only once they have been combined, and the \
                 coordinator can only sort by values the query returned."
                    .to_string(),
            );
        };

        // A LIMIT applied by each node to its own groups can cut a group that is
        // large overall but small there, before the groups are ever combined —
        // so the nodes are sent the query without it and it lands once, on the
        // merged groups. An AVG is rewritten here too.
        let (projection, how, derived) = match rewrite_projection_for_derived(select, &columns) {
            Ok(parts) => parts,
            Err(why) => return SqlMerge::Unsupported(why),
        };
        let rewritten =
            !derived.is_empty() || having.is_some() || needs_rewrite(query) || normalized;

        return SqlMerge::GroupedAggregate {
            how,
            post: MergePlan {
                node_sql: if rewritten {
                    match sql_with_projection(query, projection, keep_node_ordering) {
                        Some(sql) => Some(sql),
                        None => return SqlMerge::Unsupported(UNREWRITABLE.to_string()),
                    }
                } else {
                    None
                },
                order_by,
                offset: query_offset(query),
                limit: query_limit(query),
                derived,
                having,
                distinct_count: None,
                quantile: None,
                finish_sql: None,
            },
        };
    }

    if !any_agg {
        let Some(order_by) = resolve_order_by(query, select) else {
            return SqlMerge::Unsupported(
                "ORDER BY cannot be applied across nodes unless every term is a projected \
                 column: each node sorted only its own partitions, and the coordinator can \
                 only re-sort by values the query actually returned."
                    .to_string(),
            );
        };

        // An OFFSET is the clause that most needs this: each node skipping its
        // own ten rows skips thirty, and none of them the ten the caller meant.
        return SqlMerge::Concatenate(MergePlan {
            node_sql: (needs_rewrite(query) || normalized)
                .then(|| sql_without_ordering(query, keep_node_ordering)),
            order_by,
            offset: query_offset(query),
            limit: query_limit(query),
            derived: Vec::new(),
            having: None,
            distinct_count: None,
            quantile: None,
            finish_sql: None,
        });
    }

    // Mixing aggregates with bare columns without GROUP BY is not valid SQL the
    // engine would accept, but if it did there is no sound merge.
    if columns.iter().any(|c| matches!(c, ColumnMerge::RowWise)) {
        return SqlMerge::Unsupported(
            "mixing aggregate and non-aggregate columns cannot be merged across nodes.".to_string(),
        );
    }

    let (projection, how, derived) = match rewrite_projection_for_derived(select, &columns) {
        Ok(parts) => parts,
        Err(why) => return SqlMerge::Unsupported(why),
    };
    // Normalized as well as rewritten: a scalar `HAVING` is applied here by
    // name, so the nodes have to return the name it was resolved to.
    let node_sql = if derived.is_empty() && !normalized {
        None
    } else {
        match sql_with_projection(query, projection, false) {
            Some(sql) => Some(sql),
            None => return SqlMerge::Unsupported(UNREWRITABLE.to_string()),
        }
    };

    SqlMerge::ScalarAggregate {
        how: how
            .into_iter()
            .map(|op| op.expect("aggregate columns only, checked above"))
            .collect(),
        post: MergePlan {
            node_sql,
            derived,
            having,
            ..Default::default()
        },
    }
}

/// Said when an ORDER BY names something the response will not carry.
const UNORDERABLE: &str =
    "ORDER BY cannot be applied across nodes unless every term is a projected column: the rows \
     are only whole once they reach the coordinator, which can sort only by what the query \
     returned.";

/// Said when a query parses but cannot be put back together with a replacement
/// projection — a shape the rewriter does not know how to edit.
const UNREWRITABLE: &str =
    "this query cannot be rewritten to merge across nodes: project the aggregate on its own \
     and combine in the client.";

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

/// The query's own `OFFSET`, if it is a plain number.
fn query_offset(query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query) -> Option<usize> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{Expr as SqlExpr, Value};
    match &query.offset.as_ref()?.value {
        SqlExpr::Value(Value::Number(n, _)) => n.parse().ok(),
        _ => None,
    }
}

/// Whether the nodes need a different query from the caller's.
///
/// Only when a clause exists that the nodes would apply to their own partitions
/// and the coordinator must apply once instead. With none of them present the
/// caller's SQL is already the right thing to send, and leaving it untouched
/// keeps the common query byte-identical to what it was before.
fn needs_rewrite(query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query) -> bool {
    query.order_by.is_some() || query.limit.is_some() || query.offset.is_some()
}
/// The query's own `LIMIT`, if it is a plain number.
fn query_limit(query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query) -> Option<usize> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{Expr as SqlExpr, Value};
    match query.limit.as_ref()? {
        SqlExpr::Value(Value::Number(n, _)) => n.parse().ok(),
        _ => None,
    }
}

/// A `HAVING` clause resolved against the response's own columns.
///
/// Held apart from the SQL because it is evaluated after the groups are
/// combined, not by the nodes: each node would otherwise filter on its own
/// partial count and drop groups that clear the threshold only once the
/// partials are added up.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Compare {
        column: String,
        op: CmpOp,
        value: serde_json::Value,
    },
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl Predicate {
    /// Whether a merged row survives the filter.
    pub fn admits(&self, row: &HashMap<String, serde_json::Value>) -> bool {
        match self {
            Predicate::And(a, b) => a.admits(row) && b.admits(row),
            Predicate::Or(a, b) => a.admits(row) || b.admits(row),
            Predicate::Not(inner) => !inner.admits(row),
            Predicate::Compare { column, op, value } => {
                let Some(actual) = row.get(column) else {
                    return false;
                };
                // SQL comparisons against NULL are unknown, which filters out.
                if actual.is_null() || value.is_null() {
                    return false;
                }
                let ord = compare_json(actual, value);
                match op {
                    CmpOp::Eq => ord == std::cmp::Ordering::Equal,
                    CmpOp::NotEq => ord != std::cmp::Ordering::Equal,
                    CmpOp::Lt => ord == std::cmp::Ordering::Less,
                    CmpOp::LtEq => ord != std::cmp::Ordering::Greater,
                    CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                    CmpOp::GtEq => ord != std::cmp::Ordering::Less,
                }
            }
        }
    }
}

/// Resolve a `HAVING` expression against the projected columns.
///
/// `HAVING COUNT(*) > 5` filters on a value the query also projects, so the
/// aggregate is matched to the column it was projected as and the comparison is
/// made there. Anything that cannot be tied to a projected column — or any
/// shape beyond comparisons joined by AND/OR/NOT — yields `None`, and the
/// caller turns that into a refusal rather than a guess.
fn resolve_having(
    expr: &chronik_columnar::datafusion::sql::sqlparser::ast::Expr,
    names: &[String],
    by_expr_text: &HashMap<String, String>,
) -> Option<Predicate> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        BinaryOperator, Expr as SqlExpr, UnaryOperator, Value,
    };

    match expr {
        SqlExpr::Nested(inner) => resolve_having(inner, names, by_expr_text),
        SqlExpr::UnaryOp {
            op: UnaryOperator::Not,
            expr: inner,
        } => Some(Predicate::Not(Box::new(resolve_having(
            inner,
            names,
            by_expr_text,
        )?))),
        SqlExpr::BinaryOp { left, op, right } => {
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) {
                let l = Box::new(resolve_having(left, names, by_expr_text)?);
                let r = Box::new(resolve_having(right, names, by_expr_text)?);
                return Some(match op {
                    BinaryOperator::And => Predicate::And(l, r),
                    _ => Predicate::Or(l, r),
                });
            }

            let cmp = match op {
                BinaryOperator::Eq => CmpOp::Eq,
                BinaryOperator::NotEq => CmpOp::NotEq,
                BinaryOperator::Lt => CmpOp::Lt,
                BinaryOperator::LtEq => CmpOp::LtEq,
                BinaryOperator::Gt => CmpOp::Gt,
                BinaryOperator::GtEq => CmpOp::GtEq,
                _ => return None,
            };

            // Either side may be the literal; `5 < COUNT(*)` means the same as
            // `COUNT(*) > 5` with the comparison turned around.
            if let Some(value) = json_literal(right) {
                let column = resolve_output_column(left, names, by_expr_text)?;
                return Some(Predicate::Compare {
                    column,
                    op: cmp,
                    value,
                });
            }
            let value = json_literal(left)?;
            let column = resolve_output_column(right, names, by_expr_text)?;
            let flipped = match cmp {
                CmpOp::Lt => CmpOp::Gt,
                CmpOp::LtEq => CmpOp::GtEq,
                CmpOp::Gt => CmpOp::Lt,
                CmpOp::GtEq => CmpOp::LtEq,
                other => other,
            };
            Some(Predicate::Compare {
                column,
                op: flipped,
                value,
            })
        }
        _ => {
            let _ = Value::Null;
            None
        }
    }
}

/// Tie an expression to the name it was projected under.
fn resolve_output_column(
    expr: &chronik_columnar::datafusion::sql::sqlparser::ast::Expr,
    names: &[String],
    by_expr_text: &HashMap<String, String>,
) -> Option<String> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::Expr as SqlExpr;
    if let SqlExpr::Identifier(id) = expr {
        if names.iter().any(|n| n.eq_ignore_ascii_case(&id.value)) {
            return Some(id.value.clone());
        }
    }
    by_expr_text.get(&expr.to_string()).cloned()
}

/// A literal as JSON, if that is what this expression is.
fn json_literal(
    expr: &chronik_columnar::datafusion::sql::sqlparser::ast::Expr,
) -> Option<serde_json::Value> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{Expr as SqlExpr, Value};
    match expr {
        SqlExpr::Nested(inner) => json_literal(inner),
        SqlExpr::Value(Value::Number(n, _)) => n.parse::<f64>().ok().and_then(|f| {
            if f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
                Some(serde_json::Value::from(f as i64))
            } else {
                serde_json::Number::from_f64(f).map(serde_json::Value::Number)
            }
        }),
        SqlExpr::Value(Value::SingleQuotedString(s) | Value::DoubleQuotedString(s)) => {
            Some(serde_json::Value::String(s.clone()))
        }
        SqlExpr::Value(Value::Boolean(b)) => Some(serde_json::Value::Bool(*b)),
        _ => None,
    }
}

/// The output name of every projected column, plus a lookup from the text of
/// the expression it came from.
#[allow(clippy::type_complexity)]
fn projection_names(
    select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select,
) -> Option<(Vec<String>, HashMap<String, String>)> {
    use chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem;

    let mut names = Vec::new();
    let mut by_expr_text = HashMap::new();
    for item in &select.projection {
        match item {
            SelectItem::ExprWithAlias { expr, alias } => {
                names.push(alias.value.clone());
                by_expr_text.insert(expr.to_string(), alias.value.clone());
            }
            SelectItem::UnnamedExpr(expr) => {
                let text = expr.to_string();
                names.push(text.clone());
                by_expr_text.insert(text.clone(), text);
            }
            // A wildcard's columns are not known from the SQL alone.
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => return None,
        }
    }
    Some((names, by_expr_text))
}

/// Give every unaliased projected column the name the planner resolved for it.
///
/// `ORDER BY`, `HAVING`, and the group keys of a reshaped query are all applied
/// by name over the merged rows, and the name the planner computes is the
/// parser's rendering of the expression — `COUNT(*)`, `UPPER(k)`. The engine
/// names those same columns `count(*)` and `upper(t.k)`, so every one of those
/// lookups missed, each one quietly:
///
/// - `ORDER BY COUNT(*) DESC LIMIT 10` found no such column, so the sort
///   compared null to null, did nothing, and the limit returned ten arbitrary
///   groups as the top ten.
/// - `HAVING COUNT(*) > 1` admitted no row at all, because a missing column
///   fails the comparison — the query returned empty.
/// - A quantile's or a distinct count's group keys all hashed to the same
///   identity, collapsing every group into one.
///
/// Asking the nodes for the name the planner already resolved makes the two
/// agree at the source, rather than teaching each of those three places to
/// guess what the engine would have called the column.
fn alias_bare_projection(
    select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select,
) -> (
    Vec<chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem>,
    bool,
) {
    use chronik_columnar::datafusion::sql::sqlparser::ast::{Expr as SqlExpr, Ident, SelectItem};

    let mut projection = Vec::with_capacity(select.projection.len());
    let mut changed = false;
    for item in &select.projection {
        match item {
            // A bare column is already named what the planner calls it, so it
            // is left exactly as the caller wrote it. Only the names the engine
            // would render differently need saying — `t.k` among them, which
            // the engine returns as `k`.
            SelectItem::UnnamedExpr(SqlExpr::Identifier(_)) => projection.push(item.clone()),
            SelectItem::UnnamedExpr(expr) => {
                changed = true;
                projection.push(SelectItem::ExprWithAlias {
                    expr: expr.clone(),
                    // Quoted, so a name like `COUNT(*)` is read as a name.
                    alias: Ident::with_quote('"', expr.to_string()),
                });
            }
            other => projection.push(other.clone()),
        }
    }
    (projection, changed)
}

/// Whether this projection returns columns the SQL does not name.
fn projection_has_wildcard(
    select: &chronik_columnar::datafusion::sql::sqlparser::ast::Select,
) -> bool {
    use chronik_columnar::datafusion::sql::sqlparser::ast::SelectItem;
    select.projection.iter().any(|item| {
        matches!(
            item,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
        )
    })
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
    let mut has_wildcard = false;
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
            // A wildcard's columns are not named by the SQL, but they are all
            // in the response, so a bare column name is still orderable — see
            // below. Refusing here turned `SELECT * FROM t ORDER BY ts LIMIT 10`
            // away, which is the commonest query there is against a stream.
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => has_wildcard = true,
        }
    }

    let mut keys = Vec::new();
    for term in exprs {
        let column = match &term.expr {
            SqlExpr::Identifier(id) => {
                let want = id.value.clone();
                if names.iter().any(|n| n.eq_ignore_ascii_case(&want)) {
                    want
                } else if has_wildcard {
                    // `SELECT *` returns every column of the topic, so a bare
                    // name is one of them or the query does not run at all: the
                    // nodes keep their own ORDER BY in this case precisely so
                    // the engine rejects a name that is not a column, rather
                    // than the coordinator sorting by a column that is absent.
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

/// Finish a merged result: everything the nodes could not do for themselves,
/// in the order SQL means it.
///
/// The derived come first because the rest may refer to them — `HAVING
/// AVG(x) > 5` and `ORDER BY avg_x` name a column that does not exist until the
/// sums and counts have been divided.
fn finish_rows(
    columns: &mut Vec<String>,
    rows: &mut Vec<HashMap<String, serde_json::Value>>,
    plan: &MergePlan,
) {
    finish_derived_rows(columns, rows, &plan.derived);
    // When the coordinator still has a query to run, the filtering and ordering
    // wait for it: they may name a column that does not exist until it has.
    if plan.finish_sql.is_some() {
        return;
    }
    // The distinct values have been de-duplicated across every node by now, so
    // what is left to do is count them.
    if let Some(spec) = plan.distinct_count.as_ref() {
        finish_distinct_count(columns, rows, spec);
    }
    // Likewise the counted values: they are the whole distribution now.
    if let Some(spec) = plan.quantile.as_ref() {
        finish_quantile(columns, rows, spec);
    }
    apply_finish(rows, plan);
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
pub fn apply_finish(rows: &mut Vec<HashMap<String, serde_json::Value>>, post: &MergePlan) {
    use serde_json::Value as J;
    use std::cmp::Ordering;

    // HAVING first, and only now that the groups are whole: a threshold applied
    // to a node's partial count drops groups that clear it once added up. It
    // comes before the sort so the sort is not done over rows that are about to
    // be dropped.
    if let Some(predicate) = post.having.as_ref() {
        rows.retain(|row| predicate.admits(row));
    }

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

    // Offset before limit, and both over the union: the nodes ran a query with
    // these clauses removed precisely so that they land once, here.
    if let Some(offset) = post.offset {
        if offset >= rows.len() {
            rows.clear();
        } else {
            rows.drain(..offset);
        }
    }

    if let Some(limit) = post.limit {
        rows.truncate(limit);
    }
}

/// Drop the `HAVING` from a query bound for the nodes.
///
/// The threshold is meant for the combined groups. Left in, each node applies
/// it to its own partial count, so a group that only clears the bar once the
/// partials are added up is dropped before anyone can add them up.
fn strip_having(query: &mut chronik_columnar::datafusion::sql::sqlparser::ast::Query) {
    use chronik_columnar::datafusion::sql::sqlparser::ast::SetExpr;
    if let SetExpr::Select(select) = query.body.as_mut() {
        select.having = None;
    }
}
/// The caller's query with `ORDER BY`, `LIMIT` and `OFFSET` removed.
///
/// Those clauses mean something different on a node than they do to the caller:
/// applied to one node's partitions they rank and cut the wrong set. Removing
/// them makes each node return its full contribution, which is what lets the
/// coordinator apply them once over the union. The `ORDER BY` goes too — it
/// costs each node a sort whose result is discarded by the merge.
/// `keep_order` leaves the `ORDER BY` in place. That is for a `SELECT *`, whose
/// sort column the SQL never names: letting the nodes sort makes the engine
/// reject a column that does not exist, instead of the coordinator sorting by
/// an absent column and quietly returning the rows unsorted.
fn sql_without_ordering(
    query: &chronik_columnar::datafusion::sql::sqlparser::ast::Query,
    keep_order: bool,
) -> String {
    let mut stripped = query.clone();
    if !keep_order {
        stripped.order_by = None;
    }
    stripped.limit = None;
    stripped.offset = None;
    strip_having(&mut stripped);
    stripped.to_string()
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

/// Finish a merged result by running the rest of the query over it.
///
/// The nodes were asked only for things that combine; what is left — the shape
/// wrapped around those aggregates — is applied here, once, to the combined
/// values. It is executed by the query engine rather than by an evaluator
/// written for the purpose, so the arithmetic, the null handling and the type
/// coercion are the same ones the query would have got on a single node.
///
/// Only the merged rows are involved, which is one row, or one per group.
pub async fn run_finish_sql(
    response: SqlResponse,
    finish_sql: &str,
) -> Result<SqlResponse, String> {
    use chronik_columnar::datafusion::prelude::SessionContext;

    let execution_time_ms = response.execution_time_ms;
    let truncated = response.truncated;

    let batch = rows_to_record_batch(&response.columns, &response.rows)?;
    let ctx = SessionContext::new();
    chronik_columnar::udfs::register_udfs(&ctx)
        .map_err(|e| format!("could not prepare the coordinator's query context: {e}"))?;
    ctx.register_batch(MERGED_TABLE, batch)
        .map_err(|e| format!("could not stage the merged rows: {e}"))?;

    let batches = ctx
        .sql(finish_sql)
        .await
        .map_err(|e| format!("could not plan the final step of this query: {e}"))?
        .collect()
        .await
        .map_err(|e| format!("could not run the final step of this query: {e}"))?;

    let mut columns = Vec::new();
    let mut rows = Vec::new();
    for batch in &batches {
        if columns.is_empty() {
            columns = batch
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
        }
        for row_idx in 0..batch.num_rows() {
            let mut row = HashMap::new();
            for (col_idx, name) in columns.iter().enumerate() {
                row.insert(
                    name.clone(),
                    super::sql_handler::arrow_value_to_json(batch.column(col_idx), row_idx),
                );
            }
            rows.push(row);
        }
    }

    let row_count = rows.len();
    Ok(SqlResponse {
        columns,
        rows,
        row_count,
        execution_time_ms,
        truncated,
    })
}

/// Build an Arrow batch from merged rows.
///
/// The type of each column is taken from the values actually present: the
/// merged rows are JSON and carry no schema of their own. Integers stay
/// integers so a summed count does not come back as `60.0`, and a column that
/// mixes shapes falls back to text rather than guessing which one was meant.
fn rows_to_record_batch(
    columns: &[String],
    rows: &[HashMap<String, serde_json::Value>],
) -> Result<chronik_columnar::datafusion::arrow::record_batch::RecordBatch, String> {
    use chronik_columnar::datafusion::arrow::array::{
        ArrayRef, BooleanArray, Float64Array, Int64Array, StringArray,
    };
    use chronik_columnar::datafusion::arrow::datatypes::{DataType, Field, Schema};
    use chronik_columnar::datafusion::arrow::record_batch::RecordBatch;
    use serde_json::Value as J;
    use std::sync::Arc;

    let mut fields = Vec::with_capacity(columns.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());

    for name in columns {
        let values: Vec<&J> = rows
            .iter()
            .map(|r| r.get(name).unwrap_or(&J::Null))
            .collect();

        let (kind, nullable) = column_kind(&values);
        let array: ArrayRef = match kind {
            DataType::Int64 => Arc::new(values.iter().map(|v| v.as_i64()).collect::<Int64Array>()),
            DataType::Float64 => {
                Arc::new(values.iter().map(|v| v.as_f64()).collect::<Float64Array>())
            }
            DataType::Boolean => {
                Arc::new(values.iter().map(|v| v.as_bool()).collect::<BooleanArray>())
            }
            _ => Arc::new(
                values
                    .iter()
                    .map(|v| match v {
                        J::Null => None,
                        J::String(s) => Some(s.clone()),
                        other => Some(other.to_string()),
                    })
                    .collect::<StringArray>(),
            ),
        };
        fields.push(Field::new(name, array.data_type().clone(), nullable));
        arrays.push(array);
    }

    if arrays.is_empty() {
        return Err("the merged result has no columns to finish over".to_string());
    }

    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .map_err(|e| format!("could not stage the merged rows: {e}"))
}

/// Decide one column's type from the values in it.
fn column_kind(
    values: &[&serde_json::Value],
) -> (
    chronik_columnar::datafusion::arrow::datatypes::DataType,
    bool,
) {
    use chronik_columnar::datafusion::arrow::datatypes::DataType;
    use serde_json::Value as J;

    let mut nullable = false;
    let (mut ints, mut floats, mut bools, mut strings) = (0, 0, 0, 0);
    for value in values {
        match value {
            J::Null => nullable = true,
            J::Number(n) if n.is_f64() && !n.is_i64() => floats += 1,
            J::Number(_) => ints += 1,
            J::Bool(_) => bools += 1,
            _ => strings += 1,
        }
    }

    let kind = match (ints, floats, bools, strings) {
        (_, 0, 0, 0) => DataType::Int64,
        // Integers and floats together are one numeric column.
        (_, _, 0, 0) => DataType::Float64,
        (0, 0, _, 0) => DataType::Boolean,
        _ => DataType::Utf8,
    };
    (kind, nullable)
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
    post: &MergePlan,
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

    // The query's own HAVING and ORDER BY apply to the combined groups, not to
    // any one node's partial view of them.
    finish_rows(&mut columns, &mut rows, post);

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

        let merged = merge_sql_responses(local, vec![peer], 100, &MergePlan::default());
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

        let merged = merge_sql_responses(local, vec![peer], 2, &MergePlan::default()); // limit=2
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
            SqlMerge::Concatenate(MergePlan::default())
        );
        assert_eq!(
            sql_merge_strategy("SELECT * FROM orders"),
            SqlMerge::Concatenate(MergePlan::default())
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
        let merged = merge_sql_responses(local, vec![peer], 100, &MergePlan::default());
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
            scalar_ops("SELECT COUNT(*) FROM orders"),
            vec![AggMerge::Sum]
        );
        assert_eq!(
            scalar_ops("SELECT SUM(amount), MIN(amount), MAX(amount) FROM orders"),
            vec![AggMerge::Sum, AggMerge::Min, AggMerge::Max]
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
    /// The average of per-node derived equals the true average only when every
    /// node holds the same number of rows, which a partitioned topic does not
    /// guarantee. So the nodes are asked for the sum and the count instead, and
    /// the division happens once — exact however the rows are spread.
    #[test]
    fn avg_is_carried_as_a_sum_and_a_count() {
        let SqlMerge::ScalarAggregate { how, post } =
            sql_merge_strategy("SELECT AVG(amount) AS avg_amount FROM orders")
        else {
            panic!("AVG should now be answered");
        };
        assert_eq!(how, vec![AggMerge::Sum, AggMerge::Sum]);
        let node_sql = post
            .node_sql
            .as_deref()
            .expect("nodes get a rewritten query");
        assert!(
            node_sql.contains("SUM(amount)") && node_sql.contains("COUNT(amount)"),
            "the nodes must be asked for a sum and a count, got {node_sql}"
        );
        assert!(
            !node_sql.contains("AVG"),
            "no node should compute an average of its own rows"
        );

        // Two nodes holding very different row counts: the naive mean of the
        // per-node derived would be 15, the true average is 12.
        let avg = &post.derived[0];
        let mut merged = SqlResponse {
            columns: vec![avg.sum.clone(), avg.count.clone()],
            rows: vec![HashMap::from([
                (avg.sum.clone(), serde_json::Value::from(1200)),
                (avg.count.clone(), serde_json::Value::from(100)),
            ])],
            row_count: 1,
            execution_time_ms: 0,
            truncated: false,
        };
        finish_derived(&mut merged, &post.derived);
        assert_eq!(merged.columns, vec!["avg_amount".to_string()]);
        assert_eq!(merged.rows[0]["avg_amount"], serde_json::Value::from(12.0));
    }

    /// Dividing by a zero count is no rows, not zero.
    #[test]
    fn an_average_over_no_rows_is_null() {
        let derived = vec![DerivedColumn {
            output: "a".into(),
            kind: Derived::Average,
            sum: "s".into(),
            count: "n".into(),
            sum_sq: None,
        }];
        let mut merged = SqlResponse {
            columns: vec!["s".into(), "n".into()],
            rows: vec![HashMap::from([
                ("s".to_string(), serde_json::Value::Null),
                ("n".to_string(), serde_json::Value::from(0)),
            ])],
            row_count: 1,
            execution_time_ms: 0,
            truncated: false,
        };
        finish_derived(&mut merged, &derived);
        assert!(merged.rows[0]["a"].is_null());
    }

    /// An unnamed AVG used to be refused: the nodes are asked for a sum and a
    /// count, so the column the engine would have generated is for an
    /// expression that is no longer in the query, and there was nowhere to put
    /// the average back. The nodes are now asked for that name too, so the
    /// average comes back under the expression the caller wrote.
    #[test]
    fn an_unnamed_avg_is_answered_under_the_name_the_caller_wrote() {
        let SqlMerge::ScalarAggregate { post, .. } =
            sql_merge_strategy("SELECT AVG(amount) FROM orders")
        else {
            panic!("an unnamed average is answered, not refused");
        };
        assert_eq!(post.derived.len(), 1);
        assert_eq!(post.derived[0].output, "AVG(amount)");
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
        let merged = merge_grouped_aggregate(local, peers, &how, &MergePlan::default(), 1000);
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
        let merged = merge_grouped_aggregate(local, peers, &how, &MergePlan::default(), 1000);
        assert_eq!(merged.rows.len(), 2, "k=1 was held by all three nodes");
    }

    /// A per-node LIMIT drops groups before the groups combine, so the nodes are
    /// sent the query without it and it lands on the merged groups instead.
    /// This is the top-N-by-group every grouped dashboard is made of.
    #[test]
    fn group_by_with_a_limit_runs_the_nodes_without_it() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT k, COUNT(*) AS c FROM t GROUP BY k ORDER BY c DESC LIMIT 2")
        else {
            panic!("GROUP BY with LIMIT should now be answered");
        };
        assert_eq!(
            post.node_sql.as_deref(),
            Some("SELECT k, COUNT(*) AS c FROM t GROUP BY k"),
            "the nodes must not apply the caller's ORDER BY or LIMIT"
        );
        assert_eq!(post.limit, Some(2));

        // k=2 is the largest group overall (11) while being the smaller of the
        // two groups on one node, which is exactly what a per-node LIMIT 2
        // would have thrown away.
        let local = resp(&["k", "c"], vec![row(&[("k", 1), ("c", 4)])]);
        let peers = vec![
            resp(
                &["k", "c"],
                vec![row(&[("k", 2), ("c", 9)]), row(&[("k", 3), ("c", 1)])],
            ),
            resp(
                &["k", "c"],
                vec![row(&[("k", 2), ("c", 2)]), row(&[("k", 1), ("c", 1)])],
            ),
        ];
        let merged = merge_grouped_aggregate(local, peers, &how, &post, 1000);
        let ranked: Vec<(i64, i64)> = merged
            .rows
            .iter()
            .map(|r| (r["k"].as_i64().unwrap(), r["c"].as_i64().unwrap()))
            .collect();
        assert_eq!(ranked, vec![(2, 11), (1, 5)], "the top two groups overall");
    }

    /// A threshold belongs to the combined groups. Left with the nodes, a group
    /// of 4 + 4 would be dropped by both for failing `> 5`, when together it is
    /// 8 and clears it.
    #[test]
    fn having_filters_the_combined_groups_not_each_node_s() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT k, COUNT(*) AS c FROM t GROUP BY k HAVING COUNT(*) > 5")
        else {
            panic!("GROUP BY with HAVING should now be answered");
        };
        assert_eq!(
            post.node_sql.as_deref(),
            Some("SELECT k, COUNT(*) AS c FROM t GROUP BY k"),
            "the nodes must not apply the threshold to their own partials"
        );

        let local = resp(&["k", "c"], vec![row(&[("k", 1), ("c", 4)])]);
        let peers = vec![
            resp(
                &["k", "c"],
                vec![row(&[("k", 1), ("c", 4)]), row(&[("k", 2), ("c", 2)])],
            ),
            resp(&["k", "c"], vec![row(&[("k", 2), ("c", 1)])]),
        ];
        let merged = merge_grouped_aggregate(local, peers, &how, &post, 1000);
        let kept: Vec<(i64, i64)> = merged
            .rows
            .iter()
            .map(|r| (r["k"].as_i64().unwrap(), r["c"].as_i64().unwrap()))
            .collect();
        assert_eq!(
            kept,
            vec![(1, 8)],
            "k=1 is 8 once combined and stays; k=2 is 3 and goes"
        );
    }

    /// The threshold may be written either way round, and joined with AND/OR.
    #[test]
    fn having_handles_the_comparison_either_way_round() {
        let plan = |sql: &str| match sql_merge_strategy(sql) {
            SqlMerge::GroupedAggregate { post, .. } => post.having.expect("a predicate"),
            other => panic!("expected a grouped merge, got {other:?}"),
        };

        let row_of = |c: i64| {
            HashMap::from([
                ("k".to_string(), serde_json::Value::from(1)),
                ("c".to_string(), serde_json::Value::from(c)),
            ])
        };

        let gt = plan("SELECT k, COUNT(*) AS c FROM t GROUP BY k HAVING COUNT(*) > 5");
        assert!(gt.admits(&row_of(6)) && !gt.admits(&row_of(5)));

        // `5 < COUNT(*)` is the same threshold written backwards.
        let flipped = plan("SELECT k, COUNT(*) AS c FROM t GROUP BY k HAVING 5 < COUNT(*)");
        assert!(flipped.admits(&row_of(6)) && !flipped.admits(&row_of(5)));

        // Filtering on the projected name works too.
        let by_alias = plan("SELECT k, COUNT(*) AS c FROM t GROUP BY k HAVING c >= 6");
        assert!(by_alias.admits(&row_of(6)) && !by_alias.admits(&row_of(5)));

        let ranged = plan("SELECT k, COUNT(*) AS c FROM t GROUP BY k HAVING c > 2 AND c < 10");
        assert!(
            ranged.admits(&row_of(6)) && !ranged.admits(&row_of(1)) && !ranged.admits(&row_of(11))
        );
    }

    /// A HAVING that names something the query does not project cannot be
    /// applied here, and is said so rather than dropped.
    #[test]
    fn a_having_on_an_unprojected_aggregate_is_refused() {
        assert!(refusal("SELECT k FROM t GROUP BY k HAVING SUM(amount) > 5").contains("HAVING"));
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

    fn scalar_ops(sql: &str) -> Vec<AggMerge> {
        match sql_merge_strategy(sql) {
            SqlMerge::ScalarAggregate { how, .. } => how,
            other => panic!("expected a scalar aggregate for {sql}, got {other:?}"),
        }
    }

    /// `COUNT(DISTINCT sev)` returned **9** where the answer was 3: the matcher
    /// keyed on the function name and never looked at the DISTINCT, so three
    /// nodes' de-duplicated counts were summed.
    ///
    /// It is not summed now — it is rebuilt. The nodes return the distinct
    /// values they hold, the union is de-duplicated, and counting happens once.
    #[test]
    fn count_distinct_is_rebuilt_from_the_values() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT COUNT(DISTINCT sev) AS c FROM t")
        else {
            panic!("COUNT(DISTINCT) should now be answered");
        };
        assert_eq!(how, vec![None], "the value is a key of the de-duplication");
        let spec = post.distinct_count.as_ref().expect("a distinct-count plan");
        assert_eq!(spec.output, "c");
        assert!(
            spec.keys.is_empty(),
            "no GROUP BY, so one count for the topic"
        );

        let node_sql = post.node_sql.as_deref().expect("a rewritten query");
        assert!(
            node_sql.starts_with("SELECT DISTINCT sev AS "),
            "the nodes must be asked for values, not a count: {node_sql}"
        );

        // sev=a is held by two of the three nodes and must still count once.
        let value = &spec.value;
        let of = |v: &str| HashMap::from([(value.clone(), serde_json::Value::from(v))]);
        let as_resp = |rows: Vec<HashMap<String, serde_json::Value>>| SqlResponse {
            columns: vec![value.clone()],
            rows,
            row_count: 0,
            execution_time_ms: 0,
            truncated: false,
        };
        let merged = merge_grouped_aggregate(
            as_resp(vec![of("a"), of("b")]),
            vec![as_resp(vec![of("a")]), as_resp(vec![of("c")])],
            &how,
            &post,
            1000,
        );
        assert_eq!(merged.columns, vec!["c".to_string()]);
        assert_eq!(merged.rows[0]["c"], serde_json::Value::from(3));
    }

    /// Counted within a GROUP BY, the values are de-duplicated per key.
    #[test]
    fn count_distinct_within_a_group_counts_per_key() {
        let SqlMerge::GroupedAggregate { how, post } = sql_merge_strategy(
            "SELECT day AS day, COUNT(DISTINCT user) AS users FROM t GROUP BY day",
        ) else {
            panic!("grouped COUNT(DISTINCT) should be answered");
        };
        let spec = post.distinct_count.as_ref().expect("a distinct-count plan");
        assert_eq!(spec.keys, vec!["day".to_string()]);

        let value = spec.value.clone();
        let pair = |day: i64, user: &str| {
            HashMap::from([
                ("day".to_string(), serde_json::Value::from(day)),
                (value.clone(), serde_json::Value::from(user)),
            ])
        };
        let as_resp = |rows: Vec<HashMap<String, serde_json::Value>>| SqlResponse {
            columns: vec!["day".to_string(), value.clone()],
            rows,
            row_count: 0,
            execution_time_ms: 0,
            truncated: false,
        };
        // ann appears on day 1 from two nodes; day 2 has only bob.
        let merged = merge_grouped_aggregate(
            as_resp(vec![pair(1, "ann"), pair(2, "bob")]),
            vec![as_resp(vec![pair(1, "ann"), pair(1, "cat")])],
            &how,
            &post,
            1000,
        );
        let mut got: Vec<(i64, i64)> = merged
            .rows
            .iter()
            .map(|r| (r["day"].as_i64().unwrap(), r["users"].as_i64().unwrap()))
            .collect();
        got.sort();
        assert_eq!(got, vec![(1, 2), (2, 1)]);
    }

    /// Counting distinct values alongside another aggregate would compute that
    /// aggregate over the de-duplicated values instead of the rows.
    #[test]
    fn count_distinct_beside_another_aggregate_is_refused() {
        assert!(
            refusal("SELECT COUNT(*) AS n, COUNT(DISTINCT x) AS d FROM t")
                .contains("alongside another aggregate")
        );
        assert!(
            refusal("SELECT COUNT(DISTINCT x) AS a, COUNT(DISTINCT y) AS b FROM t")
                .contains("one query at a time")
        );
    }

    /// `median`, `stddev`, `approx_percentile_cont`, `array_agg` … every
    /// aggregate the old four-name matcher did not know was treated as a
    /// row-wise expression and its per-node results concatenated — one row per
    /// node where the caller asked for one number.
    #[test]
    fn aggregates_that_cannot_be_combined_are_refused() {
        for sql in [
            "SELECT approx_distinct(x) AS d FROM t",
            "SELECT array_agg(x) AS a FROM t",
            "SELECT corr(x, y) AS c FROM t",
            "SELECT first_value(x) AS f FROM t",
        ] {
            let _ = refusal(sql);
        }
    }

    /// A variance needs no sketch and no second pass: the sum, the sum of
    /// squares and the count are each summable, and `E[x²] - E[x]²` does the
    /// rest. So the nodes are asked for those three and the answer is exact —
    /// the same number one node holding every row would have produced.
    #[test]
    fn variance_and_stddev_are_rebuilt_from_running_totals() {
        for (sql, kind) in [
            ("SELECT stddev(x) AS s FROM t", Derived::StddevSample),
            (
                "SELECT stddev_pop(x) AS s FROM t",
                Derived::StddevPopulation,
            ),
            ("SELECT var(x) AS s FROM t", Derived::VarSample),
            ("SELECT var_pop(x) AS s FROM t", Derived::VarPopulation),
        ] {
            let SqlMerge::ScalarAggregate { how, post } = sql_merge_strategy(sql) else {
                panic!("{sql} should now be answered");
            };
            assert_eq!(how, vec![AggMerge::Sum; 3], "sum, count and sum of squares");
            assert_eq!(post.derived[0].kind, kind);
            let node_sql = post.node_sql.as_deref().expect("a rewritten query");
            assert!(
                node_sql.contains("SUM((x) * (x))"),
                "the nodes must be asked for the squares, got {node_sql}"
            );
        }
    }

    /// Checked against values worked out by hand: 2, 4, 4, 4, 5, 5, 7, 9 has
    /// mean 5, population variance 4 and sample variance 32/7.
    #[test]
    fn the_variance_formula_matches_the_textbook() {
        let (sum, sum_sq, n) = (40.0, 232.0, 8.0);
        assert_eq!(Derived::Average.combine(sum, sum_sq, n), Some(5.0));
        assert_eq!(Derived::VarPopulation.combine(sum, sum_sq, n), Some(4.0));
        assert_eq!(Derived::StddevPopulation.combine(sum, sum_sq, n), Some(2.0));
        assert_eq!(Derived::VarSample.combine(sum, sum_sq, n), Some(32.0 / 7.0));

        // A single row has no sample variance, and no rows has nothing at all.
        assert_eq!(Derived::VarSample.combine(3.0, 9.0, 1.0), None);
        assert_eq!(Derived::Average.combine(0.0, 0.0, 0.0), None);

        // Identical values must not come out as a negative variance.
        assert_eq!(Derived::VarPopulation.combine(9.0, 27.0, 3.0), Some(0.0));
    }

    /// The sharpest one: AVG was correctly refused, and wrapping it in any
    /// function walked straight past that refusal. `ROUND(AVG(n),2)` returned
    /// 8.54, 8.63, 8.31 for a column whose average was 8.5.
    ///
    /// It does not evade anything now because there is nothing to evade: the
    /// wrapping is taken off, the aggregate underneath is merged properly, and
    /// the wrapping goes back on over the combined value.
    #[test]
    fn wrapping_an_aggregate_no_longer_evades_the_merge() {
        for sql in [
            "SELECT ROUND(AVG(x), 2) AS a FROM t",
            "SELECT SUM(x) * 2 AS s FROM t",
            "SELECT CAST(SUM(x) AS BIGINT) AS s FROM t",
        ] {
            let SqlMerge::GroupedAggregate { post, .. } = sql_merge_strategy(sql) else {
                panic!("{sql} should be taken apart, not refused");
            };
            let node_sql = post.node_sql.as_deref().expect("a rewritten query");
            assert!(
                !node_sql.contains("ROUND") && !node_sql.contains("CAST"),
                "the wrapping must not go to the nodes: {node_sql}"
            );
            assert!(
                post.finish_sql.is_some(),
                "and must be applied here instead"
            );
        }
    }

    /// `SUM(x)/COUNT(x)` is what the AVG refusal used to tell callers to write,
    /// and it was itself unguarded: three nodes, three ratios, none of them the
    /// answer. Several aggregates in one column are no harder than one — each
    /// is asked for separately and the expression joins the combined values.
    #[test]
    fn several_aggregates_in_one_column_are_each_merged() {
        for sql in [
            "SELECT SUM(x)/COUNT(x) AS r FROM t",
            "SELECT SUM(x) + SUM(y) AS s FROM t",
        ] {
            let SqlMerge::GroupedAggregate { how, post } = sql_merge_strategy(sql) else {
                panic!("{sql} should be taken apart, not refused");
            };
            assert_eq!(
                how,
                vec![Some(AggMerge::Sum), Some(AggMerge::Sum)],
                "both aggregates go to the nodes on their own"
            );
            assert!(post.finish_sql.is_some());
        }
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

    /// Three nodes each skipping their own ten rows skips thirty, and none of
    /// them the ten the caller meant. The nodes run without it and the page is
    /// taken once, from the ordered union.
    #[test]
    fn offset_is_taken_once_over_the_merged_rows() {
        let SqlMerge::Concatenate(post) =
            sql_merge_strategy("SELECT k FROM t ORDER BY k LIMIT 2 OFFSET 3")
        else {
            panic!("OFFSET should now be answered");
        };
        assert_eq!(post.node_sql.as_deref(), Some("SELECT k FROM t"));
        assert_eq!((post.offset, post.limit), (Some(3), Some(2)));

        let local = resp(&["k"], vec![row(&[("k", 1)]), row(&[("k", 4)])]);
        let peers = vec![
            resp(&["k"], vec![row(&[("k", 2)]), row(&[("k", 5)])]),
            resp(&["k"], vec![row(&[("k", 3)]), row(&[("k", 6)])]),
        ];
        let merged = merge_sql_responses(local, peers, 1000, &post);
        let ks: Vec<i64> = merged
            .rows
            .iter()
            .map(|r| r["k"].as_i64().unwrap())
            .collect();
        assert_eq!(ks, vec![4, 5], "rows 4 and 5 of 1..6, not each node's own");
    }

    /// The coordinator can only sort by values the query actually returned.
    ///
    /// A wildcard *does* return them, which is why it is no longer refused —
    /// see [`planner_name_tests::a_wildcard_can_be_ordered_by_a_column_it_returns`](super::planner_name_tests).
    #[test]
    fn ordering_by_an_unprojected_column_is_refused() {
        assert!(refusal("SELECT k FROM t ORDER BY ts DESC LIMIT 5").contains("ORDER BY"));
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
        apply_finish(
            &mut rows,
            &MergePlan {
                order_by: vec![OrderKey {
                    column: "v".into(),
                    descending: false,
                    nulls_first: false,
                }],
                ..Default::default()
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
            scalar_ops("SELECT COUNT(*) AS c, SUM(x) AS s FROM t"),
            vec![AggMerge::Sum, AggMerge::Sum]
        );
        assert_eq!(
            scalar_ops("SELECT MIN(x) AS a, MAX(x) AS b FROM t"),
            vec![AggMerge::Min, AggMerge::Max]
        );
        assert_eq!(
            scalar_ops("SELECT COUNT(*) AS c FROM t WHERE sev = 'ERROR'"),
            vec![AggMerge::Sum]
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
            scalar_ops("SELECT COUNT(*) AS c FROM logs a JOIN logs b ON a.id = b.id"),
            vec![AggMerge::Sum]
        );
        assert_eq!(
            scalar_ops("WITH a AS (SELECT id FROM logs) SELECT COUNT(*) AS c FROM a"),
            vec![AggMerge::Sum]
        );
    }

    /// A grouped AVG must survive the merge with its value intact.
    ///
    /// It did not: the division ran inside the merge and then again in the
    /// handler, where the sum and count columns had already been consumed, so
    /// the second pass overwrote every value with null. Only the cluster caught
    /// it, which is why it is pinned here.
    #[test]
    fn a_grouped_average_is_divided_exactly_once() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT k, AVG(x) AS avg_x FROM t GROUP BY k")
        else {
            panic!("grouped AVG should be answered");
        };

        let d = &post.derived[0];
        let group = |k: i64, sum: i64, count: i64| {
            HashMap::from([
                ("k".to_string(), serde_json::Value::from(k)),
                (d.sum.clone(), serde_json::Value::from(sum)),
                (d.count.clone(), serde_json::Value::from(count)),
            ])
        };
        let cols = vec!["k".to_string(), d.sum.clone(), d.count.clone()];
        let as_resp = |rows: Vec<HashMap<String, serde_json::Value>>| SqlResponse {
            columns: cols.clone(),
            rows,
            row_count: 0,
            execution_time_ms: 0,
            truncated: false,
        };

        // 30 over 3 rows here, 70 over 7 there: the true average is 10, while
        // averaging the per-node averages would give 10 only by luck — use
        // lopsided values so the difference shows.
        let merged = merge_grouped_aggregate(
            as_resp(vec![group(1, 30, 3)]),
            vec![as_resp(vec![group(1, 170, 7)])],
            &how,
            &post,
            1000,
        );

        assert_eq!(merged.rows.len(), 1);
        assert_eq!(
            merged.rows[0]["avg_x"],
            serde_json::Value::from(20.0),
            "200 over 10 rows, not the mean of 10 and 24.3"
        );
        assert_eq!(merged.columns, vec!["k".to_string(), "avg_x".to_string()]);
    }

    /// A quantile is not derivable from per-node quantiles — the median of
    /// three medians is not the median — but it is derivable from how often
    /// each value occurs, and counts do combine. So the nodes are asked for a
    /// frequency table and the answer is exact, not estimated.
    #[test]
    fn a_quantile_is_rebuilt_from_value_counts() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT median(x) AS m FROM t")
        else {
            panic!("median should now be answered");
        };
        assert_eq!(
            how,
            vec![None, Some(AggMerge::Sum)],
            "value, then its count"
        );

        let spec = post.quantile.as_ref().expect("a quantile plan");
        assert_eq!(spec.fraction, 0.5);
        let node_sql = post.node_sql.as_deref().expect("a rewritten query");
        assert!(
            node_sql.contains("COUNT(*)") && node_sql.contains("GROUP BY x"),
            "the nodes must be asked for a frequency table: {node_sql}"
        );
        assert!(
            !node_sql.contains("median"),
            "no node should take a median of its own rows"
        );

        // 1 once, 2 twice, 3 once across three nodes: 1,2,2,3 has median 2.
        let (v, c) = (spec.value.clone(), spec.count.clone());
        let bucket = |value: i64, count: i64| {
            HashMap::from([
                (v.clone(), serde_json::Value::from(value)),
                (c.clone(), serde_json::Value::from(count)),
            ])
        };
        let as_resp = |rows: Vec<HashMap<String, serde_json::Value>>| SqlResponse {
            columns: vec![v.clone(), c.clone()],
            rows,
            row_count: 0,
            execution_time_ms: 0,
            truncated: false,
        };
        let merged = merge_grouped_aggregate(
            as_resp(vec![bucket(1, 1), bucket(2, 1)]),
            vec![as_resp(vec![bucket(2, 1)]), as_resp(vec![bucket(3, 1)])],
            &how,
            &post,
            1000,
        );
        assert_eq!(merged.columns, vec!["m".to_string()]);
        assert_eq!(merged.rows[0]["m"], serde_json::Value::from(2.0));
    }

    /// Worked out by hand against the definition `percentile_cont` uses, so an
    /// off-by-one in the walk over cumulative counts cannot hide.
    #[test]
    fn the_quantile_walk_matches_percentile_cont() {
        // 10, 20, 30, 40 — position is fraction * (n - 1), interpolated.
        let d = || vec![(10.0, 1.0), (20.0, 1.0), (30.0, 1.0), (40.0, 1.0)];
        assert_eq!(quantile_of(d(), 0.0), Some(10.0));
        assert_eq!(quantile_of(d(), 1.0), Some(40.0));
        assert_eq!(
            quantile_of(d(), 0.5),
            Some(25.0),
            "halfway between 20 and 30"
        );
        assert_eq!(quantile_of(d(), 0.25), Some(17.5));

        // Repeated values must count as many times as they occur: 1,1,1,9 has
        // median 1, not 5.
        assert_eq!(quantile_of(vec![(1.0, 3.0), (9.0, 1.0)], 0.5), Some(1.0));

        // Order of arrival must not matter — the nodes answer in any order.
        assert_eq!(quantile_of(vec![(9.0, 1.0), (1.0, 3.0)], 0.5), Some(1.0));

        assert_eq!(quantile_of(Vec::new(), 0.5), None);
    }

    /// The fraction has to be a literal, because it is applied here rather than
    /// by the nodes.
    ///
    /// An unnamed quantile used to be refused too, for want of a name to put
    /// the answer back under. The nodes are now asked for that name, so
    /// `SELECT median(x) FROM t` is answered — see
    /// [`planner_name_tests`](super::planner_name_tests).
    #[test]
    fn a_quantile_needs_a_literal_fraction() {
        assert!(
            refusal("SELECT approx_percentile_cont(x, y) AS p FROM t").contains("literal fraction")
        );

        let SqlMerge::GroupedAggregate { post, .. } = sql_merge_strategy("SELECT median(x) FROM t")
        else {
            panic!("an unnamed median is answered under the name the caller wrote");
        };
        let spec = post.quantile.as_ref().expect("a quantile plan");
        assert_eq!(spec.output, "median(x)");
        assert!(spec.keys.is_empty(), "no GROUP BY means one distribution");
    }

    /// `ROUND(AVG(x), 2)` walked past the AVG refusal and returned one average
    /// per node. The wrapping cannot be merged, but what it wraps can: the
    /// nodes are asked for the sum and the count, and the rounding is applied
    /// once, here.
    #[test]
    fn an_aggregate_inside_an_expression_is_taken_apart() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT ROUND(AVG(x), 2) AS a FROM t")
        else {
            panic!("a wrapped aggregate should now be answered");
        };

        let node_sql = post.node_sql.as_deref().expect("a rewritten query");
        assert!(
            node_sql.contains("SUM(x)") && node_sql.contains("COUNT(x)"),
            "the nodes must be asked for the totals: {node_sql}"
        );
        assert!(
            !node_sql.contains("ROUND"),
            "no node should round its own partial: {node_sql}"
        );
        assert_eq!(how, vec![Some(AggMerge::Sum), Some(AggMerge::Sum)]);

        let finish = post.finish_sql.as_deref().expect("a finishing query");
        assert!(
            finish.contains("ROUND") && finish.contains(MERGED_TABLE),
            "the rounding belongs to the coordinator: {finish}"
        );
    }

    /// `SUM(x)/COUNT(x)` is what the old AVG refusal told callers to write, and
    /// it was unguarded. Both aggregates are now projected and the division is
    /// done over the totals.
    #[test]
    fn a_ratio_of_two_aggregates_is_taken_apart() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT SUM(x) / COUNT(x) AS r FROM t")
        else {
            panic!("a ratio of aggregates should now be answered");
        };
        assert_eq!(how, vec![Some(AggMerge::Sum), Some(AggMerge::Sum)]);
        let node_sql = post.node_sql.as_deref().unwrap();
        assert!(node_sql.contains("SUM(x)") && node_sql.contains("COUNT(x)"));
    }

    /// Two columns that each contain `SUM(x)` must not share one helper.
    #[test]
    fn two_computed_columns_do_not_collide() {
        let SqlMerge::GroupedAggregate { post, .. } =
            sql_merge_strategy("SELECT SUM(x) * 2 AS a, SUM(x) * 3 AS b FROM t")
        else {
            panic!("expected a computed plan");
        };
        let finish = post.finish_sql.as_deref().unwrap();
        assert!(finish.contains("__chronik_inner_0_0"));
        assert!(finish.contains("__chronik_inner_1_0"));
    }

    /// The finishing step is run by the query engine over the merged rows, so
    /// the arithmetic is the engine's. Exercised end to end on real values.
    #[tokio::test]
    async fn the_coordinator_finishes_the_query_with_the_engine() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT ROUND(AVG(x), 2) AS a FROM t")
        else {
            panic!("expected a computed plan");
        };

        let d = &post.derived[0];
        let partial = |sum: i64, count: i64| SqlResponse {
            columns: vec![d.sum.clone(), d.count.clone()],
            rows: vec![HashMap::from([
                (d.sum.clone(), serde_json::Value::from(sum)),
                (d.count.clone(), serde_json::Value::from(count)),
            ])],
            row_count: 1,
            execution_time_ms: 0,
            truncated: false,
        };

        // 10 over 3 and 90 over 7: the true average is 10, and the mean of the
        // per-node averages would have been about 8.1.
        let merged =
            merge_grouped_aggregate(partial(10, 3), vec![partial(90, 7)], &how, &post, 100);
        let finished = run_finish_sql(merged, post.finish_sql.as_deref().unwrap())
            .await
            .expect("the coordinator should finish the query");

        assert_eq!(finished.columns, vec!["a".to_string()]);
        assert_eq!(finished.rows[0]["a"], serde_json::Value::from(10.0));
    }

    /// A group key rides through the finishing step untouched, and the
    /// expression is applied per group.
    #[tokio::test]
    async fn a_computed_column_is_finished_per_group() {
        let SqlMerge::GroupedAggregate { how, post } =
            sql_merge_strategy("SELECT k, SUM(x) * 2 AS doubled FROM t GROUP BY k")
        else {
            panic!("expected a computed plan");
        };

        let helper = "__chronik_inner_1_0";
        let group = |k: i64, sum: i64| {
            HashMap::from([
                ("k".to_string(), serde_json::Value::from(k)),
                (helper.to_string(), serde_json::Value::from(sum)),
            ])
        };
        let as_resp = |rows: Vec<HashMap<String, serde_json::Value>>| SqlResponse {
            columns: vec!["k".to_string(), helper.to_string()],
            rows,
            row_count: 0,
            execution_time_ms: 0,
            truncated: false,
        };

        let merged = merge_grouped_aggregate(
            as_resp(vec![group(1, 5)]),
            vec![as_resp(vec![group(1, 3), group(2, 4)])],
            &how,
            &post,
            100,
        );
        let finished = run_finish_sql(merged, post.finish_sql.as_deref().unwrap())
            .await
            .expect("finishes");

        let mut got: Vec<(i64, i64)> = finished
            .rows
            .iter()
            .map(|r| (r["k"].as_i64().unwrap(), r["doubled"].as_i64().unwrap()))
            .collect();
        got.sort();
        assert_eq!(got, vec![(1, 16), (2, 8)], "k=1 is 5+3 doubled");
    }

    /// The merged rows carry no schema of their own, so the staging has to take
    /// the type from the values — and keep integers integral.
    #[test]
    fn staged_columns_take_their_type_from_the_values() {
        use chronik_columnar::datafusion::arrow::datatypes::DataType;
        let j = serde_json::Value::from;

        assert_eq!(column_kind(&[&j(1), &j(2)]).0, DataType::Int64);
        let f = serde_json::Value::from(2.5_f64);
        assert_eq!(column_kind(&[&j(1), &f]).0, DataType::Float64);
        assert_eq!(
            column_kind(&[&serde_json::Value::Bool(true)]).0,
            DataType::Boolean
        );
        assert_eq!(
            column_kind(&[&serde_json::Value::String("a".into())]).0,
            DataType::Utf8
        );
        // A column that mixes shapes falls back to text rather than guessing.
        assert_eq!(
            column_kind(&[&j(1), &serde_json::Value::String("a".into())]).0,
            DataType::Utf8
        );
        // Nulls alone are still a column, and every case is nullable-aware.
        assert!(column_kind(&[&serde_json::Value::Null, &j(1)]).1);
    }

    /// An AVG or a variance sitting *beside* a computed column still has to be
    /// taken apart.
    ///
    /// It was not: the computed planner passed anything that was not a plain
    /// mergeable aggregate straight through to the nodes, so `AVG(x)` went out
    /// as `AVG(x)` and each node's own average came back as a group key of its
    /// own — one row per node, quietly, which is the exact failure this whole
    /// issue is about. Found by reviewing the code rather than by a test, so
    /// here is the test.
    #[test]
    fn an_aggregate_beside_a_computed_column_is_still_taken_apart() {
        for sql in [
            "SELECT AVG(x) AS a, ROUND(AVG(y), 2) AS b FROM t",
            "SELECT stddev(x) AS s, SUM(y) * 2 AS d FROM t",
            "SELECT k, AVG(x) AS a, SUM(y) * 2 AS d FROM t GROUP BY k",
        ] {
            let SqlMerge::GroupedAggregate { how, post } = sql_merge_strategy(sql) else {
                panic!("{sql} should be answered");
            };
            let node_sql = post.node_sql.as_deref().expect("a rewritten query");
            for forbidden in ["AVG(", "STDDEV(", "stddev("] {
                assert!(
                    !node_sql.contains(forbidden),
                    "{forbidden} must not reach the nodes in {sql}: {node_sql}"
                );
            }
            // Every aggregate column combines; only a group key may be a key.
            let keys = how.iter().filter(|h| h.is_none()).count();
            assert_eq!(
                keys,
                usize::from(sql.contains("GROUP BY")),
                "no per-node aggregate may be treated as a group key: {how:?}"
            );
        }
    }

    /// A quantile and a computed column need the nodes to return different
    /// shapes — a frequency table and bare aggregates — so asking for both in
    /// one query is refused rather than answered from the wrong one.
    #[test]
    fn a_quantile_beside_a_computed_column_is_refused() {
        assert!(
            refusal("SELECT median(x) AS m, SUM(y) * 2 AS d FROM t").contains("separately"),
            "the two shapes cannot be asked for at once"
        );
        assert!(
            refusal("SELECT COUNT(DISTINCT x) AS c, SUM(y) * 2 AS d FROM t").contains("separately")
        );
    }

    /// A scalar aggregate can carry a HAVING — `SELECT COUNT(*) FROM t HAVING
    /// COUNT(*) > 5` is a row or no row — and it belongs to the combined total.
    ///
    /// The plan carried it and nothing applied it: the handler's scalar branch
    /// divided the averages and stopped, so the threshold was silently dropped
    /// and a count of 3 came back from a query that asked for counts above 5.
    /// Found by reading the branches, not by a test.
    #[test]
    fn a_having_on_a_scalar_aggregate_is_not_dropped() {
        let SqlMerge::ScalarAggregate { how, post } =
            sql_merge_strategy("SELECT COUNT(*) AS c FROM t HAVING COUNT(*) > 5")
        else {
            panic!("a scalar aggregate with HAVING should be answered");
        };
        assert_eq!(how, vec![AggMerge::Sum]);
        let predicate = post.having.as_ref().expect("the threshold is in the plan");

        // Three nodes totalling 3 must not survive `> 5`; totalling 9 must.
        let short = HashMap::from([("c".to_string(), serde_json::Value::from(3))]);
        let over = HashMap::from([("c".to_string(), serde_json::Value::from(9))]);
        assert!(!predicate.admits(&short));
        assert!(predicate.admits(&over));

        // And applying the plan has to actually remove the row.
        let mut rows = vec![short];
        apply_finish(&mut rows, &post);
        assert!(
            rows.is_empty(),
            "the threshold has to be applied, not carried"
        );
    }
}

/// A coordinator that applies the rest of a query by name must be asking the
/// nodes for those names.
///
/// The planner's name for an unaliased column is the parser's rendering of the
/// expression — `COUNT(*)`, `UPPER(k)`. The engine names the same columns
/// `count(*)` and `upper(t.k)`. Every name-based step was therefore looking up
/// a column that was not there, and each failed quietly in its own way: the
/// sort did nothing and `LIMIT` returned arbitrary rows as the top N, `HAVING`
/// admitted nothing and returned empty, and a quantile's groups collapsed into
/// one. The tests here hold the two sides together.
#[cfg(test)]
mod planner_name_tests {
    use super::*;

    fn row(pairs: &[(&str, i64)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::from(*v)))
            .collect()
    }

    fn plan_of(sql: &str) -> MergePlan {
        match sql_merge_strategy(sql) {
            SqlMerge::Concatenate(post)
            | SqlMerge::GroupedAggregate { post, .. }
            | SqlMerge::ScalarAggregate { post, .. } => post,
            SqlMerge::Unsupported(why) => panic!("expected a plan for {sql}, refused: {why}"),
        }
    }

    /// Every column name this predicate looks up.
    fn predicate_columns(predicate: &Predicate, into: &mut Vec<String>) {
        match predicate {
            Predicate::And(a, b) | Predicate::Or(a, b) => {
                predicate_columns(a, into);
                predicate_columns(b, into);
            }
            Predicate::Not(inner) => predicate_columns(inner, into),
            Predicate::Compare { column, .. } => into.push(column.clone()),
        }
    }

    /// The output names a query is *guaranteed* to come back with: explicit
    /// aliases, and bare column references, which the engine names as written.
    /// An unaliased expression is not one of them — that is the whole bug.
    fn guaranteed_names(sql: &str) -> Option<Vec<String>> {
        use chronik_columnar::datafusion::sql::parser::{DFParser, Statement};
        use chronik_columnar::datafusion::sql::sqlparser::ast::{
            Expr as SqlExpr, SelectItem, SetExpr, Statement as AstStatement,
        };

        let statements = DFParser::parse_sql(sql).expect("the planner's own SQL must parse");
        let Some(Statement::Statement(ast)) = statements.front() else {
            panic!("not a statement: {sql}");
        };
        let AstStatement::Query(query) = ast.as_ref() else {
            panic!("not a query: {sql}");
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Some(Vec::new());
        };

        let mut names = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::ExprWithAlias { alias, .. } => names.push(alias.value.clone()),
                SelectItem::UnnamedExpr(SqlExpr::Identifier(id)) => names.push(id.value.clone()),
                // `SELECT *` returns columns this cannot enumerate; the engine
                // validates the name instead, because the ORDER BY is left in.
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => return None,
                SelectItem::UnnamedExpr(_) => {}
            }
        }
        Some(names)
    }

    /// The invariant, over every shape the planner can produce. This is the
    /// test that catches the whole family at once: all three bugs were one
    /// root cause, and each was found only by reading a different branch.
    #[test]
    fn every_name_the_coordinator_uses_is_one_the_nodes_are_asked_for() {
        for sql in [
            "SELECT k, COUNT(*) FROM t GROUP BY k ORDER BY COUNT(*) DESC LIMIT 2",
            "SELECT k, COUNT(*) FROM t GROUP BY k ORDER BY 2 DESC LIMIT 2",
            "SELECT k, COUNT(*) FROM t GROUP BY k HAVING COUNT(*) > 1",
            "SELECT k, COUNT(*) FROM t GROUP BY k HAVING COUNT(*) > 1 ORDER BY COUNT(*)",
            "SELECT k, AVG(v) FROM t GROUP BY k ORDER BY AVG(v) DESC",
            "SELECT UPPER(k), SUM(v) FROM t GROUP BY UPPER(k) ORDER BY UPPER(k)",
            "SELECT t.k, COUNT(*) FROM t GROUP BY t.k ORDER BY COUNT(*) DESC",
            "SELECT COUNT(*) FROM t HAVING COUNT(*) > 5",
            "SELECT AVG(v) FROM t",
            "SELECT k, median(v) AS m FROM t GROUP BY k ORDER BY m DESC LIMIT 2",
            "SELECT UPPER(k), median(v) AS m FROM t GROUP BY UPPER(k)",
            "SELECT UPPER(k), COUNT(DISTINCT v) AS d FROM t GROUP BY UPPER(k)",
            "SELECT k, ROUND(AVG(v), 2) FROM t GROUP BY k ORDER BY 2 DESC",
            "SELECT UPPER(k), ROUND(AVG(v), 2) AS b FROM t GROUP BY UPPER(k)",
            "SELECT k, SUM(v) * 2 FROM t GROUP BY k ORDER BY 2 DESC",
            "SELECT k, COUNT(*) AS c FROM t GROUP BY k ORDER BY c DESC LIMIT 2",
            "SELECT _offset, _value FROM t ORDER BY _offset DESC LIMIT 5",
            // No aggregate at all, and still a name the engine renders its own
            // way: `lower(t.k)` against the planner's `LOWER(k)`.
            "SELECT LOWER(k) FROM t ORDER BY LOWER(k) LIMIT 3",
            "SELECT k, COUNT(*), ROUND(AVG(v), 2) AS b FROM t GROUP BY k ORDER BY b DESC",
        ] {
            let plan = plan_of(sql);
            // With no rewrite the caller's SQL went out as it stood, and the
            // planner then used no names of its own.
            let node_sql = plan.node_sql.clone().unwrap_or_else(|| sql.to_string());
            let Some(guaranteed) = guaranteed_names(&node_sql) else {
                continue; // a wildcard; the engine checks the name
            };

            let mut wanted: Vec<String> = plan.order_by.iter().map(|k| k.column.clone()).collect();
            if let Some(predicate) = plan.having.as_ref() {
                predicate_columns(predicate, &mut wanted);
            }
            if let Some(spec) = plan.quantile.as_ref() {
                wanted.extend(spec.keys.iter().cloned());
            }
            if let Some(spec) = plan.distinct_count.as_ref() {
                wanted.extend(spec.keys.iter().cloned());
            }
            // A derived column is created by the coordinator after the merge,
            // so it is a name the plan supplies rather than one it looks up.
            let created: Vec<String> = plan.derived.iter().map(|d| d.output.clone()).collect();
            let quantile_output = plan.quantile.as_ref().map(|s| s.output.clone());
            let distinct_output = plan.distinct_count.as_ref().map(|s| s.output.clone());

            for name in wanted {
                let known = guaranteed.contains(&name)
                    || created.contains(&name)
                    || quantile_output.as_deref() == Some(name.as_str())
                    || distinct_output.as_deref() == Some(name.as_str())
                    // The computed path aliases its own output in finish_sql.
                    || plan.finish_sql.as_deref().is_some_and(|s| {
                        s.contains(&format!("AS \"{}\"", name.replace('"', "\"\"")))
                    });
                assert!(
                    known,
                    "{sql}\n  the coordinator looks up {name:?}, which the nodes are never \
                     asked for.\n  node SQL: {node_sql}\n  guaranteed: {guaranteed:?}"
                );
            }
        }
    }

    /// `ORDER BY COUNT(*) DESC LIMIT 10` — the canonical top-N. The nodes are
    /// asked for the name the sort will use, so the sort happens; before this,
    /// the comparison found null against null on every row, left the order
    /// untouched, and `LIMIT` returned ten arbitrary groups as the top ten.
    #[test]
    fn top_n_by_an_unaliased_count_is_actually_sorted() {
        let plan = plan_of("SELECT k, COUNT(*) FROM t GROUP BY k ORDER BY COUNT(*) DESC LIMIT 2");
        assert_eq!(plan.order_by.len(), 1);
        assert_eq!(plan.order_by[0].column, "COUNT(*)");
        assert!(plan.order_by[0].descending);
        assert_eq!(plan.limit, Some(2));
        let node_sql = plan.node_sql.as_deref().expect("the name has to be asked for");
        assert!(
            node_sql.contains(r#"COUNT(*) AS "COUNT(*)""#),
            "the nodes must be asked for the name the sort uses: {node_sql}"
        );
        assert!(
            !node_sql.contains("LIMIT"),
            "the limit belongs to the merged groups: {node_sql}"
        );

        // And the sort does order the rows it is given.
        let mut rows = vec![
            row(&[("COUNT(*)", 3)]),
            row(&[("COUNT(*)", 9)]),
            row(&[("COUNT(*)", 5)]),
        ];
        apply_finish(&mut rows, &plan);
        let counts: Vec<i64> = rows
            .iter()
            .map(|r| r["COUNT(*)"].as_i64().unwrap())
            .collect();
        assert_eq!(counts, vec![9, 5], "descending, then cut to the limit");
    }

    /// `HAVING COUNT(*) > 1` on an unaliased count returned *no rows at all*: a
    /// column that is absent fails the comparison, so every group was dropped.
    #[test]
    fn having_on_an_unaliased_count_keeps_the_groups_that_clear_it() {
        let plan = plan_of("SELECT k, COUNT(*) FROM t GROUP BY k HAVING COUNT(*) > 1");
        let node_sql = plan.node_sql.as_deref().expect("the name has to be asked for");
        assert!(
            node_sql.contains(r#"COUNT(*) AS "COUNT(*)""#),
            "the nodes must be asked for the name HAVING filters on: {node_sql}"
        );
        assert!(
            !node_sql.contains("HAVING"),
            "the threshold belongs to the combined groups: {node_sql}"
        );

        let mut rows = vec![row(&[("COUNT(*)", 1)]), row(&[("COUNT(*)", 4)])];
        apply_finish(&mut rows, &plan);
        assert_eq!(rows.len(), 1, "the group with 4 rows clears the threshold");
        assert_eq!(rows[0]["COUNT(*)"], serde_json::Value::from(4));
    }

    /// An expression group key is looked up per row to decide which group a
    /// quantile's counts belong to. Named `UPPER(k)` by the planner and
    /// `upper(t.k)` by the engine, every row hashed to the same identity and
    /// the groups collapsed into one.
    #[test]
    fn an_expression_group_key_is_named_so_a_quantiles_groups_survive() {
        let plan = plan_of("SELECT UPPER(k), median(v) AS m FROM t GROUP BY UPPER(k)");
        let spec = plan.quantile.as_ref().expect("a quantile plan");
        assert_eq!(spec.keys, vec!["UPPER(k)".to_string()]);
        let node_sql = plan.node_sql.as_deref().expect("a reshaped query");
        assert!(
            node_sql.contains(r#"UPPER(k) AS "UPPER(k)""#),
            "the nodes must be asked for the key the groups are gathered by: {node_sql}"
        );
    }

    /// `SELECT * FROM t ORDER BY ts LIMIT 10` is the commonest query there is
    /// against a stream, and it was refused: a wildcard's columns are not named
    /// by the SQL, so the sort column could not be proved to be in the
    /// response. It is — the nodes keep their own ORDER BY, so the engine
    /// rejects a name that is not a column instead of the coordinator sorting
    /// by one that is absent.
    #[test]
    fn a_wildcard_can_be_ordered_by_a_column_it_returns() {
        let plan = plan_of("SELECT * FROM t WHERE v > 1 ORDER BY v DESC LIMIT 5 OFFSET 2");
        assert_eq!(plan.order_by.len(), 1);
        assert_eq!(plan.order_by[0].column, "v");
        assert!(plan.order_by[0].descending);
        assert_eq!(plan.limit, Some(5));
        assert_eq!(plan.offset, Some(2));

        let node_sql = plan.node_sql.as_deref().expect("limit and offset are stripped");
        assert!(
            node_sql.contains("ORDER BY"),
            "the nodes keep the sort so the engine validates the column: {node_sql}"
        );
        assert!(
            !node_sql.contains("LIMIT") && !node_sql.contains("OFFSET"),
            "the cut belongs to the merged rows: {node_sql}"
        );
    }

    /// An ORDER BY a wildcard cannot supply is still refused: the response
    /// carries `k`, not `LOWER(k)`, so there is nothing to sort by.
    #[test]
    fn a_wildcard_does_not_excuse_an_unprojected_expression() {
        assert!(matches!(
            sql_merge_strategy("SELECT * FROM t ORDER BY LOWER(k) LIMIT 2"),
            SqlMerge::Unsupported(_)
        ));
    }

    /// A quantile is answered as a count per value and a distinct count as the
    /// values themselves. Asked together, the distinct count matched neither
    /// guard and was treated as a group key — which put `COUNT(DISTINCT v)`
    /// into the nodes' GROUP BY, so the engine rejected the rewrite with a
    /// message about SQL the caller never wrote.
    #[test]
    fn a_quantile_and_a_distinct_count_are_refused_together() {
        let SqlMerge::Unsupported(why) = sql_merge_strategy(
            "SELECT k, median(v) AS m, COUNT(DISTINCT v) AS d FROM t GROUP BY k",
        ) else {
            panic!("a quantile beside a distinct count has no single node shape");
        };
        assert!(why.contains("separately"), "{why}");
    }

    /// With the name now asked for, a bare `AVG` no longer has to be aliased by
    /// the caller to be answered at all.
    #[test]
    fn a_bare_average_is_answered_rather_than_refused() {
        let plan = plan_of("SELECT AVG(v) FROM t");
        assert_eq!(plan.derived.len(), 1);
        assert_eq!(plan.derived[0].output, "AVG(v)");
    }

    /// The names only change where they were wrong. A query that needs no name
    /// of its own is still sent exactly as the caller wrote it, so its columns
    /// come back named as the engine names them — the same on one node as on
    /// three.
    #[test]
    fn a_query_needing_no_name_is_sent_unchanged() {
        for sql in [
            "SELECT k, COUNT(*) FROM t GROUP BY k",
            "SELECT COUNT(*) FROM t",
            "SELECT k, SUM(v) FROM t GROUP BY k",
            "SELECT _offset, _value FROM t",
        ] {
            assert!(
                plan_of(sql).node_sql.is_none(),
                "{sql} must reach the nodes unchanged, keeping the engine's column names"
            );
        }
    }
}

/// The coordinator's own finishing query, run for real.
///
/// The names the planner resolves are now asked of the nodes, which means the
/// finishing step has to be able to *reference* a column called `UPPER(k)` or
/// `COUNT(*)`. That is what [`quote_ident`] is for, and nothing proved it
/// worked end to end: the computed path builds its SQL as text at plan time,
/// so a name the engine cannot parse fails only when a query runs.
#[cfg(test)]
mod finish_sql_tests {
    use super::*;

    #[tokio::test]
    async fn a_finishing_query_can_name_an_unaliased_group_key() {
        let SqlMerge::GroupedAggregate { post, .. } = sql_merge_strategy(
            "SELECT UPPER(k), ROUND(AVG(v), 2) AS b FROM t GROUP BY UPPER(k)",
        ) else {
            panic!("a wrapped aggregate is answered by finishing on the coordinator");
        };
        let finish_sql = post.finish_sql.as_deref().expect("a finishing step");
        let derived = post.derived.first().expect("the inner AVG is carried as totals");

        // What the nodes return, merged: the group key under the name the
        // planner asked for, plus the totals the average is rebuilt from.
        let mut merged = SqlResponse {
            columns: vec![
                "UPPER(k)".to_string(),
                derived.sum.clone(),
                derived.count.clone(),
            ],
            rows: vec![HashMap::from([
                ("UPPER(k)".to_string(), serde_json::json!("A")),
                (derived.sum.clone(), serde_json::json!(10)),
                (derived.count.clone(), serde_json::json!(4)),
            ])],
            row_count: 1,
            execution_time_ms: 1,
            truncated: false,
        };
        finish_derived(&mut merged, &post.derived);

        let finished = run_finish_sql(merged, finish_sql)
            .await
            .expect("the finishing query must be valid SQL over the merged rows");

        assert_eq!(finished.row_count, 1);
        let row = &finished.rows[0];
        assert_eq!(
            row.get("UPPER(k)"),
            Some(&serde_json::json!("A")),
            "the group key keeps the name the nodes were asked for: {:?}",
            finished.columns
        );
        assert_eq!(
            row.get("b").and_then(|v| v.as_f64()),
            Some(2.5),
            "10 / 4, rounded: {:?}",
            finished.columns
        );
    }

    /// The same for a name that is only a name because it is quoted.
    #[tokio::test]
    async fn a_finishing_query_can_name_a_count_star_column() {
        let sql = "SELECT COUNT(*), ROUND(AVG(v), 2) AS b FROM t";
        let SqlMerge::GroupedAggregate { how, post } = sql_merge_strategy(sql) else {
            panic!("a wrapped aggregate is answered by finishing on the coordinator");
        };
        // The count must be summed, not treated as a key: keyed, each node's
        // own count would become a group of its own and the query would return
        // one row per node.
        assert_eq!(
            how,
            vec![Some(AggMerge::Sum), Some(AggMerge::Sum), Some(AggMerge::Sum)],
            "count, then the average's sum and count"
        );
        let finish_sql = post.finish_sql.as_deref().expect("a finishing step");
        let derived = post.derived.first().expect("the inner AVG is carried as totals");

        let mut merged = SqlResponse {
            columns: vec![
                "COUNT(*)".to_string(),
                derived.sum.clone(),
                derived.count.clone(),
            ],
            rows: vec![HashMap::from([
                ("COUNT(*)".to_string(), serde_json::json!(7)),
                (derived.sum.clone(), serde_json::json!(9)),
                (derived.count.clone(), serde_json::json!(2)),
            ])],
            row_count: 1,
            execution_time_ms: 1,
            truncated: false,
        };
        finish_derived(&mut merged, &post.derived);

        let finished = run_finish_sql(merged, finish_sql)
            .await
            .expect("the finishing query must be valid SQL over the merged rows");
        let row = &finished.rows[0];
        assert_eq!(row.get("COUNT(*)"), Some(&serde_json::json!(7)));
        assert_eq!(row.get("b").and_then(|v| v.as_f64()), Some(4.5));
    }
}
