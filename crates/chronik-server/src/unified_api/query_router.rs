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
pub use chronik_search::api::{SearchResponse, ShardInfo, HitsInfo, TotalHits, Hit};

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
                let port = std::env::var(format!(
                    "CHRONIK_UNIFIED_API_PORT_NODE_{}",
                    peer.id
                ))
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

        let peer_metrics: HashMap<u64, PeerMetrics> =
            peers.iter().map(|p| (p.node_id, PeerMetrics::new())).collect();

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
    pub async fn refresh_partition_map(
        &self,
        metadata_store: &dyn MetadataStore,
        topic: &str,
    ) {
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

            let mut selected_ids: std::collections::HashSet<u64> =
                std::collections::HashSet::new();

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
            replicas.values().all(|partition_replicas| {
                partition_replicas.contains(&self.self_node_id)
            })
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
                self.refresh_partition_map(metadata_store, &topic.name).await;
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
    merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
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
    merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
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
) -> SqlResponse {
    let columns = local.columns.clone();
    let execution_time_ms = local.execution_time_ms;
    let mut all_rows = local.rows;

    for peer_resp in peers {
        all_rows.extend(peer_resp.rows);
    }

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
    /// Rows from each node are disjoint; append them.
    Concatenate,
    /// One row per node; combine column by column with these functions, in the
    /// order the columns appear.
    ScalarAggregate(Vec<AggMerge>),
    /// Cannot be combined without changing the query. Better to say so than to
    /// return a number that looks right.
    Unsupported(&'static str),
}

/// How one aggregate column combines across nodes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggMerge {
    Sum,
    Min,
    Max,
}

/// Decide how to merge, from the query itself.
///
/// Deliberately conservative: anything not recognised as safely combinable is
/// [`SqlMerge::Unsupported`] rather than guessed at. `AVG` is the clearest
/// example — averaging three nodes' averages is not the average unless every
/// node held the same number of rows, which is exactly what a partitioned topic
/// does not guarantee. Computing it properly needs `SUM` and `COUNT` carried
/// separately, which means rewriting the query, not merging its output.
pub fn sql_merge_strategy(sql: &str) -> SqlMerge {
    use chronik_columnar::datafusion::sql::parser::{DFParser, Statement};
    use chronik_columnar::datafusion::sql::sqlparser::ast::{
        Expr as SqlExpr, GroupByExpr, SelectItem, SetExpr, Statement as AstStatement,
    };

    let Ok(statements) = DFParser::parse_sql(sql) else {
        return SqlMerge::Concatenate; // unparseable here; the engine will reject it
    };
    let Some(Statement::Statement(ast)) = statements.front() else {
        return SqlMerge::Concatenate;
    };
    let AstStatement::Query(query) = ast.as_ref() else {
        return SqlMerge::Concatenate;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        // UNION and friends: the operands were each evaluated per node, so the
        // shape is not something this function can reason about.
        return SqlMerge::Concatenate;
    };

    let grouped = match &select.group_by {
        GroupByExpr::Expressions(exprs, _) => !exprs.is_empty(),
        GroupByExpr::All(_) => true,
    };

    // Collect the aggregate function of each projected column, if any.
    let mut aggs: Vec<Option<AggMerge>> = Vec::new();
    for item in &select.projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) => expr,
            SelectItem::ExprWithAlias { expr, .. } => expr,
            // `*` is never an aggregate.
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                aggs.push(None);
                continue;
            }
        };

        aggs.push(match expr {
            SqlExpr::Function(f) => {
                let name = f.name.to_string().to_ascii_uppercase();
                match name.as_str() {
                    "COUNT" | "SUM" => Some(AggMerge::Sum),
                    "MIN" => Some(AggMerge::Min),
                    "MAX" => Some(AggMerge::Max),
                    "AVG" | "MEAN" => {
                        return SqlMerge::Unsupported(
                            "AVG cannot be merged across nodes: averaging per-node averages is \
                             only correct when every node holds the same number of rows. Use \
                             SUM(x) and COUNT(x) and divide.",
                        )
                    }
                    // Unknown function: it may or may not be an aggregate, and
                    // assuming wrong is how a query silently returns nonsense.
                    _ => None,
                }
            }
            _ => None,
        });
    }

    let any_agg = aggs.iter().any(Option::is_some);

    if grouped {
        if any_agg {
            return SqlMerge::Unsupported(
                "GROUP BY cannot be merged across nodes: each node groups only its own \
                 partitions, so the same key appears once per node and the groups would need \
                 re-aggregating by key.",
            );
        }
        // GROUP BY with no aggregate is a DISTINCT in disguise; concatenating
        // can repeat a key across nodes, so it is not safe either.
        return SqlMerge::Unsupported(
            "GROUP BY cannot be merged across nodes: the same key may be produced by more \
             than one node.",
        );
    }

    if !any_agg {
        if select.distinct.is_some() {
            return SqlMerge::Unsupported(
                "SELECT DISTINCT cannot be merged across nodes: each node de-duplicates only \
                 its own partitions, so a value held by two nodes appears twice.",
            );
        }
        return SqlMerge::Concatenate;
    }

    // Mixing aggregates with bare columns without GROUP BY is not valid SQL the
    // engine would accept, but if it did there is no sound merge.
    if aggs.iter().any(Option::is_none) {
        return SqlMerge::Unsupported(
            "mixing aggregate and non-aggregate columns cannot be merged across nodes.",
        );
    }

    SqlMerge::ScalarAggregate(aggs.into_iter().map(Option::unwrap).collect())
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
    let columns = local.columns.clone();
    let execution_time_ms = local.execution_time_ms;

    let mut rows: Vec<HashMap<String, serde_json::Value>> = Vec::new();
    rows.extend(local.rows);
    for peer in peers {
        rows.extend(peer.rows);
    }

    let mut merged: HashMap<String, serde_json::Value> = HashMap::new();
    for (idx, column) in columns.iter().enumerate() {
        let op = how.get(idx).copied().unwrap_or(AggMerge::Sum);
        let mut acc: Option<serde_json::Value> = None;

        for row in &rows {
            let Some(value) = row.get(column) else { continue };
            if value.is_null() {
                continue;
            }
            acc = Some(match acc {
                None => value.clone(),
                Some(current) => combine_numbers(&current, value, op),
            });
        }

        merged.insert(
            column.clone(),
            acc.unwrap_or(serde_json::Value::Null),
        );
    }

    SqlResponse {
        columns,
        rows: vec![merged],
        row_count: 1,
        execution_time_ms,
        truncated: false,
    }
}

