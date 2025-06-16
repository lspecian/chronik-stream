//! Retry logic and exponential backoff for object storage operations.

use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::object_store::errors::ObjectStoreError;

/// Retry configuration
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    
    /// Initial retry delay
    pub initial_delay: Duration,
    
    /// Maximum retry delay
    pub max_delay: Duration,
    
    /// Backoff multiplier
    pub backoff_multiplier: f32,
    
    /// Jitter factor (0.0 to 1.0)
    pub jitter_factor: f32,
    
    /// Retry timeout (total time across all attempts)
    pub timeout: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 2.0,
            jitter_factor: 0.1,
            timeout: Duration::from_secs(60),
        }
    }
}

/// Exponential backoff implementation
pub struct ExponentialBackoff {
    config: RetryConfig,
    attempt: u32,
    start_time: Instant,
}

impl ExponentialBackoff {
    /// Create a new exponential backoff instance
    pub fn new(config: RetryConfig) -> Self {
        Self {
            config,
            attempt: 0,
            start_time: Instant::now(),
        }
    }
    
    /// Check if we should retry
    pub fn should_retry(&self, error: &ObjectStoreError) -> bool {
        // Don't retry if we've exceeded max attempts
        if self.attempt >= self.config.max_attempts {
            return false;
        }
        
        // Don't retry if we've exceeded timeout
        if self.start_time.elapsed() >= self.config.timeout {
            return false;
        }
        
        // Only retry if the error is retryable
        error.is_retryable()
    }
    
    /// Calculate delay for next retry
    pub fn next_delay(&mut self) -> Duration {
        self.attempt += 1;
        
        // Calculate base delay with exponential backoff
        let base_delay = Duration::from_millis(
            (self.config.initial_delay.as_millis() as f32 
                * self.config.backoff_multiplier.powi(self.attempt as i32 - 1)) as u64
        );
        
        // Cap at max delay
        let capped_delay = base_delay.min(self.config.max_delay);
        
        // Add jitter to avoid thundering herd
        let jitter = if self.config.jitter_factor > 0.0 {
            let jitter_ms = (capped_delay.as_millis() as f32 * self.config.jitter_factor) as u64;
            Duration::from_millis(fastrand::u64(0..=jitter_ms))
        } else {
            Duration::ZERO
        };
        
        capped_delay + jitter
    }
    
    /// Get current attempt number
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
    
    /// Get elapsed time since start
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }
}

/// Retry helper function
pub async fn retry_with_backoff<F, T, E>(
    config: RetryConfig,
    operation_name: &str,
    mut operation: F,
) -> Result<T, ObjectStoreError>
where
    F: FnMut() -> Result<T, E>,
    E: Into<ObjectStoreError>,
{
    let mut backoff = ExponentialBackoff::new(config);
    let mut last_error: Option<ObjectStoreError> = None;
    
    loop {
        match operation() {
            Ok(result) => {
                if backoff.attempt() > 0 {
                    debug!(
                        "Operation '{}' succeeded after {} attempts in {:?}",
                        operation_name,
                        backoff.attempt(),
                        backoff.elapsed()
                    );
                }
                return Ok(result);
            }
            Err(error) => {
                let storage_error: ObjectStoreError = error.into();
                
                if !backoff.should_retry(&storage_error) {
                    if let Some(last_err) = last_error {
                        return Err(ObjectStoreError::RetryExhausted {
                            attempts: backoff.attempt(),
                            last_error: last_err.to_string(),
                        });
                    } else {
                        return Err(storage_error);
                    }
                }
                
                let delay = backoff.next_delay();
                
                warn!(
                    "Operation '{}' failed (attempt {}/{}): {}. Retrying in {:?}...",
                    operation_name,
                    backoff.attempt(),
                    backoff.config.max_attempts,
                    storage_error,
                    delay
                );
                
                last_error = Some(storage_error);
                sleep(delay).await;
            }
        }
    }
}

/// Retry helper for async operations
pub async fn retry_async_with_backoff<F, Fut, T, E>(
    config: RetryConfig,
    operation_name: &str,
    mut operation: F,
) -> Result<T, ObjectStoreError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: Into<ObjectStoreError>,
{
    let mut backoff = ExponentialBackoff::new(config);
    let mut last_error: Option<ObjectStoreError> = None;
    
    loop {
        match operation().await {
            Ok(result) => {
                if backoff.attempt() > 0 {
                    debug!(
                        "Async operation '{}' succeeded after {} attempts in {:?}",
                        operation_name,
                        backoff.attempt(),
                        backoff.elapsed()
                    );
                }
                return Ok(result);
            }
            Err(error) => {
                let storage_error: ObjectStoreError = error.into();
                
                if !backoff.should_retry(&storage_error) {
                    if let Some(last_err) = last_error {
                        return Err(ObjectStoreError::RetryExhausted {
                            attempts: backoff.attempt(),
                            last_error: last_err.to_string(),
                        });
                    } else {
                        return Err(storage_error);
                    }
                }
                
                let delay = backoff.next_delay();
                
                warn!(
                    "Async operation '{}' failed (attempt {}/{}): {}. Retrying in {:?}...",
                    operation_name,
                    backoff.attempt(),
                    backoff.config.max_attempts,
                    storage_error,
                    delay
                );
                
                last_error = Some(storage_error);
                sleep(delay).await;
            }
        }
    }
}

/// Retry policy for different error types
#[derive(Debug, Clone)]
pub enum RetryPolicy {
    /// No retry
    Never,
    
    /// Retry with fixed delay
    Fixed {
        delay: Duration,
        max_attempts: u32,
    },
    
    /// Retry with exponential backoff
    Exponential(RetryConfig),
    
