//! Distributed locking mechanism for consumer group coordination
//!
//! This module provides distributed locks using TiKV's transaction API
//! to ensure only one broker can modify a consumer group at a time.

use chronik_common::{Result, Error};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tikv_client::RawClient;
use tracing::{debug, info, warn, error};
use uuid::Uuid;

/// Lock key prefix in TiKV
const LOCK_KEY_PREFIX: &str = "chronik:locks:";

/// Default lock TTL (30 seconds)
const DEFAULT_LOCK_TTL: Duration = Duration::from_secs(30);

/// Lock acquisition retry interval
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Distributed lock using TiKV
pub struct DistributedLock {
    /// TiKV client
    client: Arc<RawClient>,
    /// Lock key
    key: String,
    /// Lock owner ID (unique per lock instance)
    owner_id: String,
    /// Lock expiration time
    expires_at: Option<Instant>,
    /// Whether the lock is currently held
    held: bool,
}

impl DistributedLock {
    /// Create a new distributed lock
    pub fn new(client: Arc<RawClient>, resource: &str) -> Self {
        let key = format!("{}{}", LOCK_KEY_PREFIX, resource);
        let owner_id = Uuid::new_v4().to_string();
        
        Self {
            client,
            key,
            owner_id,
            expires_at: None,
            held: false,
        }
    }
    
    /// Try to acquire the lock
    pub async fn try_acquire(&mut self) -> Result<bool> {
        if self.held {
            return Ok(true); // Already held
        }
        
        let lock_key = self.key.as_bytes().to_vec();
        let lock_value = self.create_lock_value();
        
        // Try to acquire with compare-and-swap
        match self.client.get(lock_key.clone()).await {
            Ok(Some(existing_value)) => {
                // Check if lock is expired
                if self.is_lock_expired(&existing_value) {
                    // Try to acquire expired lock
                    let prev_value = Some(existing_value);
                    match self.client.compare_and_swap(
                        lock_key,
                        prev_value,
                        lock_value
                    ).await {
                        Ok((_, swapped)) => {
                            if swapped {
                                self.held = true;
                                self.expires_at = Some(Instant::now() + DEFAULT_LOCK_TTL);
                                info!("Acquired expired lock for resource: {}", self.key);
                                Ok(true)
                            } else {
                                Ok(false) // Someone else got it first
                            }
                        }
                        Err(e) => {
                            warn!("Failed to acquire expired lock: {}", e);
                            Ok(false)
                        }
                    }
                } else {
                    Ok(false) // Lock is held by someone else
                }
            }
            Ok(None) => {
                // No lock exists, try to create
                match self.client.compare_and_swap(
                    lock_key,
                    None,
                    lock_value
                ).await {
                    Ok((_, swapped)) => {
                        if swapped {
                            self.held = true;
                            self.expires_at = Some(Instant::now() + DEFAULT_LOCK_TTL);
                            info!("Acquired new lock for resource: {}", self.key);
                            Ok(true)
                        } else {
                            Ok(false) // Someone else created it first
                        }
                    }
                    Err(e) => {
                        warn!("Failed to create lock: {}", e);
                        Ok(false)
                    }
                }
            }
            Err(e) => {
                error!("Failed to check lock status: {}", e);
                Err(Error::Storage(format!("Failed to check lock: {}", e)))
            }
        }
    }
    
    /// Acquire the lock, blocking until successful or timeout
    pub async fn acquire(&mut self, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        
        loop {
            if self.try_acquire().await? {
                return Ok(());
            }
            
            if start.elapsed() >= timeout {
                return Err(Error::Internal("Failed to acquire lock within timeout".to_string()));
            }
            
            tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
        }
    }
    
    /// Release the lock
    pub async fn release(&mut self) -> Result<()> {
        if !self.held {
            return Ok(()); // Not held
        }
        
        let lock_key = self.key.as_bytes().to_vec();
        let expected_value = self.create_lock_value();
        
        // Only delete if we still own it
        match self.client.compare_and_swap(
            lock_key,
            Some(expected_value),
            vec![]
        ).await {
            Ok((_, deleted)) => {
                if deleted {
                    self.held = false;
                    self.expires_at = None;
                    info!("Released lock for resource: {}", self.key);
                    Ok(())
                } else {
                    // Lock was already taken by someone else or expired
                    self.held = false;
                    self.expires_at = None;
                    warn!("Lock was already released or taken by another owner");
                    Ok(())
                }
            }
            Err(e) => {
                error!("Failed to release lock: {}", e);
                Err(Error::Storage(format!("Failed to release lock: {}", e)))
            }
        }
    }
    
    /// Extend the lock TTL
    pub async fn extend(&mut self) -> Result<()> {
        if !self.held {
            return Err(Error::Internal("Cannot extend lock that is not held".to_string()));
        }
        
        let lock_key = self.key.as_bytes().to_vec();
        let old_value = self.create_lock_value();
        let new_value = self.create_lock_value(); // Will have updated timestamp
        
        match self.client.compare_and_swap(
            lock_key,
            Some(old_value),
            new_value
        ).await {
            Ok((_, swapped)) => {
                if swapped {
                    self.expires_at = Some(Instant::now() + DEFAULT_LOCK_TTL);
                    debug!("Extended lock TTL for resource: {}", self.key);
                    Ok(())
                } else {
                    // Lost the lock
                    self.held = false;
                    self.expires_at = None;
                    Err(Error::Internal("Lock was lost during extension".to_string()))
                }
            }
            Err(e) => {
                error!("Failed to extend lock: {}", e);
                Err(Error::Storage(format!("Failed to extend lock: {}", e)))
            }
        }
    }
    
