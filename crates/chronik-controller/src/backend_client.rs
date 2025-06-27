//! Backend service client for admin API integration.
//!
//! Provides communication with ingest and search nodes for cluster management operations.

use crate::{
    ControllerState, BrokerInfo, TopicConfig,
    metadata_sync::MetadataUpdate,
};
use chronik_common::{Result, Error};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tonic::transport::{Channel, Endpoint};
use tracing::{info, warn, error, debug, span, Level};
use reqwest::{Client as HttpClient, StatusCode};
use serde::{Deserialize, Serialize};

/// Backend service manager for coordinating with cluster nodes
pub struct BackendServiceManager {
    /// HTTP client for REST APIs
    http_client: HttpClient,
    
    /// gRPC channels to ingest nodes
    ingest_channels: Arc<RwLock<HashMap<u32, Channel>>>,
    
    /// Search node endpoints
    search_endpoints: Arc<RwLock<Vec<SearchNodeInfo>>>,
    
    /// Cluster state reference
    cluster_state: Arc<RwLock<ControllerState>>,
    
    /// Connection timeout
    connection_timeout: Duration,
    
    /// Request timeout
    request_timeout: Duration,
}

#[derive(Clone, Debug)]
struct SearchNodeInfo {
    id: String,
    endpoint: String,
    healthy: bool,
    last_check: std::time::Instant,
}

impl BackendServiceManager {
    /// Create new backend service manager
    pub fn new(
        cluster_state: Arc<RwLock<ControllerState>>,
    ) -> Self {
        let http_client = HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to create HTTP client");
        
        Self {
            http_client,
            ingest_channels: Arc::new(RwLock::new(HashMap::new())),
            search_endpoints: Arc::new(RwLock::new(Vec::new())),
            cluster_state,
            connection_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
        }
    }
    
    /// Initialize connections to backend services
    pub async fn initialize(&self) -> Result<()> {
        info!("Initializing backend service connections");
        
        // Connect to ingest nodes (brokers)
        self.connect_to_ingest_nodes().await?;
        
        // Discover and connect to search nodes
        self.discover_search_nodes().await?;
        
        // Start health check loop
        self.start_health_checks().await;
        
        Ok(())
    }
    
    /// Connect to all ingest nodes
    async fn connect_to_ingest_nodes(&self) -> Result<()> {
        let state = self.cluster_state.read().await;
        let brokers: Vec<_> = state.brokers.values()
            .filter(|b| b.status.as_deref() == Some("online"))
            .cloned()
            .collect();
        drop(state);
        
        for broker in brokers {
            if let Err(e) = self.connect_to_ingest_node(&broker).await {
                warn!("Failed to connect to ingest node {}: {}", broker.id, e);
            }
        }
        
        Ok(())
    }
    
    /// Connect to a specific ingest node
    async fn connect_to_ingest_node(&self, broker: &BrokerInfo) -> Result<()> {
        let _span = span!(Level::DEBUG, "connect_ingest", broker_id = %broker.id);
        let _guard = _span.enter();
        
        debug!("Connecting to ingest node {} at {}", broker.id, broker.address);
        
        // For ingest nodes, we use gRPC (though in reality they expose Kafka protocol)
        // This is a simplified example - in production, you'd use the Kafka protocol
        let endpoint = Endpoint::from_shared(format!("http://{}", broker.address))
            .map_err(|e| Error::Network(format!("Invalid endpoint: {}", e)))?
            .timeout(self.connection_timeout)
            .connect_timeout(self.connection_timeout);
        
        match endpoint.connect().await {
            Ok(channel) => {
                info!("Connected to ingest node {}", broker.id);
                let mut channels = self.ingest_channels.write().await;
                channels.insert(broker.id, channel);
                Ok(())
            }
            Err(e) => {
                error!("Failed to connect to ingest node {}: {}", broker.id, e);
                Err(Error::Network(format!("Connection failed: {}", e)))
            }
        }
    }
    
