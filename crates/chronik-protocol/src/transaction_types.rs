//! Transaction API types for Kafka protocol.
//!
//! Implements data structures for:
//! - InitProducerId (API 22)
//! - AddPartitionsToTxn (API 24)
//! - EndTxn (API 26)

use serde::{Deserialize, Serialize};

/// InitProducerId Request (API 22)
///
/// The InitProducerId API is used to get a producer ID and epoch for transactional
/// or idempotent producers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitProducerIdRequest {
    /// The transactional id, or null if the producer is not transactional.
    pub transactional_id: Option<String>,

    /// The time in ms to wait before aborting idle transactions sent by this producer.
    /// This is only relevant if a TransactionalId has been defined.
    pub transaction_timeout_ms: i32,

    /// The producer id. This is used to disambiguate requests if a transactional id is reused
    /// following its expiration. (v3+)
    pub producer_id: Option<i64>,

    /// The producer epoch. This will be checked against the producer epoch on the broker,
    /// and the request will return an error if they do not match. (v3+)
    pub producer_epoch: Option<i16>,
}

/// InitProducerId Response (API 22)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitProducerIdResponse {
    /// The duration in milliseconds for which the request was throttled due to a quota violation,
    /// or zero if the request did not violate any quota.
    pub throttle_time_ms: i32,

    /// The error code, or 0 if there was no error.
    pub error_code: i16,

    /// The current producer id.
    pub producer_id: i64,

    /// The current epoch associated with the producer id.
    pub producer_epoch: i16,
}

/// AddPartitionsToTxn Request (API 24)
///
/// This API is used to add partitions to an ongoing transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnRequest {
    /// The transactional id corresponding to the transaction.
    pub transactional_id: String,

    /// Current producer id in use by the transactional id.
    pub producer_id: i64,

    /// Current epoch associated with the producer id.
    pub producer_epoch: i16,

    /// The partitions to add to the transaction.
    pub topics: Vec<AddPartitionsToTxnTopic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnTopic {
    /// The name of the topic.
    pub name: String,

    /// The partition indexes to add to the transaction.
    pub partitions: Vec<i32>,
}

/// AddPartitionsToTxn Response (API 24)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnResponse {
    /// The duration in milliseconds for which the request was throttled due to a quota violation,
    /// or zero if the request did not violate any quota.
    pub throttle_time_ms: i32,

    /// The results for each topic.
    pub results: Vec<AddPartitionsToTxnTopicResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnTopicResult {
    /// The topic name.
    pub name: String,

    /// The results for each partition.
    pub results: Vec<AddPartitionsToTxnPartitionResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddPartitionsToTxnPartitionResult {
    /// The partition index.
    pub partition_index: i32,

    /// The response error code.
    pub error_code: i16,
}

/// EndTxn Request (API 26)
///
/// This API is used to commit or abort a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndTxnRequest {
    /// The transactional id corresponding to the transaction.
    pub transactional_id: String,

    /// The producer id.
    pub producer_id: i64,

    /// The producer epoch.
    pub producer_epoch: i16,

    /// True if the transaction was committed, false if it was aborted.
    pub committed: bool,
}

/// EndTxn Response (API 26)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndTxnResponse {
    /// The duration in milliseconds for which the request was throttled due to a quota violation,
    /// or zero if the request did not violate any quota.
    pub throttle_time_ms: i32,

    /// The error code, or 0 if there was no error.
    pub error_code: i16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_producer_id_structures() {
        let request = InitProducerIdRequest {
            transactional_id: Some("test-txn".to_string()),
            transaction_timeout_ms: 60000,
            producer_id: None,
            producer_epoch: None,
        };

        assert_eq!(request.transaction_timeout_ms, 60000);

        let response = InitProducerIdResponse {
            throttle_time_ms: 0,
            error_code: 0,
            producer_id: 1001,
            producer_epoch: 0,
        };

        assert_eq!(response.producer_id, 1001);
        assert_eq!(response.producer_epoch, 0);
    }

    #[test]
    fn test_add_partitions_to_txn_structures() {
        let request = AddPartitionsToTxnRequest {
            transactional_id: "test-txn".to_string(),
            producer_id: 1001,
            producer_epoch: 0,
            topics: vec![
                AddPartitionsToTxnTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![0, 1, 2],
                }
            ],
        };

        assert_eq!(request.topics.len(), 1);
        assert_eq!(request.topics[0].partitions.len(), 3);
    }

    #[test]
    fn test_end_txn_structures() {
        let commit_request = EndTxnRequest {
            transactional_id: "test-txn".to_string(),
            producer_id: 1001,
            producer_epoch: 0,
            committed: true,
        };

        assert!(commit_request.committed);

        let abort_request = EndTxnRequest {
            transactional_id: "test-txn".to_string(),
            producer_id: 1001,
            producer_epoch: 0,
            committed: false,
        };

        assert!(!abort_request.committed);
    }
}
