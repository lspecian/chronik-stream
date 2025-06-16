//! Configuration structures for object storage backends.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

use crate::object_store::auth::{AuthConfig, S3Credentials, GcsCredentials, AzureCredentials};

/// Main configuration for object storage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectStoreConfig {
    /// Storage backend configuration
    pub backend: StorageBackend,
    
    /// Bucket/container name
    pub bucket: String,
    
    /// Optional prefix for all keys
    pub prefix: Option<String>,
    
    /// Connection configuration
    pub connection: ConnectionConfig,
    
    /// Performance tuning configuration
    pub performance: PerformanceConfig,
    
    /// Retry configuration
    pub retry: RetryConfig,
    
    /// Authentication configuration
    pub auth: AuthConfig,
    
    /// Additional metadata/tags to apply to objects
    pub default_metadata: Option<HashMap<String, String>>,
    
    /// Enable server-side encryption
    pub encryption: Option<EncryptionConfig>,
}

impl Default for ObjectStoreConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackend::Local {
                path: "/tmp/chronik-storage".to_string(),
            },
            bucket: "chronik".to_string(),
            prefix: None,
            connection: ConnectionConfig::default(),
            performance: PerformanceConfig::default(),
            retry: RetryConfig::default(),
            auth: AuthConfig::None,
            default_metadata: None,
            encryption: None,
        }
    }
}

/// Storage backend types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageBackend {
    /// AWS S3 or S3-compatible storage (MinIO, etc.)
    S3 {
        /// S3 region
        region: String,
        /// Custom endpoint for S3-compatible storage (e.g., MinIO)
        endpoint: Option<String>,
        /// Force path-style addressing (required for MinIO)
        force_path_style: bool,
        /// Use virtual hosted-style URLs
        use_virtual_hosted_style: bool,
        /// Custom signing region for cross-region access
        signing_region: Option<String>,
        /// Disable SSL/TLS (for local development)
        disable_ssl: bool,
    },
    
    /// Google Cloud Storage
    Gcs {
        /// GCP project ID
        project_id: Option<String>,
        /// Custom endpoint (for testing)
        endpoint: Option<String>,
    },
    
    /// Azure Blob Storage
    Azure {
        /// Storage account name
        account_name: String,
        /// Azure endpoint (optional, defaults to Azure global)
        endpoint: Option<String>,
        /// Use Azure emulator (Azurite)
        use_emulator: bool,
    },
    
    /// Local filesystem
    Local {
        /// Root path for storage
        path: String,
    },
}

/// Connection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Connection timeout
    pub connect_timeout: Duration,
    
    /// Request timeout
    pub request_timeout: Duration,
    
    /// TCP keep-alive interval
    pub tcp_keepalive: Option<Duration>,
    
    /// Maximum number of connections per host
    pub max_connections_per_host: usize,
    
    /// Maximum idle connections
    pub max_idle_connections: usize,
    
    /// Idle connection timeout
    pub idle_timeout: Duration,
    
    /// Connection pool timeout
    pub pool_timeout: Duration,
    
    /// Enable HTTP/2
    pub http2: bool,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            tcp_keepalive: Some(Duration::from_secs(30)),
            max_connections_per_host: 10,
            max_idle_connections: 100,
            idle_timeout: Duration::from_secs(90),
            pool_timeout: Duration::from_secs(10),
            http2: true,
        }
    }
}

/// Performance configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    /// Buffer size for streaming operations
    pub buffer_size: usize,
    
    /// Minimum part size for multipart uploads (in bytes)
    pub multipart_threshold: u64,
    
    /// Part size for multipart uploads (in bytes)
    pub multipart_part_size: u64,
    
    /// Maximum number of concurrent multipart uploads
    pub max_concurrent_uploads: usize,
    
    /// Enable compression for uploads
    pub enable_compression: bool,
    
    /// Compression level (1-9, only used if compression is enabled)
    pub compression_level: u32,
    
    /// Enable client-side caching
    pub enable_caching: bool,
    
    /// Cache size limit (in bytes)
    pub cache_size_limit: u64,
    
    /// Cache TTL
    pub cache_ttl: Duration,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            buffer_size: 64 * 1024,              // 64KB
            multipart_threshold: 16 * 1024 * 1024, // 16MB
            multipart_part_size: 16 * 1024 * 1024, // 16MB
            max_concurrent_uploads: 4,
            enable_compression: false,
            compression_level: 6,
            enable_caching: true,
            cache_size_limit: 100 * 1024 * 1024, // 100MB
            cache_ttl: Duration::from_secs(3600), // 1 hour
        }
    }
}

/// Retry configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Server-side encryption configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionConfig {
    /// Encryption type
    pub encryption_type: EncryptionType,
    
    /// KMS key ID (for KMS encryption)
    pub kms_key_id: Option<String>,
    
    /// Customer-provided encryption key (base64 encoded)
    pub customer_key: Option<String>,
}

/// Server-side encryption types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EncryptionType {
    /// AES-256 server-side encryption
    Aes256,
    /// AWS KMS encryption
    AwsKms,
    /// Customer-provided keys
    CustomerKey,
}

impl ObjectStoreConfig {
    /// Create a new S3 configuration
    pub fn s3(region: String, bucket: String) -> Self {
        Self {
            backend: StorageBackend::S3 {
                region,
                endpoint: None,
                force_path_style: false,
                use_virtual_hosted_style: true,
                signing_region: None,
                disable_ssl: false,
            },
            bucket,
            auth: AuthConfig::S3(S3Credentials::FromEnvironment),
            ..Default::default()
        }
    }
    
