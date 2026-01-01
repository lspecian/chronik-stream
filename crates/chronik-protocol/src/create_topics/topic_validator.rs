//! Topic Validation
//!
//! Validates topic parameters including name, partition count, replication factor, and configs.
//! Supports advanced storage features: columnar storage (Arrow/Parquet) and vector search.
//! Complexity: < 20 per function

use crate::create_topics_types::error_codes;
use std::collections::HashMap;
use tracing::{warn, error, debug};

/// Validator for topic parameters
pub struct TopicValidator;

impl TopicValidator {
    /// Validate topic name format
    ///
    /// Complexity: < 10 (character validation loop)
    ///
    /// Valid topic names must:
    /// - Be non-empty
    /// - Not start with "__" (reserved for internal topics)
    /// - Contain only alphanumeric, dots, hyphens, and underscores
    /// - Not exceed 249 characters
    pub fn is_valid_topic_name(name: &str) -> bool {
        if name.is_empty() || name.len() > 249 {
            return false;
        }

        // Reserved prefix for internal topics
        if name.starts_with("__") {
            return false;
        }

        // Valid characters: alphanumeric, dots, hyphens, underscores
        name.chars().all(|c| {
            c.is_alphanumeric() || c == '.' || c == '-' || c == '_'
        })
    }

    /// Validate partition count
    ///
    /// Complexity: < 5 (range checks)
    pub fn validate_partition_count(
        topic_name: &str,
        num_partitions: i32,
    ) -> Result<(), i16> {
        if num_partitions <= 0 {
            warn!("CreateTopics: Invalid partition count {} for topic '{}'",
                num_partitions, topic_name);
            return Err(error_codes::INVALID_PARTITIONS);
        }

        if num_partitions > 10000 {
            warn!("CreateTopics: Partition count {} exceeds limit 10000 for topic '{}'",
                num_partitions, topic_name);
            return Err(error_codes::INVALID_PARTITIONS);
        }

        Ok(())
    }

    /// Validate replication factor
    ///
    /// Complexity: < 5 (simple check)
    pub fn validate_replication_factor(replication_factor: i16) -> Result<(), i16> {
        if replication_factor <= 0 {
            return Err(error_codes::INVALID_REPLICATION_FACTOR);
        }
        Ok(())
    }

    /// Validate topic configs
    ///
    /// Complexity: < 15 (config validation with known config keys)
    ///
    /// Validates topic configuration parameters against Kafka's known config keys
    pub fn validate_configs(
        configs: &HashMap<String, String>,
        replication_factor: i16,
    ) -> Result<(), i16> {
        // Validate standard Kafka configs
        Self::validate_kafka_configs(configs, replication_factor)?;

        // Validate columnar storage configs
        Self::validate_columnar_configs(configs)?;

        // Validate vector search configs
        Self::validate_vector_configs(configs)?;

        Ok(())
    }

