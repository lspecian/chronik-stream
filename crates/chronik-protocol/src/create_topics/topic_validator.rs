//! Topic Validation
//!
//! Validates topic parameters including name, partition count, replication factor, and configs.
//! Complexity: < 20 per function

use crate::create_topics_types::error_codes;
use std::collections::HashMap;
use tracing::{warn, error};

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
        // Known valid config keys (subset for validation)
        let valid_keys = [
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
        ];

        for (key, value) in configs {
            // Validate key is known
            if !valid_keys.contains(&key.as_str()) {
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
                _ => {} // Other configs validated at runtime
            }
        }

        Ok(())
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
}
