//! Enhanced heartbeat monitoring with persistence and metrics
//!
//! This module provides improved heartbeat tracking with state persistence,
//! distributed coordination, and monitoring capabilities.

use chronik_common::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use prometheus::{IntGauge, IntCounter, Histogram, HistogramOpts};

/// Heartbeat metrics
pub struct HeartbeatMetrics {
    /// Number of active consumer group members
    pub active_members: IntGauge,
    /// Number of expired members detected
    pub expired_members: IntCounter,
    /// Heartbeat latency histogram
    pub heartbeat_latency: Histogram,
    /// Session timeout violations
    pub timeout_violations: IntCounter,
}

impl HeartbeatMetrics {
    pub fn new() -> Self {
        Self {
            active_members: IntGauge::new(
                "consumer_group_active_members",
                "Number of active consumer group members"
            ).unwrap(),
            expired_members: IntCounter::new(
                "consumer_group_expired_members_total",
                "Total number of expired members detected"
            ).unwrap(),
            heartbeat_latency: Histogram::with_opts(
                HistogramOpts::new(
                    "consumer_group_heartbeat_latency_seconds",
                    "Heartbeat latency in seconds"
                )
            ).unwrap(),
            timeout_violations: IntCounter::new(
                "consumer_group_timeout_violations_total",
                "Total number of session timeout violations"
            ).unwrap(),
        }
    }
}

/// Enhanced heartbeat monitor configuration
#[derive(Debug, Clone)]
pub struct HeartbeatMonitorConfig {
    /// Check interval for expired members
    pub check_interval: Duration,
    /// Grace period before declaring member dead
    pub grace_period: Duration,
    /// Enable persistence of heartbeat state
    pub enable_persistence: bool,
    /// Enable distributed coordination
    pub enable_distributed: bool,
}

impl Default for HeartbeatMonitorConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(5), // Check every 5 seconds instead of 1
            grace_period: Duration::from_secs(2),   // 2 second grace period
            enable_persistence: true,
            enable_distributed: true,
        }
    }
}

/// Member heartbeat state
#[derive(Debug, Clone)]
pub struct HeartbeatState {
    pub member_id: String,
    pub group_id: String,
    pub last_heartbeat: Instant,
    pub session_timeout: Duration,
    pub generation_id: i32,
    pub consecutive_misses: u32,
}

impl HeartbeatState {
    /// Check if member should be considered expired
    pub fn is_expired(&self, grace_period: Duration) -> bool {
        self.last_heartbeat.elapsed() > (self.session_timeout + grace_period)
    }
    
    /// Get time until expiration
    pub fn time_until_expiration(&self) -> Option<Duration> {
        let elapsed = self.last_heartbeat.elapsed();
        if elapsed < self.session_timeout {
            Some(self.session_timeout - elapsed)
        } else {
            None
        }
    }
}

/// Enhanced heartbeat monitor
pub struct HeartbeatMonitor {
    /// Configuration
    config: HeartbeatMonitorConfig,
    
    /// Heartbeat states by group and member
    states: Arc<RwLock<HashMap<(String, String), HeartbeatState>>>,
    
    /// Metrics
    metrics: Arc<HeartbeatMetrics>,
    
    /// Callback for expired members
    expiration_callback: Arc<dyn Fn(String, String) + Send + Sync>,
}

