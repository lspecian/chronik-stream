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
    /// Topic configurations as the topic actually has them.
    ///
    /// Each known key reports the topic's value; one it overrides is marked
    /// TOPIC_CONFIG (not default), with the default kept as a synonym. Keys the
    /// topic sets that are not in the known list are reported too. (This used
    /// to return the defaults whatever the topic held, so a config changed with
    /// `kafka-configs --alter` never showed up in `--describe`.)
    pub async fn get_topic_configs(
        topic: &chronik_common::metadata::TopicMetadata,
        configuration_keys: &Option<Vec<String>>,
        include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> Result<Vec<ConfigEntry>> {
        let known = [
            ("retention.ms", "604800000", "The minimum age of a log file to be eligible for deletion", config_type::LONG),
            ("segment.ms", "604800000", "The time after which Kafka will force the log to roll", config_type::LONG),
            ("segment.bytes", "1073741824", "The segment file size for the log", config_type::LONG),
            ("min.insync.replicas", "1", "Minimum number of replicas that must acknowledge a write", config_type::INT),
            ("compression.type", "producer", "The compression type for a topic", config_type::STRING),
            ("cleanup.policy", "delete", "The retention policy to use on log segments", config_type::STRING),
            ("max.message.bytes", "1048588", "The maximum size of a message", config_type::INT),
        ];
        // The topic's own values: the typed fields, then the free-form map.
        let mut overrides: std::collections::BTreeMap<String, String> = topic
            .config
            .config
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        overrides.insert(
            "retention.ms".into(),
            topic.config.retention_ms.map(|ms| ms.to_string()).unwrap_or_else(|| "-1".into()),
        );
        overrides.insert("segment.bytes".into(), topic.config.segment_bytes.to_string());

        let mut configs = Vec::new();
        for (name, default_value, doc, config_type_val) in known {
            if !Self::should_include_config(name, configuration_keys) {
                continue;
            }
            let value = overrides.remove(name);
            configs.push(Self::build_topic_config_entry(
                name,
                Some(default_value),
                value.as_deref().filter(|v| *v != default_value),
                doc,
                config_type_val,
                include_synonyms,
                include_documentation,
                api_version,
            ));
        }
        for (name, value) in overrides {
            if Self::should_include_config(&name, configuration_keys) {
                // No known default: report the topic value alone, never the
                // value itself dressed up as the default.
                configs.push(Self::build_topic_config_entry(
                    &name,
                    None,
                    Some(&value),
                    "",
                    config_type::STRING,
                    include_synonyms,
                    false,
                    api_version,
                ));
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
        default_value: Option<&str>,
        topic_value: Option<&str>,
        doc: &str,
        config_type_val: i8,
        include_synonyms: bool,
        include_documentation: bool,
        api_version: i16,
    ) -> ConfigEntry {
        let mut synonyms = Vec::new();
        if include_synonyms {
            if let Some(v) = topic_value {
                synonyms.push(ConfigSynonym {
                    name: name.to_string(),
                    value: Some(v.to_string()),
                    source: config_source::TOPIC_CONFIG,
                });
            }
            if let Some(default_value) = default_value {
                synonyms.push(ConfigSynonym {
                    name: name.to_string(),
                    value: Some(default_value.to_string()),
                    source: config_source::DEFAULT_CONFIG,
                });
            }
        }
        ConfigEntry {
            name: name.to_string(),
            value: topic_value.or(default_value).map(str::to_string),
            read_only: false,
            is_default: topic_value.is_none(),
            config_source: if topic_value.is_some() { config_source::TOPIC_CONFIG } else { config_source::DEFAULT_CONFIG },
            is_sensitive: false,
            synonyms,
            config_type: if api_version >= 3 { Some(config_type_val) } else { None },
            documentation: if include_documentation && api_version >= 3 && !doc.is_empty() {
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
    use chronik_common::metadata::{TopicConfig, TopicMetadata};

    #[test]
    fn test_should_include_config_no_filter() {
        // No filter = include all
        assert!(ConfigProvider::should_include_config("retention.ms", &None));
    }

    #[test]
    fn test_should_include_config_with_filter_match() {
        let keys = Some(vec!["retention.ms".to_string(), "segment.bytes".to_string()]);
        assert!(ConfigProvider::should_include_config("retention.ms", &keys));
    }

    #[test]
    fn test_should_include_config_with_filter_no_match() {
        let keys = Some(vec!["retention.ms".to_string()]);
        assert!(!ConfigProvider::should_include_config("segment.bytes", &keys));
    }

    #[tokio::test]
    async fn test_get_topic_configs_no_filter() {
        let configs = ConfigProvider::get_topic_configs(&topic(TopicConfig::default()), &None, false, false, 0)
            .await
            .unwrap();
        // The 7 known keys, all at their defaults.
        assert_eq!(configs.len(), 7);
        assert!(configs.iter().all(|c| c.is_default), "{:?}", configs);
    }

    fn topic(config: TopicConfig) -> TopicMetadata {
        TopicMetadata {
            id: uuid::Uuid::nil(),
            name: "t".into(),
            config,
            created_at: chronik_common::Utc::now(),
            updated_at: chronik_common::Utc::now(),
            auto_created: false,
        }
    }

    #[tokio::test]
    async fn test_get_topic_configs_with_filter() {
        let keys = Some(vec!["retention.ms".to_string(), "segment.bytes".to_string()]);
        let configs = ConfigProvider::get_topic_configs(&topic(TopicConfig::default()), &keys, false, false, 0)
            .await
            .unwrap();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "retention.ms");
        assert_eq!(configs[1].name, "segment.bytes");
    }

    /// What the topic holds is what DescribeConfigs reports.
    #[tokio::test]
    async fn topic_overrides_are_reported_as_topic_config() {
        let mut config = TopicConfig::default();
        config.retention_ms = Some(777_777);
        config.config.insert("cleanup.policy".into(), "compact".into());
        config.config.insert("columnar.enabled".into(), "true".into());
        let configs = ConfigProvider::get_topic_configs(&topic(config), &None, true, false, 3).await.unwrap();
        let get = |n: &str| configs.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("{} missing", n));
        let r = get("retention.ms");
        assert_eq!((r.value.as_deref(), r.is_default, r.config_source), (Some("777777"), false, config_source::TOPIC_CONFIG));
        assert_eq!(r.synonyms.len(), 2);
        assert_eq!(get("cleanup.policy").value.as_deref(), Some("compact"));
        assert!(!get("cleanup.policy").is_default);
        assert_eq!(get("columnar.enabled").value.as_deref(), Some("true"));
        assert!(get("segment.bytes").is_default);
        // A key with no known default carries only its topic value: its own
        // value used to be reported as the default too ("DEFAULT_CONFIG:
        // searchable=true" for a key whose default is false).
        let c = get("columnar.enabled");
        assert_eq!(c.synonyms.len(), 1, "{:?}", c.synonyms);
        assert_eq!(c.synonyms[0].source, config_source::TOPIC_CONFIG);
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
