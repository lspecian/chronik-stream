//! Integration tests for Chronik Stream.

mod testcontainers_setup;
mod kafka_protocol_test;
mod storage_test;
mod end_to_end_test;
mod kafka_compatibility_test;
mod search_integration_test;
mod failure_recovery_test;
mod performance_test;
mod multi_language_client_test;