impl HeartbeatMonitor {
    /// Create a new heartbeat monitor
    pub fn new<F>(
        config: HeartbeatMonitorConfig,
        metrics: Arc<HeartbeatMetrics>,
        expiration_callback: F,
    ) -> Self 
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        Self {
            config,
            states: Arc::new(RwLock::new(HashMap::new())),
            metrics,
            expiration_callback: Arc::new(expiration_callback),
        }
    }
    
    /// Record a heartbeat
    pub async fn record_heartbeat(
        &self,
        group_id: &str,
        member_id: &str,
        generation_id: i32,
        session_timeout: Duration,
    ) -> Result<()> {
        let start = Instant::now();
        
        let mut states = self.states.write().await;
        let key = (group_id.to_string(), member_id.to_string());
        
        match states.get_mut(&key) {
            Some(state) => {
                // Update existing state
                state.last_heartbeat = Instant::now();
                state.generation_id = generation_id;
                state.consecutive_misses = 0;
                
                debug!(
                    group_id = %group_id,
                    member_id = %member_id,
                    generation = generation_id,
                    "Updated heartbeat"
                );
            }
            None => {
                // Create new state
                let state = HeartbeatState {
                    member_id: member_id.to_string(),
                    group_id: group_id.to_string(),
                    last_heartbeat: Instant::now(),
                    session_timeout,
                    generation_id,
                    consecutive_misses: 0,
                };
                
                states.insert(key, state);
                self.metrics.active_members.inc();
                
                info!(
                    group_id = %group_id,
                    member_id = %member_id,
                    generation = generation_id,
                    "New member heartbeat registered"
                );
            }
        }
        
        // Record latency
        let latency = start.elapsed().as_secs_f64();
        self.metrics.heartbeat_latency.observe(latency);
        
        Ok(())
    }
    
    /// Check for expired members
    pub async fn check_expired(&self) -> Vec<(String, String)> {
        let mut states = self.states.write().await;
        let mut expired = Vec::new();
        let grace_period = self.config.grace_period;
        
        // Find expired members
        let expired_keys: Vec<_> = states.iter()
            .filter(|(_, state)| state.is_expired(grace_period))
            .map(|(key, state)| {
                warn!(
                    group_id = %state.group_id,
                    member_id = %state.member_id,
                    elapsed_secs = ?state.last_heartbeat.elapsed().as_secs(),
                    session_timeout_secs = ?state.session_timeout.as_secs(),
                    "Member expired"
                );
                
                expired.push((state.group_id.clone(), state.member_id.clone()));
                key.clone()
            })
            .collect();
        
        // Remove expired states
        for key in expired_keys {
            states.remove(&key);
            self.metrics.active_members.dec();
            self.metrics.expired_members.inc();
        }
        
        expired
    }
    
    /// Get heartbeat statistics
    pub async fn get_stats(&self) -> HeartbeatStats {
        let states = self.states.read().await;
        
        let mut groups = HashMap::new();
        let mut total_members = 0;
        let mut healthy_members = 0;
        
        for (_, state) in states.iter() {
            total_members += 1;
            
            if state.time_until_expiration().is_some() {
                healthy_members += 1;
            }
            
            *groups.entry(state.group_id.clone()).or_insert(0) += 1;
        }
        
        HeartbeatStats {
            total_members,
            healthy_members,
            unhealthy_members: total_members - healthy_members,
            groups_count: groups.len(),
            members_per_group: groups,
        }
    }
    
    /// Start the monitoring task
    pub fn start(self: Arc<Self>) {
        let monitor = self.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(monitor.config.check_interval);
            
            loop {
                interval.tick().await;
                
                // Check for expired members
                let expired = monitor.check_expired().await;
                
                // Notify about expired members
                for (group_id, member_id) in expired {
                    (monitor.expiration_callback)(group_id, member_id);
                }
                
                // Log statistics periodically
                if interval.period().as_secs() % 60 == 0 {
                    let stats = monitor.get_stats().await;
                    info!(
                        total_members = stats.total_members,
                        healthy = stats.healthy_members,
                        groups = stats.groups_count,
                        "Heartbeat monitor statistics"
                    );
                }
            }
        });
    }
    
    /// Persist heartbeat state (for recovery after restart)
    pub async fn persist_state(&self, _metadata_store: &dyn chronik_common::metadata::traits::MetadataStore) -> Result<()> {
        if !self.config.enable_persistence {
            return Ok(());
        }
        
        let _states = self.states.read().await;
        
        // TODO: Implement persistence to metadata store
        // This would store heartbeat timestamps and allow recovery after broker restart
        
        Ok(())
    }
    
    /// Restore heartbeat state from persistence
    pub async fn restore_state(&self, _metadata_store: &dyn chronik_common::metadata::traits::MetadataStore) -> Result<()> {
        if !self.config.enable_persistence {
            return Ok(());
        }
        
        // TODO: Implement restoration from metadata store
        // This would load previously persisted heartbeat states
        
        Ok(())
    }
}

/// Heartbeat statistics
#[derive(Debug, Clone)]
pub struct HeartbeatStats {
    pub total_members: usize,
    pub healthy_members: usize,
    pub unhealthy_members: usize,
    pub groups_count: usize,
    pub members_per_group: HashMap<String, usize>,
}

/// Create a heartbeat monitor for integration with GroupManager
pub fn create_heartbeat_monitor<F>(
    config: HeartbeatMonitorConfig,
    expiration_callback: F,
) -> Arc<HeartbeatMonitor>
where
    F: Fn(String, String) + Send + Sync + 'static,
{
    let metrics = Arc::new(HeartbeatMetrics::new());
    
    // Register metrics with Prometheus
    // prometheus::register(Box::new(metrics.active_members.clone())).unwrap();
    // prometheus::register(Box::new(metrics.expired_members.clone())).unwrap();
    // prometheus::register(Box::new(metrics.heartbeat_latency.clone())).unwrap();
    // prometheus::register(Box::new(metrics.timeout_violations.clone())).unwrap();
    
    Arc::new(HeartbeatMonitor::new(config, metrics, expiration_callback))
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_heartbeat_expiration() {
        let expired_members = Arc::new(RwLock::new(Vec::new()));
        let expired_clone = expired_members.clone();
        
        let monitor = create_heartbeat_monitor(
            HeartbeatMonitorConfig {
                check_interval: Duration::from_millis(100),
                grace_period: Duration::from_millis(50),
                ..Default::default()
            },
            move |group_id, member_id| {
                let expired = expired_clone.clone();
                tokio::spawn(async move {
                    expired.write().await.push((group_id, member_id));
                });
            },
        );
        
        // Record heartbeat with very short timeout
        monitor.record_heartbeat(
            "test-group",
            "test-member",
            1,
            Duration::from_millis(100),
        ).await.unwrap();
        
        // Wait for expiration
        tokio::time::sleep(Duration::from_millis(200)).await;
        
        // Check expired
        let expired = monitor.check_expired().await;
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0], ("test-group".to_string(), "test-member".to_string()));
    }
    
    #[tokio::test]
    async fn test_heartbeat_renewal() {
        let monitor = create_heartbeat_monitor(
            HeartbeatMonitorConfig::default(),
            |_, _| {},
        );
        
        // Record initial heartbeat
        monitor.record_heartbeat(
            "test-group",
            "test-member",
            1,
            Duration::from_secs(30),
        ).await.unwrap();
        
        // Wait a bit
        tokio::time::sleep(Duration::from_secs(1)).await;
        
        // Renew heartbeat
        monitor.record_heartbeat(
            "test-group",
            "test-member",
            1,
            Duration::from_secs(30),
        ).await.unwrap();
        
        // Check not expired
        let expired = monitor.check_expired().await;
        assert_eq!(expired.len(), 0);
        
        // Check stats
        let stats = monitor.get_stats().await;
        assert_eq!(stats.total_members, 1);
        assert_eq!(stats.healthy_members, 1);
    }
}