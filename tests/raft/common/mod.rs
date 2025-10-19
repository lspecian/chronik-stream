/// Common utilities for Raft integration tests
///
/// Provides test cluster management, fault injection, and assertion helpers.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use testcontainers::{clients::Cli, Container, Image};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::info;

/// Represents a single Chronik node in a test cluster
#[derive(Debug)]
pub struct TestNode {
    /// Raft node ID
    pub node_id: u64,
    /// Raft bind address
    pub raft_addr: SocketAddr,
    /// Kafka API address
    pub kafka_addr: SocketAddr,
    /// HTTP API address
    pub http_addr: SocketAddr,
    /// Process handle
    pub process: Child,
    /// Data directory
    pub data_dir: TempDir,
    /// Toxiproxy proxy name (if enabled)
    pub proxy_name: Option<String>,
}

impl TestNode {
    /// Kill the node process
    pub async fn kill(&mut self) -> Result<()> {
        info!(node_id = self.node_id, "Killing node");
        self.process.kill().await.context("Failed to kill process")?;
        Ok(())
    }

    /// Restart the node with the same configuration
    pub async fn restart(&mut self) -> Result<()> {
        info!(node_id = self.node_id, "Restarting node");

        // Kill first
        let _ = self.process.kill().await;

        // Wait a bit for cleanup
        sleep(Duration::from_millis(500)).await;

        // Restart with same data dir
        let mut cmd = Command::new(get_chronik_binary()?);
        cmd.arg("raft-cluster")
            .arg("--node-id").arg(self.node_id.to_string())
            .arg("--raft-addr").arg(self.raft_addr.to_string())
            .arg("--kafka-addr").arg(self.kafka_addr.to_string())
            .arg("--http-addr").arg(self.http_addr.to_string())
            .arg("--data-dir").arg(self.data_dir.path())
            .env("RUST_LOG", "debug");

        self.process = cmd.spawn().context("Failed to spawn process")?;

        // Wait for node to be ready
        wait_for_node_ready(self.http_addr, Duration::from_secs(30)).await?;

        Ok(())
    }

    /// Check if node is alive
    pub async fn is_alive(&self) -> bool {
        check_node_health(self.http_addr).await
    }
}

/// Test cluster configuration
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// Number of nodes in cluster
    pub node_count: usize,
    /// Enable Toxiproxy for fault injection
    pub use_toxiproxy: bool,
    /// Base port for Raft (increments for each node)
    pub base_raft_port: u16,
    /// Base port for Kafka (increments for each node)
    pub base_kafka_port: u16,
    /// Base port for HTTP (increments for each node)
    pub base_http_port: u16,
    /// Election timeout (ms)
    pub election_timeout_ms: u64,
    /// Heartbeat interval (ms)
    pub heartbeat_interval_ms: u64,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            node_count: 3,
            use_toxiproxy: false,
            base_raft_port: 7000,
            base_kafka_port: 9000,
            base_http_port: 8000,
            election_timeout_ms: 1000,
            heartbeat_interval_ms: 100,
        }
    }
}

/// Test cluster with multiple Chronik nodes
pub struct TestCluster {
    /// Cluster nodes
    pub nodes: Vec<TestNode>,
    /// Toxiproxy container (if enabled)
    pub toxiproxy: Option<ToxiproxyContainer>,
    /// Cluster configuration
    pub config: ClusterConfig,
}

impl TestCluster {
    /// Get node by ID
    pub fn get_node(&self, node_id: u64) -> Option<&TestNode> {
        self.nodes.iter().find(|n| n.node_id == node_id)
    }

    /// Get mutable node by ID
    pub fn get_node_mut(&mut self, node_id: u64) -> Option<&mut TestNode> {
        self.nodes.iter_mut().find(|n| n.node_id == node_id)
    }

    /// Get current leader (if any)
    pub async fn get_leader(&self) -> Option<u64> {
        for node in &self.nodes {
            if let Ok(info) = get_node_info(node.http_addr).await {
                if info.is_leader {
                    return Some(node.node_id);
                }
            }
        }
        None
    }