    /// Discover search nodes from cluster configuration
    async fn discover_search_nodes(&self) -> Result<()> {
        // In a real implementation, search nodes would be registered in the cluster state
        // For now, we'll use environment variables or configuration
        let search_endpoints = vec![
            SearchNodeInfo {
                id: "search-1".to_string(),
                endpoint: "http://chronik-search:9200".to_string(),
                healthy: false,
                last_check: std::time::Instant::now(),
            },
        ];
        
        let mut endpoints = self.search_endpoints.write().await;
        *endpoints = search_endpoints;
        
        // Check health of each endpoint
        for endpoint in endpoints.iter_mut() {
            match self.check_search_node_health(&endpoint.endpoint).await {
                Ok(()) => {
                    endpoint.healthy = true;
                    info!("Search node {} is healthy", endpoint.id);
                }
                Err(e) => {
                    endpoint.healthy = false;
                    warn!("Search node {} is unhealthy: {}", endpoint.id, e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Check health of a search node
    async fn check_search_node_health(&self, endpoint: &str) -> Result<()> {
        let url = format!("{}/health", endpoint);
        let response = self.http_client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| Error::Network(format!("Health check failed: {}", e)))?;
        
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Error::Network(format!("Health check returned {}", response.status())))
        }
    }
    
    /// Start background health check loop
    async fn start_health_checks(&self) {
        let manager = Arc::new(self.clone());
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            
            loop {
                interval.tick().await;
                
                // Check ingest nodes
                let state = manager.cluster_state.read().await;
                let brokers: Vec<_> = state.brokers.values().cloned().collect();
                drop(state);
                
                for broker in brokers {
                    let channels = manager.ingest_channels.read().await;
                    if !channels.contains_key(&broker.id) && broker.status.as_deref() == Some("online") {
                        drop(channels);
                        let _ = manager.connect_to_ingest_node(&broker).await;
                    }
                }
                
                // Check search nodes
                let mut endpoints = manager.search_endpoints.write().await;
                for endpoint in endpoints.iter_mut() {
                    if endpoint.last_check.elapsed() > Duration::from_secs(30) {
                        endpoint.last_check = std::time::Instant::now();
                        match manager.check_search_node_health(&endpoint.endpoint).await {
                            Ok(()) => endpoint.healthy = true,
                            Err(_) => endpoint.healthy = false,
                        }
                    }
                }
            }
        });
    }
    
    /// Send metadata update to all backend services
    pub async fn broadcast_metadata_update(&self, update: &MetadataUpdate) -> Result<()> {
        let _span = span!(Level::INFO, "broadcast_update", update_type = ?std::mem::discriminant(update));
        let _guard = _span.enter();
        
        info!("Broadcasting metadata update to backend services");
        
        // Send to ingest nodes
        self.send_to_ingest_nodes(update).await?;
        
        // Send to search nodes
        self.send_to_search_nodes(update).await?;
        
        Ok(())
    }
    
    /// Send update to all ingest nodes
    async fn send_to_ingest_nodes(&self, update: &MetadataUpdate) -> Result<()> {
        let channels = self.ingest_channels.read().await;
        
        if channels.is_empty() {
            debug!("No ingest nodes connected");
            return Ok(());
        }
        
        let mut errors = Vec::new();
        
        for (broker_id, _channel) in channels.iter() {
            // In a real implementation, we would send the update via gRPC
            // For now, we'll just log it
            debug!("Would send update to ingest node {}", broker_id);
            
            // Example of what the real implementation might look like:
            // match self.send_update_via_grpc(channel, update).await {
            //     Ok(_) => debug!("Update sent to ingest node {}", broker_id),
            //     Err(e) => errors.push(format!("Node {}: {}", broker_id, e)),
            // }
        }
        
        if !errors.is_empty() {
            warn!("Failed to send updates to some ingest nodes: {:?}", errors);
        }
        
        Ok(())
    }
    
