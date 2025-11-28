//! Broker retrieval and validation
//!
//! Extracted from `handle_metadata()` to reduce complexity.
//! Handles broker list retrieval from metadata store and validation.

use chronik_common::{Error, Result};
use chronik_common::metadata::MetadataStore;
use crate::types::MetadataBroker;
use std::sync::Arc;

/// Broker retriever
///
/// Retrieves broker list from metadata store with fallback logic.
pub struct BrokerRetriever;

impl BrokerRetriever {
    /// Get brokers from metadata store with fallback
    ///
    /// Complexity: < 15 (retrieval with filtering and fallback)
    pub async fn get_brokers(
        metadata_store: Option<Arc<dyn MetadataStore>>,
        fallback_broker_id: i32,
        fallback_host: String,
        fallback_port: i32,
    ) -> Result<Vec<MetadataBroker>> {
        if let Some(metadata_store) = metadata_store {
            // Try to get brokers from metadata store
            match metadata_store.list_brokers().await {
                Ok(broker_metas) => {
                    tracing::info!("Got {} brokers from metadata store (before filter)", broker_metas.len());
                    for b in &broker_metas {
                        tracing::info!("  BEFORE FILTER - Broker {}: {}:{} (status: {:?})",
                            b.broker_id, b.host, b.port, b.status);
                    }

                    // Filter out phantom broker (broker 0 with 0.0.0.0)
                    let brokers: Vec<MetadataBroker> = broker_metas
                        .into_iter()
                        .filter(|b| !(b.broker_id == 0 && b.host == "0.0.0.0"))
                        .map(|b| MetadataBroker {
                            node_id: b.broker_id,
                            host: b.host,
                            port: b.port,
                            rack: b.rack,
                        })
                        .collect();

                    tracing::info!("Got {} brokers from metadata store (after filter)", brokers.len());
                    for b in &brokers {
                        tracing::info!("  AFTER FILTER - Broker {}: {}:{}", b.node_id, b.host, b.port);
                    }

                    // CRITICAL FIX: If filtering removed all brokers, fall back to default
                    if brokers.is_empty() {
                        tracing::warn!("All brokers filtered out (phantom brokers), using fallback");
                        Ok(Self::create_fallback_broker(fallback_broker_id, fallback_host, fallback_port))
                    } else {
                        Ok(brokers)
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to get brokers from metadata: {:?}", e);
                    // Fallback to current broker
                    Ok(Self::create_fallback_broker(fallback_broker_id, fallback_host, fallback_port))
                }
            }
        } else {
            tracing::warn!("No metadata store available, using default broker");
            Ok(Self::create_fallback_broker(fallback_broker_id, fallback_host, fallback_port))
        }
    }

    /// Create fallback broker list
    ///
    /// Complexity: < 5 (simple broker creation)
    fn create_fallback_broker(broker_id: i32, host: String, port: i32) -> Vec<MetadataBroker> {
        tracing::warn!("Using fallback broker: {}:{} (node_id={})", host, port, broker_id);
        vec![MetadataBroker {
            node_id: broker_id,
            host,
            port,
            rack: None,
        }]
    }
}

/// Broker validator
///
/// Validates broker list to prevent client connection errors.
pub struct BrokerValidator;

impl BrokerValidator {
    /// Validate broker list
    ///
    /// Complexity: < 10 (validation checks with clear error messages)
    pub fn validate(brokers: &[MetadataBroker]) -> Result<()> {
        // CRITICAL VALIDATION: Ensure broker list is not empty
        // This prevents the AdminClient "No resolvable bootstrap urls" error
        if brokers.is_empty() {
            tracing::error!("CRITICAL: Metadata response has NO brokers - AdminClient will fail!");
            tracing::error!("This will cause 'No resolvable bootstrap urls' error in Java clients");
            return Err(Error::Internal("Metadata response must include at least one broker".into()));
        }

        // Validate each broker
        for broker in brokers {
            Self::validate_broker(broker)?;
        }

        Ok(())
    }

    /// Validate individual broker
    ///
    /// Complexity: < 10 (field validation with error messages)
    fn validate_broker(broker: &MetadataBroker) -> Result<()> {
        if broker.host.is_empty() {
            tracing::error!("CRITICAL: Broker {} has EMPTY host - AdminClient will fail!", broker.node_id);
            return Err(Error::Internal(format!("Broker {} has empty host field", broker.node_id)));
        }

        if broker.host == "0.0.0.0" {
            tracing::error!("CRITICAL: Broker {} has 0.0.0.0 host - clients cannot connect!", broker.node_id);
            return Err(Error::Internal(format!("Broker {} has invalid host 0.0.0.0", broker.node_id)));
        }

        if broker.node_id < 0 {
            tracing::error!("CRITICAL: Broker has invalid node_id {} - clients will reject!", broker.node_id);
            return Err(Error::Internal(format!("Broker has invalid node_id {}", broker.node_id)));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_fallback_broker() {
        let brokers = BrokerRetriever::create_fallback_broker(1, "localhost".to_string(), 9092);
        assert_eq!(brokers.len(), 1);
        assert_eq!(brokers[0].node_id, 1);
        assert_eq!(brokers[0].host, "localhost");
        assert_eq!(brokers[0].port, 9092);
        assert_eq!(brokers[0].rack, None);
    }

    #[test]
    fn test_validate_empty_brokers() {
        let brokers = vec![];
        let result = BrokerValidator::validate(&brokers);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least one broker"));
    }

    #[test]
    fn test_validate_broker_with_empty_host() {
        let brokers = vec![MetadataBroker {
            node_id: 1,
            host: "".to_string(),
            port: 9092,
            rack: None,
        }];
        let result = BrokerValidator::validate(&brokers);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty host"));
    }

    #[test]
    fn test_validate_broker_with_invalid_host() {
        let brokers = vec![MetadataBroker {
            node_id: 1,
            host: "0.0.0.0".to_string(),
            port: 9092,
            rack: None,
        }];
        let result = BrokerValidator::validate(&brokers);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("0.0.0.0"));
    }

    #[test]
    fn test_validate_broker_with_invalid_node_id() {
        let brokers = vec![MetadataBroker {
            node_id: -1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
        }];
        let result = BrokerValidator::validate(&brokers);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid node_id"));
    }

    #[test]
    fn test_validate_valid_broker() {
        let brokers = vec![MetadataBroker {
            node_id: 1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
        }];
        let result = BrokerValidator::validate(&brokers);
        assert!(result.is_ok());
    }
}