    /// Wait for cluster to have a leader
    pub async fn wait_for_leader(&self, timeout_duration: Duration) -> Result<u64> {
        let start = Instant::now();
        loop {
            if let Some(leader_id) = self.get_leader().await {
                info!(leader_id, "Leader elected");
                return Ok(leader_id);
            }

            if start.elapsed() > timeout_duration {
                anyhow::bail!("Timeout waiting for leader election");
            }

            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Wait for all nodes to agree on leader
    pub async fn wait_for_consensus(&self, timeout_duration: Duration) -> Result<u64> {
        let start = Instant::now();
        loop {
            let mut leader_ids = Vec::new();
            for node in &self.nodes {
                if let Ok(info) = get_node_info(node.http_addr).await {
                    leader_ids.push(info.current_leader);
                }
            }

            // Check if all nodes agree
            if !leader_ids.is_empty() && leader_ids.iter().all(|&id| id == leader_ids[0]) {
                let leader_id = leader_ids[0];
                info!(leader_id, "Consensus reached");
                return Ok(leader_id);
            }

            if start.elapsed() > timeout_duration {
                anyhow::bail!("Timeout waiting for consensus");
            }

            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Kill a specific node
    pub async fn kill_node(&mut self, node_id: u64) -> Result<()> {
        let node = self.get_node_mut(node_id)
            .context("Node not found")?;
        node.kill().await
    }

    /// Restart a specific node
    pub async fn restart_node(&mut self, node_id: u64) -> Result<()> {
        let node = self.get_node_mut(node_id)
            .context("Node not found")?;
        node.restart().await
    }

    /// Partition network: isolate nodes from rest of cluster
    pub async fn partition_network(&mut self, isolated_nodes: Vec<u64>) -> Result<()> {
        if let Some(ref mut toxiproxy) = self.toxiproxy {
            toxiproxy.partition(isolated_nodes.clone()).await?;
            info!(nodes = ?isolated_nodes, "Network partition created");
        } else {
            anyhow::bail!("Toxiproxy not enabled for this cluster");
        }
        Ok(())
    }

    /// Heal network partition
    pub async fn heal_partition(&mut self) -> Result<()> {
        if let Some(ref mut toxiproxy) = self.toxiproxy {
            toxiproxy.heal().await?;
            info!("Network partition healed");
        } else {
            anyhow::bail!("Toxiproxy not enabled for this cluster");
        }
        Ok(())
    }

    /// Add latency to specific node connections
    pub async fn add_latency(&mut self, node_id: u64, latency_ms: u32) -> Result<()> {
        if let Some(ref mut toxiproxy) = self.toxiproxy {
            toxiproxy.add_latency(node_id, latency_ms).await?;
            info!(node_id, latency_ms, "Latency added");
        } else {
            anyhow::bail!("Toxiproxy not enabled for this cluster");
        }
        Ok(())
    }

    /// Cleanup all resources
    pub async fn cleanup(&mut self) {
        for node in &mut self.nodes {
            let _ = node.kill().await;
        }
    }
}

impl Drop for TestCluster {
    fn drop(&mut self) {
        // Best-effort cleanup
        for node in &mut self.nodes {
            let _ = node.process.start_kill();
        }
    }
}

/// Spawn a test cluster with N nodes
///
/// # Example
/// ```no_run
/// use chronik_raft_tests::common::*;
///
/// #[tokio::test]
/// async fn test_cluster() {
///     let cluster = spawn_test_cluster(3).await.unwrap();
///     let leader = cluster.wait_for_leader(Duration::from_secs(10)).await.unwrap();
///     // ... test logic
/// }
/// ```
pub async fn spawn_test_cluster(node_count: usize) -> Result<TestCluster> {
    spawn_test_cluster_with_config(ClusterConfig {
        node_count,
        ..Default::default()
    }).await
}

/// Spawn test cluster with custom configuration
pub async fn spawn_test_cluster_with_config(config: ClusterConfig) -> Result<TestCluster> {
    info!(node_count = config.node_count, "Spawning test cluster");

    // Start Toxiproxy if enabled
    let toxiproxy = if config.use_toxiproxy {
        Some(ToxiproxyContainer::start().await?)
    } else {
        None
    };

    let mut nodes = Vec::new();
    let mut peer_addrs = Vec::new();

    // Build peer addresses
    for i in 0..config.node_count {
        let raft_port = config.base_raft_port + i as u16;
        peer_addrs.push(format!("127.0.0.1:{}", raft_port));
    }

    // Spawn each node
    for i in 0..config.node_count {
        let node_id = (i + 1) as u64;
        let raft_port = config.base_raft_port + i as u16;
        let kafka_port = config.base_kafka_port + i as u16;
        let http_port = config.base_http_port + i as u16;

        let raft_addr: SocketAddr = format!("127.0.0.1:{}", raft_port).parse()?;
        let kafka_addr: SocketAddr = format!("127.0.0.1:{}", kafka_port).parse()?;
        let http_addr: SocketAddr = format!("127.0.0.1:{}", http_port).parse()?;

        let data_dir = TempDir::new()?;

        let mut cmd = Command::new(get_chronik_binary()?);
        cmd.arg("raft-cluster")
            .arg("--node-id").arg(node_id.to_string())
            .arg("--raft-addr").arg(raft_addr.to_string())
            .arg("--kafka-addr").arg(kafka_addr.to_string())
            .arg("--http-addr").arg(http_addr.to_string())
            .arg("--peers").arg(peer_addrs.join(","))
            .arg("--data-dir").arg(data_dir.path())
            .arg("--election-timeout").arg(config.election_timeout_ms.to_string())
            .arg("--heartbeat-interval").arg(config.heartbeat_interval_ms.to_string())
            .env("RUST_LOG", "info,chronik_raft=debug");

        let process = cmd.spawn().context("Failed to spawn node process")?;

        nodes.push(TestNode {
            node_id,
            raft_addr,
            kafka_addr,
            http_addr,
            process,
            data_dir,
            proxy_name: None,
        });

        info!(node_id, raft_addr = %raft_addr, kafka_addr = %kafka_addr, "Node spawned");
    }

    // Wait for all nodes to be ready
    for node in &nodes {
        wait_for_node_ready(node.http_addr, Duration::from_secs(30)).await?;
    }

    Ok(TestCluster {
        nodes,
        toxiproxy,
        config,
    })
}

/// Wait for a node to be ready
async fn wait_for_node_ready(http_addr: SocketAddr, timeout_duration: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        if check_node_health(http_addr).await {
            return Ok(());
        }

        if start.elapsed() > timeout_duration {
            anyhow::bail!("Timeout waiting for node {}", http_addr);
        }

        sleep(Duration::from_millis(100)).await;
    }
}

/// Check if node is healthy via HTTP
async fn check_node_health(http_addr: SocketAddr) -> bool {
    let url = format!("http://{}/health", http_addr);
    reqwest::get(&url).await.is_ok()
}

/// Get node info from HTTP API
async fn get_node_info(http_addr: SocketAddr) -> Result<NodeInfo> {
    let url = format!("http://{}/api/v1/raft/info", http_addr);
    let resp = reqwest::get(&url).await?;
    let info = resp.json::<NodeInfo>().await?;
    Ok(info)
}

/// Node information from API
#[derive(Debug, serde::Deserialize)]
pub struct NodeInfo {
    pub node_id: u64,
    pub is_leader: bool,
    pub current_leader: u64,
    pub term: u64,
    pub commit_index: u64,
    pub last_applied: u64,
}

/// Get path to chronik-server binary
fn get_chronik_binary() -> Result<PathBuf> {
    // Try target/release first, then target/debug
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let release_binary = PathBuf::from(manifest_dir)
        .join("../../target/release/chronik-server");

    if release_binary.exists() {
        return Ok(release_binary);
    }

    let debug_binary = PathBuf::from(manifest_dir)
        .join("../../target/debug/chronik-server");

    if debug_binary.exists() {
        return Ok(debug_binary);
    }

    anyhow::bail!(
        "chronik-server binary not found. Run: cargo build --release --bin chronik-server"
    )
}

/// Toxiproxy container for fault injection
pub struct ToxiproxyContainer {
    #[allow(dead_code)]
    container: Container<'static, ToxiproxyImage>,
    api_url: String,
    proxies: HashMap<u64, String>, // node_id -> proxy_name
}

impl ToxiproxyContainer {
    /// Start Toxiproxy container
    pub async fn start() -> Result<Self> {
        let docker = Box::leak(Box::new(Cli::default()));
        let image = ToxiproxyImage::default();
        let container = docker.run(image);

        let host_port = container.get_host_port_ipv4(8474);
        let api_url = format!("http://127.0.0.1:{}", host_port);

        // Wait for Toxiproxy to be ready
        sleep(Duration::from_secs(1)).await;

        Ok(Self {
            container,
            api_url,
            proxies: HashMap::new(),
        })
    }

    /// Create proxy for a node
    pub async fn create_proxy(&mut self, node_id: u64, listen_port: u16, upstream_port: u16) -> Result<()> {
        let proxy_name = format!("node_{}", node_id);

        let body = serde_json::json!({
            "name": proxy_name,
            "listen": format!("0.0.0.0:{}", listen_port),
            "upstream": format!("host.docker.internal:{}", upstream_port),
            "enabled": true
        });

        let client = reqwest::Client::new();
        client.post(format!("{}/proxies", self.api_url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        self.proxies.insert(node_id, proxy_name);
        Ok(())
    }

    /// Partition network: disable proxies for isolated nodes
    pub async fn partition(&mut self, isolated_nodes: Vec<u64>) -> Result<()> {
        for node_id in isolated_nodes {
            if let Some(proxy_name) = self.proxies.get(&node_id) {
                self.disable_proxy(proxy_name).await?;
            }
        }
        Ok(())
    }

    /// Heal partition: enable all proxies
    pub async fn heal(&mut self) -> Result<()> {
        for proxy_name in self.proxies.values() {
            self.enable_proxy(proxy_name).await?;
        }
        Ok(())
    }

    /// Add latency toxic to proxy
    pub async fn add_latency(&mut self, node_id: u64, latency_ms: u32) -> Result<()> {
        if let Some(proxy_name) = self.proxies.get(&node_id) {
            let body = serde_json::json!({
                "type": "latency",
                "attributes": {
                    "latency": latency_ms
                }
            });

            let client = reqwest::Client::new();
            client.post(format!("{}/proxies/{}/toxics", self.api_url, proxy_name))
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
        }
        Ok(())
    }

    async fn disable_proxy(&self, proxy_name: &str) -> Result<()> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({"enabled": false});
        client.post(format!("{}/proxies/{}", self.api_url, proxy_name))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn enable_proxy(&self, proxy_name: &str) -> Result<()> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({"enabled": true});
        client.post(format!("{}/proxies/{}", self.api_url, proxy_name))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Toxiproxy Docker image
#[derive(Debug, Default)]
struct ToxiproxyImage;

impl Image for ToxiproxyImage {
    type Args = ();

    fn name(&self) -> String {
        "ghcr.io/shopify/toxiproxy".to_string()
    }

    fn tag(&self) -> String {
        "2.5.0".to_string()
    }

    fn ready_conditions(&self) -> Vec<testcontainers::core::WaitFor> {
        vec![testcontainers::core::WaitFor::message_on_stdout("API HTTP server starting")]
    }

    fn expose_ports(&self) -> Vec<u16> {
        vec![8474] // Toxiproxy API port
    }
}
