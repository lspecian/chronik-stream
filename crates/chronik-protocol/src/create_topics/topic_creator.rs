//! Topic Creation and Response Building
//!
//! Handles creating topics in metadata store and building response for each topic.
//! Complexity: < 25 per function

use crate::create_topics_types::{CreateTopicRequest, CreateTopicResponse, error_codes};
use crate::handler::ProtocolHandler;
use super::topic_validator::TopicValidator;
use chronik_common::Error;
use tracing::{info, error};

/// Creator for topics with response building
pub struct TopicCreator;

impl TopicCreator {
    /// Process a single topic request and build response
    ///
    /// Complexity: < 25 (orchestrates validation, creation, and response building)
    pub async fn process_topic(
        handler: &ProtocolHandler,
        topic: CreateTopicRequest,
        validate_only: bool,
        api_version: i16,
    ) -> CreateTopicResponse {
        // Phase 1: Validate topic parameters
        let error_code = match Self::validate_topic(&topic) {
            Ok(_) => {
                // Phase 2: Validate replication factor against available brokers
                match handler.validate_replication_factor(&topic).await {
                    Ok(_) => {
                        // Phase 3: Create topic or validate only
                        if validate_only {
                            error_codes::NONE
                        } else {
                            Self::create_topic_in_metadata(handler, &topic).await
                        }
                    }
                    Err(e) => {
                        error!("Replication factor validation failed: {}", e);
                        error_codes::INVALID_REPLICATION_FACTOR
                    }
                }
            }
            Err(error_code) => error_code,
        };

        // Build response
        Self::build_topic_response(topic, error_code, api_version)
    }

    /// Validate all topic parameters
    ///
    /// Complexity: < 15 (delegates to validator functions)
    fn validate_topic(topic: &CreateTopicRequest) -> Result<(), i16> {
        // Validate topic name
        if !TopicValidator::is_valid_topic_name(&topic.name) {
            return Err(error_codes::INVALID_TOPIC_EXCEPTION);
        }

        // Validate partition count
        TopicValidator::validate_partition_count(&topic.name, topic.num_partitions)?;

        // Validate replication factor
        TopicValidator::validate_replication_factor(topic.replication_factor)?;

        // Validate configs
        TopicValidator::validate_configs(&topic.configs, topic.replication_factor)?;

        Ok(())
    }

    /// Create topic in metadata store
    ///
    /// Complexity: < 15 (metadata store call with error handling)
    async fn create_topic_in_metadata(
        handler: &ProtocolHandler,
        topic: &CreateTopicRequest,
    ) -> i16 {
        match handler.create_topic_in_metadata(topic).await {
            Ok(_) => {
                info!("CreateTopics: Created topic '{}' with {} partitions, replication factor {}",
                    topic.name, topic.num_partitions, topic.replication_factor);
                error_codes::NONE
            }
            Err(e) => {
                error!("Failed to create topic '{}': {:?}", topic.name, e);
                match e {
                    Error::Internal(msg) if msg.contains("already exists") => {
                        error_codes::TOPIC_ALREADY_EXISTS
                    }
                    _ => error_codes::INVALID_REQUEST,
                }
            }
        }
    }

    /// Build topic response with version-specific fields
    ///
    /// Complexity: < 10 (response struct creation)
    fn build_topic_response(
        topic: CreateTopicRequest,
        error_code: i16,
        api_version: i16,
    ) -> CreateTopicResponse {
        let error_message = TopicValidator::error_message_for_code(error_code);

        CreateTopicResponse {
            name: topic.name,
            error_code,
            error_message,
            // Version 5+ returns detailed topic info
            num_partitions: if api_version >= 5 { topic.num_partitions } else { -1 },
            replication_factor: if api_version >= 5 { topic.replication_factor } else { -1 },
            configs: Vec::new(), // TODO: Return actual configs
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_validate_topic_valid() {
        let topic = CreateTopicRequest {
            name: "test-topic".to_string(),
            num_partitions: 3,
            replication_factor: 1,
            replica_assignments: Vec::new(),
            configs: HashMap::new(),
        };

        assert!(TopicCreator::validate_topic(&topic).is_ok());
    }

    #[test]
    fn test_validate_topic_invalid_name() {
        let topic = CreateTopicRequest {
            name: "__internal".to_string(),
            num_partitions: 3,
            replication_factor: 1,
            replica_assignments: Vec::new(),
            configs: HashMap::new(),
        };

        let result = TopicCreator::validate_topic(&topic);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), error_codes::INVALID_TOPIC_EXCEPTION);
    }

    #[test]
    fn test_validate_topic_invalid_partitions() {
        let topic = CreateTopicRequest {
            name: "test-topic".to_string(),
            num_partitions: 0,
            replication_factor: 1,
            replica_assignments: Vec::new(),
            configs: HashMap::new(),
        };

        let result = TopicCreator::validate_topic(&topic);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), error_codes::INVALID_PARTITIONS);
    }

    #[test]
    fn test_build_topic_response_v0() {
        let topic = CreateTopicRequest {
            name: "test-topic".to_string(),
            num_partitions: 3,
            replication_factor: 1,
            replica_assignments: Vec::new(),
            configs: HashMap::new(),
        };

        let response = TopicCreator::build_topic_response(topic, error_codes::NONE, 0);

        assert_eq!(response.name, "test-topic");
        assert_eq!(response.error_code, error_codes::NONE);
        assert_eq!(response.num_partitions, -1); // Not returned in v0
        assert_eq!(response.replication_factor, -1); // Not returned in v0
    }

    #[test]
    fn test_build_topic_response_v5() {
        let topic = CreateTopicRequest {
            name: "test-topic".to_string(),
            num_partitions: 3,
            replication_factor: 1,
            replica_assignments: Vec::new(),
            configs: HashMap::new(),
        };

        let response = TopicCreator::build_topic_response(topic, error_codes::NONE, 5);

        assert_eq!(response.name, "test-topic");
        assert_eq!(response.error_code, error_codes::NONE);
        assert_eq!(response.num_partitions, 3); // Returned in v5+
        assert_eq!(response.replication_factor, 1); // Returned in v5+
    }
}
