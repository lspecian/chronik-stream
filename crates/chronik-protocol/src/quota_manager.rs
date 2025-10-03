// Client quota management for Kafka protocol

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use chronik_common::Result;
use serde::{Deserialize, Serialize};

use crate::describe_client_quotas_types::{
    DescribeClientQuotasRequest, DescribeClientQuotasResponse,
    QuotaEntry, EntityData, QuotaValue, ComponentFilter,
};
use crate::alter_client_quotas_types::{
    AlterClientQuotasRequest, AlterClientQuotasResponse,
    AlterQuotaEntryResponse,
};

/// Entity types for quotas
pub mod entity_types {
    pub const USER: &str = "user";
    pub const CLIENT_ID: &str = "client-id";
    pub const IP: &str = "ip";
}

/// Quota configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaConfig {
    pub producer_byte_rate: Option<f64>,
    pub consumer_byte_rate: Option<f64>,
    pub request_percentage: Option<f64>,
    pub controller_mutation_rate: Option<f64>,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            producer_byte_rate: None,
            consumer_byte_rate: None,
            request_percentage: None,
            controller_mutation_rate: None,
        }
    }
}

/// Quota entity key
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct QuotaEntityKey {
    pub user: Option<String>,
    pub client_id: Option<String>,
    pub ip: Option<String>,
}

/// Quota manager
pub struct QuotaManager {
    quotas: Arc<RwLock<HashMap<QuotaEntityKey, QuotaConfig>>>,
    default_quotas: Arc<RwLock<QuotaConfig>>,
}

impl QuotaManager {
    pub fn new() -> Self {
        Self {
            quotas: Arc::new(RwLock::new(HashMap::new())),
            default_quotas: Arc::new(RwLock::new(QuotaConfig::default())),
        }
    }

    /// Describe quotas based on filters
    pub async fn describe_quotas(&self, request: DescribeClientQuotasRequest) -> Result<DescribeClientQuotasResponse> {
        let quotas = self.quotas.read().await;
        let mut entries = Vec::new();

        // Check if we should include defaults
        let include_defaults = request.components.iter().any(|c| c.match_type == 1);
        let match_any = request.components.iter().any(|c| c.match_type == 2);

        if include_defaults || match_any {
            // Include default quotas
            let default_quotas = self.default_quotas.read().await;
            let mut values = Vec::new();

            if let Some(rate) = default_quotas.producer_byte_rate {
                values.push(QuotaValue {
                    key: "producer_byte_rate".to_string(),
                    value: rate,
                });
            }
            if let Some(rate) = default_quotas.consumer_byte_rate {
                values.push(QuotaValue {
                    key: "consumer_byte_rate".to_string(),
                    value: rate,
                });
            }
            if let Some(pct) = default_quotas.request_percentage {
                values.push(QuotaValue {
                    key: "request_percentage".to_string(),
                    value: pct,
                });
            }
            if let Some(rate) = default_quotas.controller_mutation_rate {
                values.push(QuotaValue {
                    key: "controller_mutation_rate".to_string(),
                    value: rate,
                });
            }

            if !values.is_empty() {
                entries.push(QuotaEntry {
                    entity: vec![],  // Empty entity for defaults
                    values,
                });
            }
        }

        // Filter quotas based on components
        for (key, config) in quotas.iter() {
            if self.matches_filters(&request.components, key, request.strict) {
                let mut entity = Vec::new();

                if let Some(ref user) = key.user {
                    entity.push(EntityData {
                        entity_type: entity_types::USER.to_string(),
                        entity_name: Some(user.clone()),
                    });
                }
                if let Some(ref client_id) = key.client_id {
                    entity.push(EntityData {
                        entity_type: entity_types::CLIENT_ID.to_string(),
                        entity_name: Some(client_id.clone()),
                    });
                }
                if let Some(ref ip) = key.ip {
                    entity.push(EntityData {
                        entity_type: entity_types::IP.to_string(),
                        entity_name: Some(ip.clone()),
                    });
                }

                let mut values = Vec::new();
                if let Some(rate) = config.producer_byte_rate {
                    values.push(QuotaValue {
                        key: "producer_byte_rate".to_string(),
                        value: rate,
                    });
                }
                if let Some(rate) = config.consumer_byte_rate {
                    values.push(QuotaValue {
                        key: "consumer_byte_rate".to_string(),
                        value: rate,
                    });
                }
                if let Some(pct) = config.request_percentage {
                    values.push(QuotaValue {
                        key: "request_percentage".to_string(),
                        value: pct,
                    });
                }
                if let Some(rate) = config.controller_mutation_rate {
                    values.push(QuotaValue {
                        key: "controller_mutation_rate".to_string(),
                        value: rate,
                    });
                }

                if !values.is_empty() {
                    entries.push(QuotaEntry { entity, values });
                }
            }
        }

        Ok(DescribeClientQuotasResponse {
            throttle_time_ms: 0,
            error_code: 0,
            error_message: None,
            entries,
        })
    }

