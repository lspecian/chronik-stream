//! Health check and readiness probe implementation for the ingest node.
//! 
//! Provides comprehensive health checking including:
//! - Liveness: Basic process health
//! - Readiness: Service dependencies and capacity
//! - Startup: Initial bootstrap completion

use crate::handler::RequestHandler;
use chronik_common::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Health status of a component
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Component is healthy
    Healthy,
    /// Component is degraded but functional
    Degraded,
    /// Component is unhealthy
    Unhealthy,
}

/// Health check result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResult {
    /// Overall status
    pub status: HealthStatus,
    /// Individual component checks
    pub components: Vec<ComponentHealth>,
    /// Timestamp of the check
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Time taken to perform checks
    pub duration_ms: u64,
}

/// Individual component health
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// Component name
    pub name: String,
    /// Component status
    pub status: HealthStatus,
    /// Optional error message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Readiness check result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessResult {
    /// Whether the service is ready
    pub ready: bool,
    /// Readiness checks
    pub checks: Vec<ReadinessCheck>,
    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Individual readiness check
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessCheck {
    /// Check name
    pub name: String,
    /// Whether the check passed
    pub ready: bool,
    /// Optional message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Health check service
pub struct HealthService {
    /// Request handler for checking internal state
    handler: Arc<RequestHandler>,
    /// Startup time
    startup_time: Instant,
    /// Last successful health check
    last_health_check: Arc<RwLock<Option<Instant>>>,
    /// Health check interval
    check_interval: Duration,
}

impl HealthService {
    /// Create a new health service
    pub fn new(handler: Arc<RequestHandler>) -> Self {
        Self {
            handler,
            startup_time: Instant::now(),
            last_health_check: Arc::new(RwLock::new(None)),
            check_interval: Duration::from_secs(30),
        }
    }
    
    /// Perform liveness check
    pub async fn liveness(&self) -> Result<HealthCheckResult> {
        let start = Instant::now();
        let mut components = Vec::new();
        
        // Basic process health
        components.push(ComponentHealth {
            name: "process".to_string(),
            status: HealthStatus::Healthy,
            error: None,
            metadata: Some(serde_json::json!({
                "uptime_seconds": self.startup_time.elapsed().as_secs(),
                "memory_usage_mb": self.get_memory_usage_mb(),
            })),
        });
        
        // Check if handler is responsive
        let handler_status = if self.is_handler_responsive().await {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
        };
        
        components.push(ComponentHealth {
            name: "handler".to_string(),
            status: handler_status,
            error: if handler_status == HealthStatus::Unhealthy {
                Some("Handler not responsive".to_string())
            } else {
                None
            },
            metadata: None,
        });
        
        let overall_status = if components.iter().all(|c| c.status == HealthStatus::Healthy) {
            HealthStatus::Healthy
        } else if components.iter().any(|c| c.status == HealthStatus::Unhealthy) {
            HealthStatus::Unhealthy
        } else {
            HealthStatus::Degraded
        };
        
        Ok(HealthCheckResult {
            status: overall_status,
            components,
            timestamp: chrono::Utc::now(),
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
    
    /// Perform readiness check
    pub async fn readiness(&self) -> Result<ReadinessResult> {
        let mut checks = Vec::new();
        
        // Check startup completion (minimum uptime)
        let uptime = self.startup_time.elapsed();
        let startup_ready = uptime >= Duration::from_secs(5);
        checks.push(ReadinessCheck {
            name: "startup".to_string(),
            ready: startup_ready,
            message: if !startup_ready {
                Some(format!("Still starting up, uptime: {:?}", uptime))
            } else {
                None
            },
        });
        
        // Check storage connectivity
        let storage_ready = self.check_storage_ready().await;
        checks.push(ReadinessCheck {
            name: "storage".to_string(),
            ready: storage_ready,
            message: if !storage_ready {
                Some("Storage not accessible".to_string())
            } else {
                None
            },
        });
        
        // Check metadata store connectivity
        let metadata_ready = self.check_metadata_ready().await;
        checks.push(ReadinessCheck {
            name: "metadata_store".to_string(),
            ready: metadata_ready,
            message: if !metadata_ready {
                Some("Metadata store not accessible".to_string())
            } else {
                None
            },
        });
        
        // Check capacity (not overloaded)
        let capacity_ready = self.check_capacity().await;
        checks.push(ReadinessCheck {
            name: "capacity".to_string(),
            ready: capacity_ready,
            message: if !capacity_ready {
                Some("Service at capacity".to_string())
            } else {
                None
            },
        });
        
        let ready = checks.iter().all(|c| c.ready);
        
        Ok(ReadinessResult {
            ready,
            checks,
            timestamp: chrono::Utc::now(),
        })
    }
    
    /// Perform comprehensive health check
    pub async fn health(&self) -> Result<HealthCheckResult> {
        let start = Instant::now();
        let mut components = Vec::new();
        
        // Liveness components
        let liveness = self.liveness().await?;
        components.extend(liveness.components);
        
        // Storage health
        let storage_health = self.check_storage_health().await;
        components.push(ComponentHealth {
            name: "storage".to_string(),
            status: storage_health.0,
            error: storage_health.1,
            metadata: None,
        });
        
        // Metadata store health
        let metadata_health = self.check_metadata_health().await;
        components.push(ComponentHealth {
            name: "metadata_store".to_string(),
            status: metadata_health.0,
            error: metadata_health.1,
            metadata: None,
        });
        
        // Consumer group health
        let group_health = self.check_consumer_group_health().await;
        components.push(ComponentHealth {
            name: "consumer_groups".to_string(),
            status: group_health.0,
            error: group_health.1,
            metadata: group_health.2,
        });
        
        // Performance metrics
        let perf_health = self.check_performance_health().await;
        components.push(ComponentHealth {
            name: "performance".to_string(),
            status: perf_health.0,
            error: perf_health.1,
            metadata: perf_health.2,
        });
        
        // Update last health check time
        *self.last_health_check.write().await = Some(Instant::now());
        
        let overall_status = if components.iter().all(|c| c.status == HealthStatus::Healthy) {
            HealthStatus::Healthy
        } else if components.iter().any(|c| c.status == HealthStatus::Unhealthy) {
            HealthStatus::Unhealthy
        } else {
            HealthStatus::Degraded
        };
        
        Ok(HealthCheckResult {
            status: overall_status,
            components,
            timestamp: chrono::Utc::now(),
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
    
    /// Start background health check task
    pub fn start_background_checks(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.check_interval);
            loop {
                interval.tick().await;
                
                match self.health().await {
                    Ok(result) => {
                        if result.status != HealthStatus::Healthy {
                            warn!("Health check degraded: {:?}", result);
                        } else {
                            debug!("Health check passed");
                        }
                    }
                    Err(e) => {
                        warn!("Health check failed: {}", e);
                    }
                }
            }
        });
    }
    
    // Private helper methods
    
    async fn is_handler_responsive(&self) -> bool {
        // Simple check - can we get the handler metrics
        let metrics = self.handler.metrics();
        metrics.records_produced.load(std::sync::atomic::Ordering::Relaxed) >= 0
    }
    
    async fn check_storage_ready(&self) -> bool {
        // TODO: Implement actual storage check
        true
    }
    
    async fn check_metadata_ready(&self) -> bool {
        // TODO: Implement actual metadata store check
        true
    }
    
    async fn check_capacity(&self) -> bool {
        // Check if we have capacity for new connections
        // TODO: Get actual connection count from server
        true
    }
    
    async fn check_storage_health(&self) -> (HealthStatus, Option<String>) {
        // TODO: Implement comprehensive storage health check
        (HealthStatus::Healthy, None)
    }
    
    async fn check_metadata_health(&self) -> (HealthStatus, Option<String>) {
        // TODO: Implement comprehensive metadata health check
        (HealthStatus::Healthy, None)
    }
    
    async fn check_consumer_group_health(&self) -> (HealthStatus, Option<String>, Option<serde_json::Value>) {
        // TODO: Get actual consumer group stats
        let metadata = serde_json::json!({
            "active_groups": 0,
            "total_members": 0,
            "rebalancing_groups": 0,
        });
        
        (HealthStatus::Healthy, None, Some(metadata))
    }
    
    async fn check_performance_health(&self) -> (HealthStatus, Option<String>, Option<serde_json::Value>) {
        // TODO: Get actual performance metrics
        let metadata = serde_json::json!({
            "request_rate": 0.0,
            "error_rate": 0.0,
            "p99_latency_ms": 0.0,
        });
        
        (HealthStatus::Healthy, None, Some(metadata))
    }
    
    fn get_memory_usage_mb(&self) -> u64 {
        // Simple approximation - in production use proper memory tracking
        #[cfg(target_os = "linux")]
        {
            if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                for line in status.lines() {
                    if line.starts_with("VmRSS:") {
                        if let Some(kb_str) = line.split_whitespace().nth(1) {
                            if let Ok(kb) = kb_str.parse::<u64>() {
                                return kb / 1024;
                            }
                        }
                    }
                }
            }
        }
        0
    }
}

/// HTTP handlers for health endpoints
pub mod http {
    use super::*;
    use axum::{
        extract::State,
        http::StatusCode,
        response::{IntoResponse, Json},
        routing::get,
        Router,
    };
    
    /// Create health check routes
    pub fn routes() -> Router<Arc<HealthService>> {
        Router::new()
            .route("/healthz", get(liveness_handler))
            .route("/readyz", get(readiness_handler))
            .route("/health", get(health_handler))
    }
    
    /// Liveness probe handler
    async fn liveness_handler(
        State(health): State<Arc<HealthService>>,
    ) -> impl IntoResponse {
        match health.liveness().await {
            Ok(result) => {
                let status = match result.status {
                    HealthStatus::Healthy => StatusCode::OK,
                    _ => StatusCode::SERVICE_UNAVAILABLE,
                };
                (status, Json(result))
            }
            Err(e) => {
                let result = HealthCheckResult {
                    status: HealthStatus::Unhealthy,
                    components: vec![ComponentHealth {
                        name: "liveness".to_string(),
                        status: HealthStatus::Unhealthy,
                        error: Some(e.to_string()),
                        metadata: None,
                    }],
                    timestamp: chrono::Utc::now(),
                    duration_ms: 0,
                };
                (StatusCode::INTERNAL_SERVER_ERROR, Json(result))
            }
        }
    }
    
    /// Readiness probe handler
    async fn readiness_handler(
        State(health): State<Arc<HealthService>>,
    ) -> impl IntoResponse {
        match health.readiness().await {
            Ok(result) => {
                let status = if result.ready {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                (status, Json(result))
            }
            Err(e) => {
                let result = ReadinessResult {
                    ready: false,
                    checks: vec![ReadinessCheck {
                        name: "readiness".to_string(),
                        ready: false,
                        message: Some(e.to_string()),
                    }],
                    timestamp: chrono::Utc::now(),
                };
                (StatusCode::INTERNAL_SERVER_ERROR, Json(result))
            }
        }
    }
    
    /// Comprehensive health check handler
    async fn health_handler(
        State(health): State<Arc<HealthService>>,
    ) -> impl IntoResponse {
        match health.health().await {
            Ok(result) => {
                let status = match result.status {
                    HealthStatus::Healthy => StatusCode::OK,
                    HealthStatus::Degraded => StatusCode::OK, // Still return 200 for degraded
                    HealthStatus::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
                };
                (status, Json(result))
            }
            Err(e) => {
                let result = HealthCheckResult {
                    status: HealthStatus::Unhealthy,
                    components: vec![ComponentHealth {
                        name: "health".to_string(),
                        status: HealthStatus::Unhealthy,
                        error: Some(e.to_string()),
                        metadata: None,
                    }],
                    timestamp: chrono::Utc::now(),
                    duration_ms: 0,
                };
                (StatusCode::INTERNAL_SERVER_ERROR, Json(result))
            }
        }
    }
}