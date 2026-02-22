//! Default functions for serde defaults in CRD specs.

pub fn image() -> String {
    crate::constants::defaults::IMAGE.to_string()
}

pub fn image_pull_policy() -> String {
    "IfNotPresent".to_string()
}

pub fn storage_class() -> String {
    "standard".to_string()
}

pub fn storage_size() -> String {
    crate::constants::defaults::STORAGE_SIZE.to_string()
}

pub fn access_modes() -> Vec<String> {
    vec!["ReadWriteOnce".to_string()]
}

pub fn kafka_port() -> i32 {
    crate::constants::ports::KAFKA
}

pub fn wal_port() -> i32 {
    crate::constants::ports::WAL
}

pub fn raft_port() -> i32 {
    crate::constants::ports::RAFT
}

pub fn unified_api_port() -> i32 {
    crate::constants::ports::UNIFIED_API
}

pub fn wal_profile() -> String {
    crate::constants::defaults::WAL_PROFILE.to_string()
}

pub fn produce_profile() -> String {
    crate::constants::defaults::PRODUCE_PROFILE.to_string()
}

pub fn log_level() -> String {
    crate::constants::defaults::LOG_LEVEL.to_string()
}

pub fn replicas() -> i32 {
    crate::constants::defaults::MIN_CLUSTER_REPLICAS
}

pub fn replication_factor() -> i32 {
    crate::constants::defaults::REPLICATION_FACTOR
}

pub fn min_insync_replicas() -> i32 {
    crate::constants::defaults::MIN_INSYNC_REPLICAS
}

pub fn max_unavailable() -> i32 {
    1
}

pub fn restart_delay_secs() -> u64 {
    30
}

pub fn anti_affinity() -> String {
    "preferred".to_string()
}

pub fn anti_affinity_topology_key() -> String {
    "kubernetes.io/hostname".to_string()
}

pub fn service_type() -> String {
    "ClusterIP".to_string()
}

pub fn embedding_model() -> String {
    "text-embedding-3-small".to_string()
}

pub fn embedding_provider() -> String {
    "openai".to_string()
}

pub fn auto_recover() -> bool {
    true
}

pub fn min_replicas() -> i32 {
    3
}

pub fn max_replicas() -> i32 {
    9
}

pub fn scale_up_cooldown_secs() -> u64 {
    600
}

pub fn scale_down_cooldown_secs() -> u64 {
    1800
}

pub fn scale_up_stabilization_count() -> u32 {
    3
}

pub fn scale_down_stabilization_count() -> u32 {
    6
}

pub fn tolerance_percent() -> u32 {
    10
}

pub fn topic_partitions() -> i32 {
    1
}

pub fn topic_replication_factor() -> i32 {
    3
}

pub fn columnar_format() -> String {
    "parquet".to_string()
}

pub fn columnar_compression() -> String {
    "zstd".to_string()
}

pub fn columnar_partitioning() -> String {
    "daily".to_string()
}

pub fn index_metric() -> String {
    "cosine".to_string()
}

pub fn vector_field() -> String {
    "value".to_string()
}

pub fn auth_type() -> String {
    "sasl-scram-sha-256".to_string()
}

pub fn acl_effect() -> String {
    "Allow".to_string()
}

pub fn acl_pattern_type() -> String {
    "literal".to_string()
}

pub fn schema_registry_role() -> String {
    "readwrite".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults_produce_expected_values() {
        assert_eq!(kafka_port(), 9092);
        assert_eq!(wal_port(), 9291);
        assert_eq!(raft_port(), 5001);
        assert_eq!(unified_api_port(), 6092);
        assert_eq!(replicas(), 3);
        assert_eq!(replication_factor(), 3);
        assert_eq!(min_insync_replicas(), 2);
        assert_eq!(max_unavailable(), 1);
        assert_eq!(restart_delay_secs(), 30);
        assert_eq!(storage_size(), "50Gi");
        assert_eq!(wal_profile(), "medium");
        assert_eq!(produce_profile(), "balanced");
        assert_eq!(log_level(), "info");
        assert_eq!(anti_affinity(), "preferred");
        assert_eq!(service_type(), "ClusterIP");
        assert_eq!(scale_up_cooldown_secs(), 600);
        assert_eq!(scale_down_cooldown_secs(), 1800);
        assert_eq!(topic_partitions(), 1);
        assert_eq!(topic_replication_factor(), 3);
    }
}