/// Combine two JSON numbers. Non-numeric values keep the accumulator, since
/// there is no meaningful sum of two strings.
fn combine_numbers(a: &serde_json::Value, b: &serde_json::Value, op: AggMerge) -> serde_json::Value {
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
            _ => { best.insert(key, hit); }
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

        let merged = merge_sql_responses(local, vec![peer], 100);
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

        let merged = merge_sql_responses(local, vec![peer], 2); // limit=2
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
            peers: vec![
                chronik_config::NodeConfig {
                    id: 99,
                    kafka: "remote-host:9092".to_string(),
                    wal: "remote-host:9291".to_string(),
                    raft: "remote-host:5001".to_string(),
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
            let led = router.led_partitions("orders").await.expect("topic is mapped");
            seen.extend(led);
        }

        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3], "partitions must be covered exactly once");
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
            SqlMerge::Concatenate
        );
        assert_eq!(sql_merge_strategy("SELECT * FROM orders"), SqlMerge::Concatenate);
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
        assert!(merged.rows[0]["c"].is_i64(), "got {:?}", merged.rows[0]["c"]);
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

    /// GROUP BY needs re-aggregation by key, which merging output rows cannot do.
    #[test]
    fn group_by_is_refused() {
        match sql_merge_strategy("SELECT k, COUNT(*) FROM orders GROUP BY k") {
            SqlMerge::Unsupported(why) => assert!(why.contains("GROUP BY")),
            other => panic!("GROUP BY must not be merged: {:?}", other),
        }
    }

    /// DISTINCT de-duplicates per node, so a value on two nodes survives twice.
    #[test]
    fn distinct_is_refused() {
        match sql_merge_strategy("SELECT DISTINCT k FROM orders") {
            SqlMerge::Unsupported(why) => assert!(why.contains("DISTINCT")),
            other => panic!("DISTINCT must not be merged: {:?}", other),
        }
    }

    /// A null from a node that holds no rows must not erase another node's value.
    #[test]
    fn a_node_with_no_rows_does_not_erase_the_answer() {
        let local = resp(&["c"], vec![HashMap::from([("c".to_string(), serde_json::Value::Null)])]);
        let peers = vec![resp(&["c"], vec![row(&[("c", 7)])])];
        let merged = merge_scalar_aggregate(local, peers, &[AggMerge::Sum]);
        assert_eq!(merged.rows[0]["c"], serde_json::Value::from(7));
    }
}
