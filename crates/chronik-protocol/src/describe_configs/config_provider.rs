//! Configuration Provider for DescribeConfigs
//!
//! Provides topic and broker configurations with support for filtering,
//! synonyms, and documentation.
//! Complexity: < 25 per function

use crate::types::{ConfigEntry, ConfigSynonym, config_type, config_source};
use chronik_common::Result;

/// Provider for topic and broker configurations
pub struct ConfigProvider;

impl ConfigProvider {
    /// Get topic configurations
    ///
    /// Complexity: < 20 (config loop with filtering and synonym building)
    pub async fn get_topic_configs(
        _topic_name: &str,
        configuration_keys: &Option<Vec<String>>,
        include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> Result<Vec<ConfigEntry>> {
        let mut configs = Vec::new();

        // Default topic configurations
        let all_configs = vec![
            ("retention.ms", "604800000", "The minimum age of a log file to be eligible for deletion", config_type::LONG),
            ("segment.ms", "604800000", "The time after which Kafka will force the log to roll", config_type::LONG),
            ("segment.bytes", "1073741824", "The segment file size for the log", config_type::LONG),
            ("min.insync.replicas", "1", "Minimum number of replicas that must acknowledge a write", config_type::INT),
            ("compression.type", "producer", "The compression type for a topic", config_type::STRING),
            ("cleanup.policy", "delete", "The retention policy to use on log segments", config_type::STRING),
            ("max.message.bytes", "1048588", "The maximum size of a message", config_type::INT),
        ];

        for (name, default_value, doc, config_type_val) in all_configs {
            // Filter by requested keys
            if Self::should_include_config(name, configuration_keys) {
                let config = Self::build_topic_config_entry(
                    name,
                    default_value,
                    doc,
                    config_type_val,
                    include_synonyms,
                    include_documentation,
                    api_version,
                );
                configs.push(config);
            }
        }

        Ok(configs)
    }

    /// Get broker configurations
    ///
    /// Complexity: < 20 (config loop with filtering and synonym building)
    pub async fn get_broker_configs(
        _broker_id: &str,
        configuration_keys: &Option<Vec<String>>,
        include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> Result<Vec<ConfigEntry>> {
        let mut configs = Vec::new();

        // Default broker configurations
        let all_configs = vec![
            ("default.replication.factor", "1", "Default replication factor for automatically created topics", config_type::INT),
            ("log.retention.hours", "168", "The number of hours to keep a log file", config_type::INT),
            ("log.segment.bytes", "1073741824", "The maximum size of a single log file", config_type::LONG),
            ("num.network.threads", "8", "The number of threads for network requests", config_type::INT),
            ("num.io.threads", "8", "The number of threads for I/O", config_type::INT),
            ("socket.send.buffer.bytes", "102400", "The SO_SNDBUF buffer size", config_type::INT),
            ("socket.receive.buffer.bytes", "102400", "The SO_RCVBUF buffer size", config_type::INT),
        ];

        for (name, default_value, doc, config_type_val) in all_configs {
            // Filter by requested keys
            if Self::should_include_config(name, configuration_keys) {
                let config = Self::build_broker_config_entry(
                    name,
                    default_value,
                    doc,
                    config_type_val,
                    include_synonyms,
                    include_documentation,
                    api_version,
                );
                configs.push(config);
            }
        }

        Ok(configs)
    }

    /// Check if config should be included based on requested keys
    ///
    /// Complexity: < 5
    fn should_include_config(name: &str, configuration_keys: &Option<Vec<String>>) -> bool {
        match configuration_keys {
            Some(keys) => keys.contains(&name.to_string()),
            None => true, // Include all if no filter specified
        }
    }

    /// Build ConfigEntry for topic
    ///
    /// Complexity: < 15
    fn build_topic_config_entry(
        name: &str,
        default_value: &str,
        doc: &str,
        config_type_val: i8,
        include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> ConfigEntry {
        let mut synonyms = Vec::new();
        if include_synonyms {
            synonyms.push(ConfigSynonym {
                name: name.to_string(),
                value: Some(default_value.to_string()),
                source: config_source::DEFAULT_CONFIG,
            });
        }

        ConfigEntry {
            name: name.to_string(),
            value: Some(default_value.to_string()),
            read_only: false,
            is_default: true,
            config_source: config_source::DEFAULT_CONFIG,
            is_sensitive: false,
            synonyms,
            config_type: if api_version >= 3 { Some(config_type_val) } else { None },
            documentation: if include_documentation && api_version >= 3 {
                Some(doc.to_string())
            } else {
                None
            },
        }
    }

    /// Build ConfigEntry for broker
    ///
    /// Complexity: < 15
    fn build_broker_config_entry(
        name: &str,
        default_value: &str,
        doc: &str,
        config_type_val: i8,
        include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> ConfigEntry {
        let mut synonyms = Vec::new();
        if include_synonyms {
            synonyms.push(ConfigSynonym {
                name: name.to_string(),
                value: Some(default_value.to_string()),
                source: config_source::STATIC_BROKER_CONFIG,
            });
        }

        ConfigEntry {
            name: name.to_string(),
            value: Some(default_value.to_string()),
            read_only: true,
            is_default: true,
            config_source: config_source::STATIC_BROKER_CONFIG,
            is_sensitive: false,
            synonyms,
            config_type: if api_version >= 3 { Some(config_type_val) } else { None },
            documentation: if include_documentation && api_version >= 3 {
                Some(doc.to_string())
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_include_config_no_filter() {
        // No filter = include all
        assert_eq!(ConfigProvider::should_include_config("retention.ms", &None), true);
    }

    #[test]
    fn test_should_include_config_with_filter_match() {
        let keys = Some(vec!["retention.ms".to_string(), "segment.bytes".to_string()]);
        assert_eq!(ConfigProvider::should_include_config("retention.ms", &keys), true);
    }

    #[test]
    fn test_should_include_config_with_filter_no_match() {
        let keys = Some(vec!["retention.ms".to_string()]);
        assert_eq!(ConfigProvider::should_include_config("segment.bytes", &keys), false);
    }

    #[tokio::test]
    async fn test_get_topic_configs_no_filter() {
        let configs = ConfigProvider::get_topic_configs(
            "test-topic",
            &None,
            false,
            false,
            0,
        ).await.unwrap();

        // Should return all 7 default topic configs
        assert_eq!(configs.len(), 7);
    }

    #[tokio::test]
    async fn test_get_topic_configs_with_filter() {
        let keys = Some(vec!["retention.ms".to_string(), "segment.bytes".to_string()]);
        let configs = ConfigProvider::get_topic_configs(
            "test-topic",
            &keys,
            false,
            false,
            0,
        ).await.unwrap();

        // Should return only 2 filtered configs
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "retention.ms");
        assert_eq!(configs[1].name, "segment.bytes");
    }

    #[tokio::test]
    async fn test_get_broker_configs_no_filter() {
        let configs = ConfigProvider::get_broker_configs(
            "0",
            &None,
            false,
            false,
            0,
        ).await.unwrap();

        // Should return all 7 default broker configs
        assert_eq!(configs.len(), 7);
    }
}
