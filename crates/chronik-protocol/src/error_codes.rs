//! Kafka protocol error codes
//! 
//! Standard error codes from the Kafka protocol specification.
//! See: https://kafka.apache.org/protocol#protocol_error_codes

/// No error occurred
pub const NONE: i16 = 0;

/// The requested offset is out of range
pub const OFFSET_OUT_OF_RANGE: i16 = 1;

/// The message contents do not match the CRC
pub const CORRUPT_MESSAGE: i16 = 2;

/// This server does not host this topic-partition
pub const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;

/// The requested fetch size is invalid
pub const INVALID_FETCH_SIZE: i16 = 4;

/// There is no leader for this topic-partition
pub const LEADER_NOT_AVAILABLE: i16 = 5;

/// For a request which attempts to access an invalid topic
pub const INVALID_TOPIC_EXCEPTION: i16 = 17;

/// For a request which attempts to access an invalid partition
pub const INVALID_PARTITION_EXCEPTION: i16 = 37;

/// The request timed out
pub const REQUEST_TIMED_OUT: i16 = 7;

/// The broker is not available
pub const BROKER_NOT_AVAILABLE: i16 = 8;

/// The replica is not available for the requested topic-partition
pub const REPLICA_NOT_AVAILABLE: i16 = 9;

/// The coordinator is not available
pub const COORDINATOR_NOT_AVAILABLE: i16 = 15;

/// The request attempted to perform an operation on an invalid topic
pub const INVALID_TOPIC: i16 = 17;

/// The cluster membership id was not recognized
pub const UNKNOWN_MEMBER_ID: i16 = 25;

/// The generation id provided in the request is stale
pub const ILLEGAL_GENERATION: i16 = 27;

/// The rebalance timeout is invalid
pub const REBALANCE_IN_PROGRESS: i16 = 27;

/// The group is rebalancing
pub const GROUP_COORDINATOR_NOT_AVAILABLE: i16 = 16;

/// The committing offset data size is not valid
pub const OFFSET_METADATA_TOO_LARGE: i16 = 12;

/// Not authorized to access topic
pub const TOPIC_AUTHORIZATION_FAILED: i16 = 29;

/// Not authorized to access group
pub const GROUP_AUTHORIZATION_FAILED: i16 = 30;

/// Cluster authorization failed
pub const CLUSTER_AUTHORIZATION_FAILED: i16 = 31;

/// The timestamp of the message is out of acceptable range
pub const INVALID_TIMESTAMP: i16 = 32;

/// The broker does not support the requested SASL mechanism
pub const UNSUPPORTED_SASL_MECHANISM: i16 = 33;

/// Request is not valid given the current SASL state
pub const ILLEGAL_SASL_STATE: i16 = 34;

/// The version of API is not supported
pub const UNSUPPORTED_VERSION: i16 = 35;

/// Topic with this name already exists
pub const TOPIC_ALREADY_EXISTS: i16 = 36;

/// Number of partitions is invalid
pub const INVALID_PARTITIONS: i16 = 37;

/// Replication factor is invalid
pub const INVALID_REPLICATION_FACTOR: i16 = 38;

/// Replica assignment is invalid
pub const INVALID_REPLICA_ASSIGNMENT: i16 = 39;

/// Configuration is invalid
pub const INVALID_CONFIG: i16 = 40;

/// This is not the correct coordinator
pub const NOT_COORDINATOR: i16 = 41;

/// The request attempted to perform an operation on an invalid group
pub const INVALID_GROUP_ID: i16 = 42;

/// The group member's supported protocols are incompatible with those of existing members
pub const INCONSISTENT_GROUP_PROTOCOL: i16 = 43;

/// The configured groupId is invalid
pub const INVALID_GROUP_ID_EXCEPTION: i16 = 44;

/// The coordinator is loading
pub const COORDINATOR_LOAD_IN_PROGRESS: i16 = 14;

/// The coordinator is not the leader
pub const NOT_COORDINATOR_FOR_GROUP: i16 = 16;

/// Storage error on the server
pub const KAFKA_STORAGE_ERROR: i16 = 56;

/// The user-specified log directory is not found
pub const LOG_DIR_NOT_FOUND: i16 = 57;

/// SASL authentication failed
pub const SASL_AUTHENTICATION_FAILED: i16 = 58;

/// The producer's transactional id authorization failed
pub const TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 61;

/// Security features are disabled
pub const SECURITY_DISABLED: i16 = 62;

/// The broker epoch has changed
pub const BROKER_ID_NOT_REGISTERED: i16 = 68;

/// The leader epoch is older than the broker epoch
pub const STALE_BROKER_EPOCH: i16 = 77;

/// The member epoch is fenced
pub const FENCED_MEMBER_EPOCH: i16 = 78;

/// Duplicate sequence number
pub const DUPLICATE_SEQUENCE_NUMBER: i16 = 88;

/// Producer epoch is invalid
pub const INVALID_PRODUCER_EPOCH: i16 = 89;

/// Out of order sequence number
pub const OUT_OF_ORDER_SEQUENCE_NUMBER: i16 = 90;

/// Invalid transaction state
pub const INVALID_TXN_STATE: i16 = 91;