    /// Alter quotas
    pub async fn alter_quotas(&self, request: AlterClientQuotasRequest) -> Result<AlterClientQuotasResponse> {
        if request.validate_only {
            // Just validate without making changes
            let mut response_entries = Vec::new();

            for entry in request.entries {
                // Validate entity
                let error_code = if self.validate_entity(&entry.entity[..]) { 0 } else { 42 };

                response_entries.push(AlterQuotaEntryResponse {
                    error_code,
                    error_message: if error_code != 0 { Some("Invalid entity".to_string()) } else { None },
                    entity: entry.entity,
                });
            }

            return Ok(AlterClientQuotasResponse {
                throttle_time_ms: 0,
                entries: response_entries,
            });
        }

        let mut quotas = self.quotas.write().await;
        let mut response_entries = Vec::new();

        for entry in request.entries {
            let key = self.entity_to_key(&entry.entity[..]);

            // Get or create quota config
            let config = quotas.entry(key).or_insert_with(QuotaConfig::default);

            // Apply operations
            for op in entry.ops {
                match op.key.as_str() {
                    "producer_byte_rate" => {
                        config.producer_byte_rate = op.value;
                    }
                    "consumer_byte_rate" => {
                        config.consumer_byte_rate = op.value;
                    }
                    "request_percentage" => {
                        config.request_percentage = op.value;
                    }
                    "controller_mutation_rate" => {
                        config.controller_mutation_rate = op.value;
                    }
                    _ => {
                        // Unknown quota key
                        response_entries.push(AlterQuotaEntryResponse {
                            error_code: 42,
                            error_message: Some(format!("Unknown quota key: {}", op.key)),
                            entity: entry.entity.clone(),
                        });
                        continue;
                    }
                }
            }

            response_entries.push(AlterQuotaEntryResponse {
                error_code: 0,
                error_message: None,
                entity: entry.entity,
            });
        }

        Ok(AlterClientQuotasResponse {
            throttle_time_ms: 0,
            entries: response_entries,
        })
    }

    /// Set default quotas
    pub async fn set_default_quota(&self, key: &str, value: Option<f64>) -> Result<()> {
        let mut defaults = self.default_quotas.write().await;

        match key {
            "producer_byte_rate" => defaults.producer_byte_rate = value,
            "consumer_byte_rate" => defaults.consumer_byte_rate = value,
            "request_percentage" => defaults.request_percentage = value,
            "controller_mutation_rate" => defaults.controller_mutation_rate = value,
            _ => return Err(chronik_common::Error::Protocol(format!("Unknown quota key: {}", key))),
        }

        Ok(())
    }

    /// Check if a quota key matches filters
    fn matches_filters(&self, filters: &[ComponentFilter], key: &QuotaEntityKey, strict: bool) -> bool {
        for filter in filters {
            let matches = match filter.entity_type.as_str() {
                entity_types::USER => {
                    match filter.match_type {
                        0 => key.user.as_deref() == filter.match_value.as_deref(), // EXACT
                        1 => key.user.is_none(), // DEFAULT
                        2 => true, // ANY
                        _ => false,
                    }
                }
                entity_types::CLIENT_ID => {
                    match filter.match_type {
                        0 => key.client_id.as_deref() == filter.match_value.as_deref(), // EXACT
                        1 => key.client_id.is_none(), // DEFAULT
                        2 => true, // ANY
                        _ => false,
                    }
                }
                entity_types::IP => {
                    match filter.match_type {
                        0 => key.ip.as_deref() == filter.match_value.as_deref(), // EXACT
                        1 => key.ip.is_none(), // DEFAULT
                        2 => true, // ANY
                        _ => false,
                    }
                }
                _ => false,
            };

            if strict && !matches {
                return false;
            } else if !strict && matches {
                return true;
            }
        }

        strict // If strict, all must match (true if we got here); if not strict, none matched (false)
    }

    /// Convert entity data to quota key
    fn entity_to_key(&self, entity: &[EntityData]) -> QuotaEntityKey {
        let mut key = QuotaEntityKey {
            user: None,
            client_id: None,
            ip: None,
        };

        for data in entity {
            match data.entity_type.as_str() {
                entity_types::USER => key.user = data.entity_name.clone(),
                entity_types::CLIENT_ID => key.client_id = data.entity_name.clone(),
                entity_types::IP => key.ip = data.entity_name.clone(),
                _ => {}
            }
        }

        key
    }

    /// Validate entity
    fn validate_entity(&self, entity: &[EntityData]) -> bool {
        for data in entity {
            match data.entity_type.as_str() {
                entity_types::USER | entity_types::CLIENT_ID | entity_types::IP => {}
                _ => return false, // Unknown entity type
            }
        }
        true
    }

    /// Get quota for a specific client
    pub async fn get_quota(&self, user: Option<&str>, client_id: Option<&str>, ip: Option<&str>) -> QuotaConfig {
        let key = QuotaEntityKey {
            user: user.map(String::from),
            client_id: client_id.map(String::from),
            ip: ip.map(String::from),
        };

        let quotas = self.quotas.read().await;
        if let Some(config) = quotas.get(&key) {
            return config.clone();
        }

        // Return defaults
        self.default_quotas.read().await.clone()
    }
}