    /// Custom retry policy
    Custom {
        should_retry: fn(&ObjectStoreError) -> bool,
        delay: fn(u32) -> Duration,
        max_attempts: u32,
    },
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy::Exponential(RetryConfig::default())
    }
}

impl RetryPolicy {
    /// Create a new exponential backoff policy
    pub fn exponential(config: RetryConfig) -> Self {
        RetryPolicy::Exponential(config)
    }
    
    /// Create a new fixed delay policy
    pub fn fixed(delay: Duration, max_attempts: u32) -> Self {
        RetryPolicy::Fixed { delay, max_attempts }
    }
    
    /// Create a no-retry policy
    pub fn never() -> Self {
        RetryPolicy::Never
    }
    
    /// Check if we should retry for this error
    pub fn should_retry(&self, error: &ObjectStoreError, attempt: u32) -> bool {
        match self {
            RetryPolicy::Never => false,
            RetryPolicy::Fixed { max_attempts, .. } => {
                attempt < *max_attempts && error.is_retryable()
            }
            RetryPolicy::Exponential(config) => {
                attempt < config.max_attempts && error.is_retryable()
            }
            RetryPolicy::Custom { should_retry, max_attempts, .. } => {
                attempt < *max_attempts && should_retry(error)
            }
        }
    }
    
    /// Calculate delay for next retry
    pub fn delay(&self, attempt: u32) -> Duration {
        match self {
            RetryPolicy::Never => Duration::ZERO,
            RetryPolicy::Fixed { delay, .. } => *delay,
            RetryPolicy::Exponential(config) => {
                let base_delay = Duration::from_millis(
                    (config.initial_delay.as_millis() as f32 
                        * config.backoff_multiplier.powi(attempt as i32)) as u64
                );
                base_delay.min(config.max_delay)
            }
            RetryPolicy::Custom { delay, .. } => delay(attempt),
        }
    }
}

/// Retry decorator for storage operations
pub struct RetryableOperation<T> {
    operation: T,
    policy: RetryPolicy,
}

impl<T> RetryableOperation<T> {
    pub fn new(operation: T, policy: RetryPolicy) -> Self {
        Self { operation, policy }
    }
    
    pub fn with_exponential_backoff(operation: T, config: RetryConfig) -> Self {
        Self::new(operation, RetryPolicy::Exponential(config))
    }
    
    pub fn with_fixed_retry(operation: T, delay: Duration, max_attempts: u32) -> Self {
        Self::new(operation, RetryPolicy::Fixed { delay, max_attempts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_exponential_backoff_calculation() {
        let config = RetryConfig {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
            backoff_multiplier: 2.0,
            jitter_factor: 0.0, // No jitter for predictable testing
            ..Default::default()
        };
        
        let mut backoff = ExponentialBackoff::new(config);
        
        // First retry
        let delay1 = backoff.next_delay();
        assert_eq!(delay1, Duration::from_millis(100));
        
        // Second retry
        let delay2 = backoff.next_delay();
        assert_eq!(delay2, Duration::from_millis(200));
        
        // Third retry
        let delay3 = backoff.next_delay();
        assert_eq!(delay3, Duration::from_millis(400));
    }

    #[test]
    fn test_retry_policy_should_retry() {
        let policy = RetryPolicy::Fixed {
            delay: Duration::from_millis(100),
            max_attempts: 3,
        };
        
        let retryable_error = ObjectStoreError::NetworkError {
            message: "timeout".to_string(),
        };
        
        let non_retryable_error = ObjectStoreError::NotFound {
            key: "test".to_string(),
        };
        
        assert!(policy.should_retry(&retryable_error, 1));
        assert!(policy.should_retry(&retryable_error, 2));
        assert!(!policy.should_retry(&retryable_error, 3));
        assert!(!policy.should_retry(&non_retryable_error, 1));
    }

    #[tokio::test]
    async fn test_retry_with_backoff_success() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        
        let config = RetryConfig {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            ..Default::default()
        };
        
        let result: Result<&str, ObjectStoreError> = retry_with_backoff(
            config,
            "test_operation",
            || {
                let count = counter_clone.fetch_add(1, Ordering::SeqCst);
                if count < 2 {
                    Err(ObjectStoreError::NetworkError {
                        message: "simulated failure".to_string(),
                    })
                } else {
                    Ok("success")
                }
            },
        ).await;
        
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "success");
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_failure() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        
        let config = RetryConfig {
            max_attempts: 2,
            initial_delay: Duration::from_millis(1),
            ..Default::default()
        };
        
        let result: Result<(), ObjectStoreError> = retry_with_backoff(
            config,
            "test_operation",
            || {
                counter_clone.fetch_add(1, Ordering::SeqCst);
                Err(ObjectStoreError::NetworkError {
                    message: "persistent failure".to_string(),
                })
            },
        ).await;
        
        assert!(result.is_err());
        match result.unwrap_err() {
            ObjectStoreError::RetryExhausted { attempts, .. } => {
                assert_eq!(attempts, 2); // Number of retries attempted
            }
            _ => panic!("Expected RetryExhausted error"),
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3); // Initial attempt + 2 retries
    }

    #[tokio::test]
    async fn test_retry_with_non_retryable_error() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        
        let config = RetryConfig {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            ..Default::default()
        };
        
        let result: Result<(), ObjectStoreError> = retry_with_backoff(
            config,
            "test_operation",
            || {
                counter_clone.fetch_add(1, Ordering::SeqCst);
                Err(ObjectStoreError::NotFound {
                    key: "test".to_string(),
                })
            },
        ).await;
        
        assert!(result.is_err());
        match result.unwrap_err() {
            ObjectStoreError::NotFound { .. } => {
                // Expected
            }
            _ => panic!("Expected NotFound error"),
        }
        // Should only be called once since error is not retryable
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}