    /// Send update to all search nodes
    async fn send_to_search_nodes(&self, update: &MetadataUpdate) -> Result<()> {
        let endpoints = self.search_endpoints.read().await;
        let healthy_endpoints: Vec<_> = endpoints.iter()
            .filter(|e| e.healthy)
            .cloned()
            .collect();
        drop(endpoints);
        
        if healthy_endpoints.is_empty() {
            debug!("No healthy search nodes available");
            return Ok(());
        }
        
        for endpoint in healthy_endpoints {
            match self.send_update_to_search_node(&endpoint, update).await {
                Ok(_) => debug!("Update sent to search node {}", endpoint.id),
                Err(e) => warn!("Failed to send update to search node {}: {}", endpoint.id, e),
            }
        }
        
        Ok(())
    }
    
    /// Send update to a specific search node
    async fn send_update_to_search_node(&self, node: &SearchNodeInfo, update: &MetadataUpdate) -> Result<()> {
        // Convert update to search-specific format
        let search_update = match update {
            MetadataUpdate::TopicUpdate { topic_name, .. } => {
                SearchMetadataUpdate::TopicUpdate {
                    topic: topic_name.clone(),
                    action: "refresh".to_string(),
                }
            }
            MetadataUpdate::TopicDeletion { topic_name } => {
                SearchMetadataUpdate::TopicUpdate {
                    topic: topic_name.clone(),
                    action: "delete".to_string(),
                }
            }
            _ => return Ok(()), // Skip other update types for search
        };
        
        let url = format!("{}/api/v1/metadata/update", node.endpoint);
        let response = self.http_client
            .post(&url)
            .json(&search_update)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|e| Error::Network(format!("Failed to send update: {}", e)))?;
        
        if !response.status().is_success() {
            return Err(Error::Network(format!("Update failed with status {}", response.status())));
        }
        
        Ok(())
    }
    
    /// Create index for a new topic in search nodes
    pub async fn create_search_index(&self, topic: &TopicConfig) -> Result<()> {
        let _span = span!(Level::INFO, "create_search_index", topic = %topic.name);
        let _guard = _span.enter();
        
        info!("Creating search index for topic {}", topic.name);
        
        let endpoints = self.search_endpoints.read().await;
        let healthy_endpoints: Vec<_> = endpoints.iter()
            .filter(|e| e.healthy)
            .cloned()
            .collect();
        drop(endpoints);
        
        if healthy_endpoints.is_empty() {
            return Err(Error::Unavailable("No healthy search nodes available".to_string()));
        }
        
        // Create index request
        let index_config = SearchIndexConfig {
            settings: serde_json::json!({
                "number_of_shards": topic.partition_count,
                "number_of_replicas": topic.replication_factor.saturating_sub(1),
            }),
            mappings: serde_json::json!({
                "properties": {
                    "timestamp": { "type": "date" },
                    "offset": { "type": "long" },
                    "partition": { "type": "integer" },
                    "key": { "type": "keyword" },
                    "value": { "type": "text" },
                    "headers": { "type": "object" },
                }
            }),
        };
        
        let mut success = false;
        
        for endpoint in healthy_endpoints {
            let url = format!("{}/{}", endpoint.endpoint, topic.name);
            
            match self.http_client
                .put(&url)
                .json(&index_config)
                .timeout(self.request_timeout)
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() || response.status() == StatusCode::BAD_REQUEST {
                        // BAD_REQUEST might mean index already exists
                        success = true;
                        info!("Created search index for topic {} on node {}", topic.name, endpoint.id);
                    } else {
                        warn!("Failed to create index on node {}: {}", endpoint.id, response.status());
                    }
                }
                Err(e) => {
                    warn!("Failed to create index on node {}: {}", endpoint.id, e);
                }
            }
        }
        
        if success {
            Ok(())
        } else {
            Err(Error::Internal("Failed to create index on any search node".to_string()))
        }
    }
    
    /// Delete index for a topic from search nodes
    pub async fn delete_search_index(&self, topic_name: &str) -> Result<()> {
        let _span = span!(Level::INFO, "delete_search_index", topic = %topic_name);
        let _guard = _span.enter();
        
        info!("Deleting search index for topic {}", topic_name);
        
        let endpoints = self.search_endpoints.read().await;
        let healthy_endpoints: Vec<_> = endpoints.iter()
            .filter(|e| e.healthy)
            .cloned()
            .collect();
        drop(endpoints);
        
        for endpoint in healthy_endpoints {
            let url = format!("{}/{}", endpoint.endpoint, topic_name);
            
            match self.http_client
                .delete(&url)
                .timeout(self.request_timeout)
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
                        info!("Deleted search index for topic {} on node {}", topic_name, endpoint.id);
                    } else {
                        warn!("Failed to delete index on node {}: {}", endpoint.id, response.status());
                    }
                }
                Err(e) => {
                    warn!("Failed to delete index on node {}: {}", endpoint.id, e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Get cluster statistics from backend services
    pub async fn get_cluster_stats(&self) -> Result<ClusterStats> {
        let mut stats = ClusterStats::default();
        
        // Get ingest node stats
        let channels = self.ingest_channels.read().await;
        stats.active_ingest_nodes = channels.len();
        stats.total_ingest_nodes = self.cluster_state.read().await.brokers.len();
        
        // Get search node stats
        let endpoints = self.search_endpoints.read().await;
        stats.active_search_nodes = endpoints.iter().filter(|e| e.healthy).count();
        stats.total_search_nodes = endpoints.len();
        
        // Get search cluster health
        for endpoint in endpoints.iter().filter(|e| e.healthy) {
            match self.get_search_cluster_health(&endpoint.endpoint).await {
                Ok(health) => {
                    stats.search_docs_count += health.docs_count;
                    stats.search_index_count += health.index_count;
                }
                Err(e) => {
                    debug!("Failed to get search stats from {}: {}", endpoint.id, e);
                }
            }
        }
        
        Ok(stats)
    }
    
    /// Get search cluster health
    async fn get_search_cluster_health(&self, endpoint: &str) -> Result<SearchClusterHealth> {
        let url = format!("{}/api/v1/cluster/health", endpoint);
        
        let response = self.http_client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| Error::Network(format!("Failed to get cluster health: {}", e)))?;
        
        if !response.status().is_success() {
            return Err(Error::Network(format!("Health request failed: {}", response.status())));
        }
        
        response.json()
            .await
            .map_err(|e| Error::Serde(format!("Failed to parse health response: {}", e)))
    }
}

