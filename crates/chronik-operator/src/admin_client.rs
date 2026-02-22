use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::OperatorError;

/// HTTP client for the Chronik Admin API.
///
/// Communicates with `http://<host>:<10000 + node_id>/admin/...`
pub struct AdminClient {
    http: reqwest::Client,
    api_key: Option<String>,
}

impl AdminClient {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("HTTP client should build"),
            api_key,
        }
    }

    /// Health check — GET /admin/health (no auth required).
    pub async fn health(&self, base_url: &str) -> Result<HealthResponse, OperatorError> {
        let url = format!("{base_url}/admin/health");
        debug!("Health check: {url}");
        let resp = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .map_err(|e| OperatorError::AdminApi(format!("Health check failed: {e}")))?;
        Ok(resp.json().await?)
    }

    /// Cluster status — GET /admin/status.
    pub async fn status(&self, base_url: &str) -> Result<ClusterStatusResponse, OperatorError> {
        let url = format!("{base_url}/admin/status");
        debug!("Cluster status: {url}");
        let mut req = self.http.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.header("X-API-Key", key);
        }
        let resp = req
            .send()
            .await?
            .error_for_status()
            .map_err(|e| OperatorError::AdminApi(format!("Status query failed: {e}")))?;
        Ok(resp.json().await?)
    }

    /// Add a node to the cluster — POST /admin/add-node.
    pub async fn add_node(
        &self,
        base_url: &str,
        request: &AddNodeRequest,
    ) -> Result<AddNodeResponse, OperatorError> {
        let url = format!("{base_url}/admin/add-node");
        debug!("Add node {}: {url}", request.node_id);
        let mut req = self.http.post(&url).json(request);
        if let Some(ref key) = self.api_key {
            req = req.header("X-API-Key", key);
        }
        let resp = req.send().await?.error_for_status().map_err(|e| {
            OperatorError::AdminApi(format!("Add node {} failed: {e}", request.node_id))
        })?;
        Ok(resp.json().await?)
    }

    /// Remove a node from the cluster — POST /admin/remove-node.
    pub async fn remove_node(
        &self,
        base_url: &str,
        request: &RemoveNodeRequest,
    ) -> Result<RemoveNodeResponse, OperatorError> {
        let url = format!("{base_url}/admin/remove-node");
        debug!("Remove node {}: {url}", request.node_id);
        let mut req = self.http.post(&url).json(request);
        if let Some(ref key) = self.api_key {
            req = req.header("X-API-Key", key);
        }
        let resp = req.send().await?.error_for_status().map_err(|e| {
            OperatorError::AdminApi(format!("Remove node {} failed: {e}", request.node_id))
        })?;
        Ok(resp.json().await?)
    }

    /// Find the leader node by polling health on all nodes.
    ///
    /// Returns (leader_base_url, leader_node_id) or None if no leader found.
    pub async fn find_leader(&self, base_urls: &[(u64, String)]) -> Option<(String, u64)> {
        for (node_id, url) in base_urls {
            match self.health(url).await {
                Ok(health) if health.is_leader => {
                    return Some((url.clone(), *node_id));
                }
                Ok(_) => {} // Not leader
                Err(e) => {
                    warn!(node_id, "Health check failed: {e}");
                }
            }
        }
        None
    }

    /// Build the admin API base URL for a given node.
    ///
    /// In K8s, each Pod has its own IP, so admin API port = 10000 + node_id.
    pub fn admin_url(host: &str, node_id: u64) -> String {
        let port = 10000 + node_id;
        format!("http://{host}:{port}")
    }

    /// Build admin URL using the per-node Service DNS name.
    pub fn admin_url_from_dns(cluster_name: &str, node_id: u64, namespace: &str) -> String {
        let dns = crate::config_generator::node_dns_name(cluster_name, node_id, namespace);
        let port = 10000 + node_id;
        format!("http://{dns}:{port}")
    }
}

// --- Request/Response types (matching Chronik's admin_api.rs) ---

#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub node_id: u64,
    pub is_leader: bool,
    #[serde(default)]
    pub cluster_nodes: Vec<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ClusterStatusResponse {
    pub node_id: u64,
    pub leader_id: Option<u64>,
    pub is_leader: bool,
    #[serde(default)]
    pub nodes: Vec<NodeInfo>,
    #[serde(default)]
    pub partitions: Vec<PartitionInfo>,
}

#[derive(Debug, Deserialize)]
pub struct NodeInfo {
    pub node_id: u64,
    pub address: String,
    pub is_leader: bool,
}

#[derive(Debug, Deserialize)]
pub struct PartitionInfo {
    pub topic: String,
    pub partition: i32,
    pub leader: Option<u64>,
    #[serde(default)]
    pub replicas: Vec<u64>,
    #[serde(default)]
    pub isr: Vec<u64>,
}

#[derive(Debug, Serialize)]
pub struct AddNodeRequest {
    pub node_id: u64,
    pub kafka_addr: String,
    pub wal_addr: String,
    pub raft_addr: String,
}

#[derive(Debug, Deserialize)]
pub struct AddNodeResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct RemoveNodeRequest {
    pub node_id: u64,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
pub struct RemoveNodeResponse {
    pub success: bool,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_admin_url() {
        assert_eq!(
            AdminClient::admin_url("10.244.1.5", 3),
            "http://10.244.1.5:10003"
        );
    }

    #[test]
    fn test_admin_url_from_dns() {
        assert_eq!(
            AdminClient::admin_url_from_dns("prod", 2, "default"),
            "http://prod-2.default.svc.cluster.local:10002"
        );
    }

    #[test]
    fn test_add_node_request_serialization() {
        let req = AddNodeRequest {
            node_id: 4,
            kafka_addr: "host:9092".into(),
            wal_addr: "host:9291".into(),
            raft_addr: "host:5001".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"node_id\":4"));
        assert!(json.contains("\"kafka_addr\":\"host:9092\""));
    }

    #[test]
    fn test_health_response_deserialization() {
        let json = r#"{"status":"ok","node_id":1,"is_leader":true,"cluster_nodes":[1,2,3]}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert!(resp.is_leader);
        assert_eq!(resp.node_id, 1);
        assert_eq!(resp.cluster_nodes.len(), 3);
    }

    #[test]
    fn test_cluster_status_response_deserialization() {
        let json = r#"{"node_id":1,"leader_id":1,"is_leader":true,"nodes":[],"partitions":[]}"#;
        let resp: ClusterStatusResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.leader_id, Some(1));
        assert!(resp.is_leader);
    }
}