    /// Create a new S3-compatible configuration (e.g., MinIO)
    pub fn s3_compatible(
        endpoint: String, 
        region: String, 
        bucket: String,
        access_key: String,
        secret_key: String,
    ) -> Self {
        Self {
            backend: StorageBackend::S3 {
                region,
                endpoint: Some(endpoint),
                force_path_style: true,
                use_virtual_hosted_style: false,
                signing_region: None,
                disable_ssl: false,
            },
            bucket,
            auth: AuthConfig::S3(S3Credentials::AccessKey {
                access_key_id: access_key,
                secret_access_key: secret_key,
                session_token: None,
            }),
            ..Default::default()
        }
    }
    
    /// Create a new GCS configuration
    pub fn gcs(bucket: String, project_id: Option<String>) -> Self {
        Self {
            backend: StorageBackend::Gcs {
                project_id,
                endpoint: None,
            },
            bucket,
            auth: AuthConfig::Gcs(GcsCredentials::Default),
            ..Default::default()
        }
    }
    
    /// Create a new Azure configuration
    pub fn azure(account_name: String, bucket: String) -> Self {
        Self {
            backend: StorageBackend::Azure {
                account_name: account_name.clone(),
                endpoint: None,
                use_emulator: false,
            },
            bucket,
            auth: AuthConfig::Azure(AzureCredentials::DefaultChain),
            ..Default::default()
        }
    }
    
    /// Create a new local filesystem configuration
    pub fn local(path: String) -> Self {
        Self {
            backend: StorageBackend::Local { path },
            bucket: "local".to_string(),
            auth: AuthConfig::None,
            ..Default::default()
        }
    }
    
    /// Set prefix for all object keys
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }
    
    /// Set authentication configuration
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }
    
    /// Set performance configuration
    pub fn with_performance(mut self, performance: PerformanceConfig) -> Self {
        self.performance = performance;
        self
    }
    
    /// Set retry configuration
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }
    
    /// Enable server-side encryption
    pub fn with_encryption(mut self, encryption: EncryptionConfig) -> Self {
        self.encryption = Some(encryption);
        self
    }
    
    /// Validate the configuration
    pub fn validate(&self) -> Result<(), String> {
        // Validate bucket name
        if self.bucket.is_empty() {
            return Err("Bucket name cannot be empty".to_string());
        }
        
        // Validate backend-specific settings
        match &self.backend {
            StorageBackend::S3 { region, .. } => {
                if region.is_empty() {
                    return Err("S3 region cannot be empty".to_string());
                }
            }
            StorageBackend::Azure { account_name, .. } => {
                if account_name.is_empty() {
                    return Err("Azure account name cannot be empty".to_string());
                }
            }
            StorageBackend::Local { path } => {
                if path.is_empty() {
                    return Err("Local path cannot be empty".to_string());
                }
            }
            StorageBackend::Gcs { .. } => {
                // GCS validation is minimal as project_id is optional
            }
        }
        
        // Validate performance settings
        if self.performance.multipart_threshold < 5 * 1024 * 1024 {
            return Err("Multipart threshold must be at least 5MB".to_string());
        }
        
        if self.performance.multipart_part_size < 5 * 1024 * 1024 {
            return Err("Multipart part size must be at least 5MB".to_string());
        }
        
        // Validate retry settings
        if self.retry.max_attempts == 0 {
            return Err("Max retry attempts must be greater than 0".to_string());
        }
        
        if self.retry.backoff_multiplier < 1.0 {
            return Err("Backoff multiplier must be at least 1.0".to_string());
        }
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ObjectStoreConfig::default();
        assert_eq!(config.bucket, "chronik");
        assert!(config.prefix.is_none());
        assert!(config.retry.max_attempts > 0);
    }

    #[test]
    fn test_s3_config() {
        let config = ObjectStoreConfig::s3("us-east-1".to_string(), "test-bucket".to_string());
        
        match config.backend {
            StorageBackend::S3 { region, .. } => {
                assert_eq!(region, "us-east-1");
            }
            _ => panic!("Expected S3 backend"),
        }
        
        assert_eq!(config.bucket, "test-bucket");
    }

    #[test]
    fn test_s3_compatible_config() {
        let config = ObjectStoreConfig::s3_compatible(
            "http://localhost:9000".to_string(),
            "us-east-1".to_string(),
            "test-bucket".to_string(),
            "access_key".to_string(),
            "secret_key".to_string(),
        );
        
        match config.backend {
            StorageBackend::S3 { endpoint, force_path_style, .. } => {
                assert_eq!(endpoint, Some("http://localhost:9000".to_string()));
                assert!(force_path_style);
            }
            _ => panic!("Expected S3 backend"),
        }
    }

    #[test]
    fn test_config_validation() {
        let mut config = ObjectStoreConfig::default();
        assert!(config.validate().is_ok());
        
        // Test empty bucket
        config.bucket = "".to_string();
        assert!(config.validate().is_err());
        
        // Test invalid multipart settings
        config.bucket = "test".to_string();
        config.performance.multipart_threshold = 1024; // Too small
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_builder_pattern() {
        let config = ObjectStoreConfig::s3("us-west-2".to_string(), "my-bucket".to_string())
            .with_prefix("my-app")
            .with_performance(PerformanceConfig {
                buffer_size: 128 * 1024,
                ..Default::default()
            });
        
        assert_eq!(config.prefix, Some("my-app".to_string()));
        assert_eq!(config.performance.buffer_size, 128 * 1024);
    }
}