impl Clone for BackendServiceManager {
    fn clone(&self) -> Self {
        Self {
            http_client: self.http_client.clone(),
            ingest_channels: self.ingest_channels.clone(),
            search_endpoints: self.search_endpoints.clone(),
            cluster_state: self.cluster_state.clone(),
            connection_timeout: self.connection_timeout,
            request_timeout: self.request_timeout,
        }
    }
}

// Message types for backend communication

#[derive(Debug, Serialize, Deserialize)]
struct SearchMetadataUpdate {
    topic: String,
    action: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchIndexConfig {
    settings: serde_json::Value,
    mappings: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchClusterHealth {
    status: String,
    docs_count: usize,
    index_count: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct ClusterStats {
    pub active_ingest_nodes: usize,
    pub total_ingest_nodes: usize,
    pub active_search_nodes: usize,
    pub total_search_nodes: usize,
    pub search_docs_count: usize,
    pub search_index_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    
    #[tokio::test]
    async fn test_backend_service_manager_creation() {
        let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
        let manager = BackendServiceManager::new(cluster_state);
        
        // Verify initial state
        assert_eq!(manager.ingest_channels.read().await.len(), 0);
        assert_eq!(manager.search_endpoints.read().await.len(), 0);
    }
    
    #[tokio::test]
    async fn test_cluster_stats() {
        let mut state = ControllerState::default();
        state.brokers.insert(1, BrokerInfo {
            id: 1,
            address: "127.0.0.1:9092".parse().unwrap(),
            rack: None,
            status: Some("online".to_string()),
            version: Some("1.0.0".to_string()),
            metadata_version: Some(1),
        });
        
        let cluster_state = Arc::new(RwLock::new(state));
        let manager = BackendServiceManager::new(cluster_state);
        
        // Get initial stats
        let stats = manager.get_cluster_stats().await.unwrap();
        assert_eq!(stats.total_ingest_nodes, 1);
        assert_eq!(stats.active_ingest_nodes, 0); // Not connected yet
    }
}