    /// Create lock value with owner ID and expiration
    fn create_lock_value(&self) -> Vec<u8> {
        let expires_at = chrono::Utc::now() + chrono::Duration::from_std(DEFAULT_LOCK_TTL).unwrap();
        let value = format!("{}:{}", self.owner_id, expires_at.timestamp());
        value.into_bytes()
    }
    
    /// Check if a lock value is expired
    fn is_lock_expired(&self, value: &[u8]) -> bool {
        match String::from_utf8(value.to_vec()) {
            Ok(s) => {
                if let Some((_owner, timestamp_str)) = s.split_once(':') {
                    if let Ok(timestamp) = timestamp_str.parse::<i64>() {
                        let expires_at = chrono::DateTime::from_timestamp(timestamp, 0);
                        if let Some(expires) = expires_at {
                            return chrono::Utc::now() > expires;
                        }
                    }
                }
                true // Can't parse, assume expired
            }
            Err(_) => true, // Can't parse, assume expired
        }
    }
}

impl Drop for DistributedLock {
    fn drop(&mut self) {
        if self.held {
            // Try to release the lock in a blocking manner
            let client = self.client.clone();
            let key = self.key.clone();
            let value = self.create_lock_value();
            
            // Spawn a task to release the lock
            tokio::spawn(async move {
                let _ = client.compare_and_swap(
                    key.as_bytes().to_vec(),
                    Some(value),
                    vec![]
                ).await;
            });
        }
    }
}

/// Manager for distributed locks
pub struct DistributedLockManager {
    client: Arc<RawClient>,
}

impl DistributedLockManager {
    /// Create a new lock manager
    pub fn new(client: Arc<RawClient>) -> Self {
        Self { client }
    }
    
    /// Create a lock for a consumer group
    pub fn create_group_lock(&self, group_id: &str) -> DistributedLock {
        let resource = format!("group:{}", group_id);
        DistributedLock::new(self.client.clone(), &resource)
    }
    
    /// Create a lock for a topic
    pub fn create_topic_lock(&self, topic: &str) -> DistributedLock {
        let resource = format!("topic:{}", topic);
        DistributedLock::new(self.client.clone(), &resource)
    }
    
    /// Create a lock for offset commits
    pub fn create_offset_lock(&self, group_id: &str, topic: &str, partition: u32) -> DistributedLock {
        let resource = format!("offset:{}:{}:{}", group_id, topic, partition);
        DistributedLock::new(self.client.clone(), &resource)
    }
}

/// Helper to run a function with a distributed lock
pub async fn with_lock<F, R>(
    lock: &mut DistributedLock,
    timeout: Duration,
    f: F,
) -> Result<R>
where
    F: FnOnce() -> R,
{
    lock.acquire(timeout).await?;
    
    let result = f();
    
    lock.release().await?;
    
    Ok(result)
}

/// Helper to run an async function with a distributed lock
pub async fn with_lock_async<F, Fut, R>(
    lock: &mut DistributedLock,
    timeout: Duration,
    f: F,
) -> Result<R>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    lock.acquire(timeout).await?;
    
    let result = f().await;
    
    lock.release().await?;
    
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_distributed_lock_basic() {
        // This test requires a running TiKV instance
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let client = match RawClient::new(pd_endpoints).await {
            Ok(c) => Arc::new(c),
            Err(_) => {
                println!("Skipping test - TiKV not available");
                return;
            }
        };
        
        let mut lock1 = DistributedLock::new(client.clone(), "test-resource");
        let mut lock2 = DistributedLock::new(client.clone(), "test-resource");
        
        // First lock should succeed
        assert!(lock1.try_acquire().await.unwrap());
        
        // Second lock should fail
        assert!(!lock2.try_acquire().await.unwrap());
        
        // Release first lock
        lock1.release().await.unwrap();
        
        // Now second lock should succeed
        assert!(lock2.try_acquire().await.unwrap());
        
        // Clean up
        lock2.release().await.unwrap();
    }
    
    #[tokio::test]
    async fn test_lock_expiration() {
        // This test requires a running TiKV instance
        let pd_endpoints = vec!["localhost:2379".to_string()];
        let client = match RawClient::new(pd_endpoints).await {
            Ok(c) => Arc::new(c),
            Err(_) => {
                println!("Skipping test - TiKV not available");
                return;
            }
        };
        
        // Create a lock with very short TTL for testing
        let resource = "test-expiration";
        let key = format!("{}{}", LOCK_KEY_PREFIX, resource);
        
        // Manually create an expired lock
        let expired_value = format!("test-owner:{}", 
            (chrono::Utc::now() - chrono::Duration::minutes(1)).timestamp()
        );
        client.put(key.as_bytes().to_vec(), expired_value.into_bytes()).await.unwrap();
        
        // Should be able to acquire the expired lock
        let mut lock = DistributedLock::new(client.clone(), resource);
        assert!(lock.try_acquire().await.unwrap());
        
        // Clean up
        lock.release().await.unwrap();
    }
}