    /// Validate standard Kafka configuration keys
    fn validate_kafka_configs(
        configs: &HashMap<String, String>,
        replication_factor: i16,
    ) -> Result<(), i16> {
        // Known valid Kafka config keys
        let kafka_keys = [
            "cleanup.policy",
            "compression.type",
            "retention.ms",
            "retention.bytes",
            "segment.bytes",
            "segment.ms",
            "max.message.bytes",
            "min.insync.replicas",
            "unclean.leader.election.enable",
            "delete.retention.ms",
            "searchable", // Full-text search (existing feature)
        ];

        for (key, value) in configs {
            // Skip advanced storage keys (validated separately)
            if key.starts_with("columnar.") || key.starts_with("vector.") {
                continue;
            }

            // Validate key is known
            if !kafka_keys.contains(&key.as_str()) {
                warn!("Unknown config key: {}", key);
                // Don't fail on unknown keys, just warn
            }

            // Validate specific configs
            match key.as_str() {
                "min.insync.replicas" => {
                    if let Ok(min_isr) = value.parse::<i16>() {
                        if min_isr > replication_factor {
                            error!("min.insync.replicas ({}) cannot exceed replication factor ({})",
                                min_isr, replication_factor);
                            return Err(error_codes::INVALID_CONFIG);
                        }
                    } else {
                        error!("Invalid min.insync.replicas value: {}", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "retention.ms" => {
                    if value.parse::<i64>().is_err() {
                        error!("Invalid retention.ms value: {}", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "searchable" => {
                    if !Self::is_valid_bool(value) {
                        error!("Invalid searchable value: {} (expected 'true' or 'false')", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                _ => {} // Other configs validated at runtime
            }
        }

        Ok(())
    }

    /// Validate columnar storage configuration keys
    ///
    /// Supported keys:
    /// - columnar.enabled: "true" | "false"
    /// - columnar.format: "parquet" | "arrow"
    /// - columnar.compression: "zstd" | "snappy" | "lz4" | "gzip" | "none"
    /// - columnar.row_group_size: positive integer
    /// - columnar.page_size: positive integer
    /// - columnar.bloom_filter: "true" | "false"
    /// - columnar.bloom_filter.columns: comma-separated column names
    /// - columnar.partitioning: "none" | "hourly" | "daily"
    /// - columnar.statistics: "none" | "chunk" | "full"
    /// - columnar.dictionary: "true" | "false"
    fn validate_columnar_configs(configs: &HashMap<String, String>) -> Result<(), i16> {
        for (key, value) in configs {
            if !key.starts_with("columnar.") {
                continue;
            }

            match key.as_str() {
                "columnar.enabled" => {
                    if !Self::is_valid_bool(value) {
                        error!("Invalid columnar.enabled value: {} (expected 'true' or 'false')", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                    debug!("Columnar storage enabled for topic");
                }
                "columnar.format" => {
                    if !["parquet", "arrow"].contains(&value.as_str()) {
                        error!("Invalid columnar.format value: {} (expected 'parquet' or 'arrow')", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "columnar.compression" => {
                    if !["zstd", "snappy", "lz4", "gzip", "none"].contains(&value.as_str()) {
                        error!("Invalid columnar.compression value: {} (expected zstd/snappy/lz4/gzip/none)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "columnar.row_group_size" => {
                    if value.parse::<usize>().map(|v| v > 0).unwrap_or(false) == false {
                        error!("Invalid columnar.row_group_size value: {} (expected positive integer)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "columnar.page_size" => {
                    if value.parse::<usize>().map(|v| v > 0).unwrap_or(false) == false {
                        error!("Invalid columnar.page_size value: {} (expected positive integer)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "columnar.bloom_filter" | "columnar.dictionary" => {
                    if !Self::is_valid_bool(value) {
                        error!("Invalid {} value: {} (expected 'true' or 'false')", key, value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "columnar.bloom_filter.columns" => {
                    // Comma-separated list of column names - just check non-empty
                    if value.trim().is_empty() {
                        error!("Invalid columnar.bloom_filter.columns value: empty");
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "columnar.partitioning" => {
                    if !["none", "hourly", "daily"].contains(&value.as_str()) {
                        error!("Invalid columnar.partitioning value: {} (expected none/hourly/daily)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "columnar.statistics" => {
                    if !["none", "chunk", "full"].contains(&value.as_str()) {
                        error!("Invalid columnar.statistics value: {} (expected none/chunk/full)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                _ => {
                    warn!("Unknown columnar config key: {}", key);
                    // Don't fail on unknown columnar keys
                }
            }
        }

        Ok(())
    }

    /// Validate vector search configuration keys
    ///
    /// Supported keys:
    /// - vector.enabled: "true" | "false"
    /// - vector.embedding.provider: "openai" | "local" | "external"
    /// - vector.embedding.model: model name string
    /// - vector.embedding.dimensions: positive integer (auto-detected from model if omitted)
    /// - vector.embedding.endpoint: URL for external provider
    /// - vector.field: "value" | "key" | JSON path (e.g., "$.message.text")
    /// - vector.index.type: "hnsw" | "flat"
    /// - vector.index.m: positive integer (HNSW M parameter, default 16)
    /// - vector.index.ef_construction: positive integer (default 200)
    /// - vector.index.ef_search: positive integer (default 50)
    /// - vector.index.metric: "cosine" | "euclidean" | "dot"
    fn validate_vector_configs(configs: &HashMap<String, String>) -> Result<(), i16> {
        let vector_enabled = configs.get("vector.enabled")
            .map(|v| v == "true")
            .unwrap_or(false);

        for (key, value) in configs {
            if !key.starts_with("vector.") {
                continue;
            }

            match key.as_str() {
                "vector.enabled" => {
                    if !Self::is_valid_bool(value) {
                        error!("Invalid vector.enabled value: {} (expected 'true' or 'false')", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                    debug!("Vector search enabled for topic");
                }
                "vector.embedding.provider" => {
                    if !["openai", "local", "external"].contains(&value.as_str()) {
                        error!("Invalid vector.embedding.provider value: {} (expected openai/local/external)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "vector.embedding.model" => {
                    // Model name validation - just check non-empty
                    if value.trim().is_empty() {
                        error!("Invalid vector.embedding.model value: empty");
                        return Err(error_codes::INVALID_CONFIG);
                    }
                    // Validate dimensions match known models
                    Self::validate_model_dimensions(configs)?;
                }
                "vector.embedding.dimensions" => {
                    let dims = value.parse::<usize>().unwrap_or(0);
                    if dims == 0 || dims > 4096 {
                        error!("Invalid vector.embedding.dimensions value: {} (expected 1-4096)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "vector.embedding.endpoint" => {
                    // URL validation - basic check
                    if !value.starts_with("http://") && !value.starts_with("https://") {
                        error!("Invalid vector.embedding.endpoint value: {} (expected http:// or https:// URL)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "vector.field" => {
                    // Field path - must be "value", "key", or start with "$."
                    if !["value", "key"].contains(&value.as_str()) && !value.starts_with("$.") {
                        error!("Invalid vector.field value: {} (expected 'value', 'key', or JSON path like '$.field')", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "vector.index.type" => {
                    if !["hnsw", "flat"].contains(&value.as_str()) {
                        error!("Invalid vector.index.type value: {} (expected hnsw/flat)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "vector.index.m" | "vector.index.ef_construction" | "vector.index.ef_search" => {
                    if value.parse::<usize>().map(|v| v > 0).unwrap_or(false) == false {
                        error!("Invalid {} value: {} (expected positive integer)", key, value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                "vector.index.metric" => {
                    if !["cosine", "euclidean", "dot"].contains(&value.as_str()) {
                        error!("Invalid vector.index.metric value: {} (expected cosine/euclidean/dot)", value);
                        return Err(error_codes::INVALID_CONFIG);
                    }
                }
                _ => {
                    warn!("Unknown vector config key: {}", key);
                    // Don't fail on unknown vector keys
                }
            }
        }

        // If vector is enabled, require provider
        if vector_enabled && !configs.contains_key("vector.embedding.provider") {
            error!("vector.enabled=true requires vector.embedding.provider");
            return Err(error_codes::INVALID_CONFIG);
        }

        Ok(())
    }

    /// Validate that embedding dimensions match known model dimensions
    fn validate_model_dimensions(configs: &HashMap<String, String>) -> Result<(), i16> {
        let model = match configs.get("vector.embedding.model") {
            Some(m) => m.as_str(),
            None => return Ok(()), // No model specified, skip validation
        };

        let explicit_dims = configs.get("vector.embedding.dimensions")
            .and_then(|v| v.parse::<usize>().ok());

        // Known model dimensions
        let expected_dims = match model {
            "text-embedding-3-small" => Some(1536),
            "text-embedding-3-large" => Some(3072),
            "text-embedding-ada-002" => Some(1536),
            "all-MiniLM-L6-v2" => Some(384),
            "all-MiniLM-L12-v2" => Some(384),
            "all-mpnet-base-v2" => Some(768),
            "bge-small-en-v1.5" => Some(384),
            "bge-base-en-v1.5" => Some(768),
            "bge-large-en-v1.5" => Some(1024),
            _ => None, // Unknown model, can't validate
        };

        if let (Some(explicit), Some(expected)) = (explicit_dims, expected_dims) {
            if explicit != expected {
                error!(
                    "vector.embedding.dimensions ({}) does not match model '{}' expected dimensions ({})",
                    explicit, model, expected
                );
                return Err(error_codes::INVALID_CONFIG);
            }
        }

        Ok(())
    }

    /// Check if a string is a valid boolean ("true" or "false")
    fn is_valid_bool(value: &str) -> bool {
        value == "true" || value == "false"
    }

    /// Get error message for error code
    ///
    /// Complexity: < 5 (simple match)
    pub fn error_message_for_code(error_code: i16) -> Option<String> {
        match error_code {
            error_codes::INVALID_TOPIC_EXCEPTION => Some("Invalid topic name".to_string()),
            error_codes::INVALID_PARTITIONS => Some("Invalid number of partitions".to_string()),
            error_codes::INVALID_REPLICATION_FACTOR => Some("Invalid replication factor".to_string()),
            error_codes::INVALID_CONFIG => Some("Invalid topic configuration".to_string()),
            error_codes::TOPIC_ALREADY_EXISTS => Some("Topic already exists".to_string()),
            error_codes::INVALID_REQUEST => Some("Invalid request".to_string()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_topic_names() {
        assert!(TopicValidator::is_valid_topic_name("test-topic"));
        assert!(TopicValidator::is_valid_topic_name("my_topic_123"));
        assert!(TopicValidator::is_valid_topic_name("topic.with.dots"));
        assert!(TopicValidator::is_valid_topic_name("a"));
    }

    #[test]
    fn test_invalid_topic_names() {
        assert!(!TopicValidator::is_valid_topic_name(""));
        assert!(!TopicValidator::is_valid_topic_name("__internal"));
        assert!(!TopicValidator::is_valid_topic_name("topic with spaces"));
        assert!(!TopicValidator::is_valid_topic_name("topic/slash"));
        assert!(!TopicValidator::is_valid_topic_name(&"a".repeat(250))); // Too long
    }

    #[test]
    fn test_validate_partition_count() {
        assert!(TopicValidator::validate_partition_count("test", 1).is_ok());
        assert!(TopicValidator::validate_partition_count("test", 100).is_ok());
        assert!(TopicValidator::validate_partition_count("test", 10000).is_ok());

        assert!(TopicValidator::validate_partition_count("test", 0).is_err());
        assert!(TopicValidator::validate_partition_count("test", -1).is_err());
        assert!(TopicValidator::validate_partition_count("test", 10001).is_err());
    }

    #[test]
    fn test_validate_replication_factor() {
        assert!(TopicValidator::validate_replication_factor(1).is_ok());
        assert!(TopicValidator::validate_replication_factor(3).is_ok());

        assert!(TopicValidator::validate_replication_factor(0).is_err());
        assert!(TopicValidator::validate_replication_factor(-1).is_err());
    }

    #[test]
    fn test_validate_configs_min_isr() {
        let mut configs = HashMap::new();
        configs.insert("min.insync.replicas".to_string(), "2".to_string());

        // Valid: min.isr <= replication_factor
        assert!(TopicValidator::validate_configs(&configs, 3).is_ok());

        // Invalid: min.isr > replication_factor
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_error_messages() {
        assert_eq!(
            TopicValidator::error_message_for_code(error_codes::INVALID_TOPIC_EXCEPTION),
            Some("Invalid topic name".to_string())
        );
        assert_eq!(
            TopicValidator::error_message_for_code(error_codes::INVALID_PARTITIONS),
            Some("Invalid number of partitions".to_string())
        );
        assert_eq!(TopicValidator::error_message_for_code(error_codes::NONE), None);
    }

    // ==================== Columnar Config Tests ====================

    #[test]
    fn test_columnar_enabled_valid() {
        let mut configs = HashMap::new();
        configs.insert("columnar.enabled".to_string(), "true".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());

        configs.insert("columnar.enabled".to_string(), "false".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_columnar_enabled_invalid() {
        let mut configs = HashMap::new();
        configs.insert("columnar.enabled".to_string(), "yes".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());

        configs.insert("columnar.enabled".to_string(), "1".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_columnar_format_valid() {
        let mut configs = HashMap::new();
        configs.insert("columnar.format".to_string(), "parquet".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());

        configs.insert("columnar.format".to_string(), "arrow".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_columnar_format_invalid() {
        let mut configs = HashMap::new();
        configs.insert("columnar.format".to_string(), "csv".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_columnar_compression_valid() {
        for codec in &["zstd", "snappy", "lz4", "gzip", "none"] {
            let mut configs = HashMap::new();
            configs.insert("columnar.compression".to_string(), codec.to_string());
            assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
        }
    }

    #[test]
    fn test_columnar_compression_invalid() {
        let mut configs = HashMap::new();
        configs.insert("columnar.compression".to_string(), "bzip2".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_columnar_row_group_size_valid() {
        let mut configs = HashMap::new();
        configs.insert("columnar.row_group_size".to_string(), "100000".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_columnar_row_group_size_invalid() {
        let mut configs = HashMap::new();
        configs.insert("columnar.row_group_size".to_string(), "0".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());

        configs.insert("columnar.row_group_size".to_string(), "-1".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());

        configs.insert("columnar.row_group_size".to_string(), "abc".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_columnar_partitioning_valid() {
        for strategy in &["none", "hourly", "daily"] {
            let mut configs = HashMap::new();
            configs.insert("columnar.partitioning".to_string(), strategy.to_string());
            assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
        }
    }

    #[test]
    fn test_columnar_full_config() {
        let mut configs = HashMap::new();
        configs.insert("columnar.enabled".to_string(), "true".to_string());
        configs.insert("columnar.format".to_string(), "parquet".to_string());
        configs.insert("columnar.compression".to_string(), "zstd".to_string());
        configs.insert("columnar.row_group_size".to_string(), "100000".to_string());
        configs.insert("columnar.bloom_filter".to_string(), "true".to_string());
        configs.insert("columnar.bloom_filter.columns".to_string(), "_offset,_timestamp".to_string());
        configs.insert("columnar.partitioning".to_string(), "daily".to_string());
        configs.insert("columnar.statistics".to_string(), "full".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    // ==================== Vector Config Tests ====================

    #[test]
    fn test_vector_enabled_valid() {
        let mut configs = HashMap::new();
        configs.insert("vector.enabled".to_string(), "true".to_string());
        configs.insert("vector.embedding.provider".to_string(), "openai".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_vector_enabled_requires_provider() {
        let mut configs = HashMap::new();
        configs.insert("vector.enabled".to_string(), "true".to_string());
        // Missing vector.embedding.provider
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_vector_provider_valid() {
        for provider in &["openai", "local", "external"] {
            let mut configs = HashMap::new();
            configs.insert("vector.enabled".to_string(), "true".to_string());
            configs.insert("vector.embedding.provider".to_string(), provider.to_string());
            assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
        }
    }

    #[test]
    fn test_vector_provider_invalid() {
        let mut configs = HashMap::new();
        configs.insert("vector.embedding.provider".to_string(), "ollama".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_vector_dimensions_valid() {
        let mut configs = HashMap::new();
        configs.insert("vector.embedding.dimensions".to_string(), "1536".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_vector_dimensions_invalid() {
        let mut configs = HashMap::new();
        configs.insert("vector.embedding.dimensions".to_string(), "0".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());

        configs.insert("vector.embedding.dimensions".to_string(), "5000".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_vector_model_dimensions_match() {
        let mut configs = HashMap::new();
        configs.insert("vector.embedding.model".to_string(), "text-embedding-3-small".to_string());
        configs.insert("vector.embedding.dimensions".to_string(), "1536".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_vector_model_dimensions_mismatch() {
        let mut configs = HashMap::new();
        configs.insert("vector.embedding.model".to_string(), "text-embedding-3-small".to_string());
        configs.insert("vector.embedding.dimensions".to_string(), "384".to_string()); // Wrong!
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_vector_field_valid() {
        for field in &["value", "key", "$.message.text", "$.data.content"] {
            let mut configs = HashMap::new();
            configs.insert("vector.field".to_string(), field.to_string());
            assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
        }
    }

    #[test]
    fn test_vector_field_invalid() {
        let mut configs = HashMap::new();
        configs.insert("vector.field".to_string(), "payload".to_string()); // Not value/key/JSON path
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_vector_index_type_valid() {
        for idx_type in &["hnsw", "flat"] {
            let mut configs = HashMap::new();
            configs.insert("vector.index.type".to_string(), idx_type.to_string());
            assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
        }
    }

    #[test]
    fn test_vector_index_metric_valid() {
        for metric in &["cosine", "euclidean", "dot"] {
            let mut configs = HashMap::new();
            configs.insert("vector.index.metric".to_string(), metric.to_string());
            assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
        }
    }

    #[test]
    fn test_vector_index_params_valid() {
        let mut configs = HashMap::new();
        configs.insert("vector.index.m".to_string(), "16".to_string());
        configs.insert("vector.index.ef_construction".to_string(), "200".to_string());
        configs.insert("vector.index.ef_search".to_string(), "50".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_vector_endpoint_valid() {
        let mut configs = HashMap::new();
        configs.insert("vector.embedding.endpoint".to_string(), "https://api.example.com/embed".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());

        configs.insert("vector.embedding.endpoint".to_string(), "http://localhost:8080/embed".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_vector_endpoint_invalid() {
        let mut configs = HashMap::new();
        configs.insert("vector.embedding.endpoint".to_string(), "ftp://example.com".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }

    #[test]
    fn test_vector_full_config() {
        let mut configs = HashMap::new();
        configs.insert("vector.enabled".to_string(), "true".to_string());
        configs.insert("vector.embedding.provider".to_string(), "openai".to_string());
        configs.insert("vector.embedding.model".to_string(), "text-embedding-3-small".to_string());
        configs.insert("vector.embedding.dimensions".to_string(), "1536".to_string());
        configs.insert("vector.field".to_string(), "value".to_string());
        configs.insert("vector.index.type".to_string(), "hnsw".to_string());
        configs.insert("vector.index.m".to_string(), "16".to_string());
        configs.insert("vector.index.ef_construction".to_string(), "200".to_string());
        configs.insert("vector.index.ef_search".to_string(), "50".to_string());
        configs.insert("vector.index.metric".to_string(), "cosine".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    // ==================== Combined Feature Tests ====================

    #[test]
    fn test_both_features_enabled() {
        let mut configs = HashMap::new();
        // Columnar config
        configs.insert("columnar.enabled".to_string(), "true".to_string());
        configs.insert("columnar.format".to_string(), "parquet".to_string());
        configs.insert("columnar.compression".to_string(), "zstd".to_string());
        // Vector config
        configs.insert("vector.enabled".to_string(), "true".to_string());
        configs.insert("vector.embedding.provider".to_string(), "openai".to_string());
        configs.insert("vector.embedding.model".to_string(), "text-embedding-3-small".to_string());
        configs.insert("vector.field".to_string(), "value".to_string());
        // Full-text search
        configs.insert("searchable".to_string(), "true".to_string());
        // Standard Kafka config
        configs.insert("retention.ms".to_string(), "604800000".to_string());

        assert!(TopicValidator::validate_configs(&configs, 3).is_ok());
    }

    #[test]
    fn test_searchable_valid() {
        let mut configs = HashMap::new();
        configs.insert("searchable".to_string(), "true".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());

        configs.insert("searchable".to_string(), "false".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_ok());
    }

    #[test]
    fn test_searchable_invalid() {
        let mut configs = HashMap::new();
        configs.insert("searchable".to_string(), "yes".to_string());
        assert!(TopicValidator::validate_configs(&configs, 1).is_err());
    }
}
