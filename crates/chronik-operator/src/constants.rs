/// Kubernetes label keys following the app.kubernetes.io convention.
pub mod labels {
    pub const NAME: &str = "app.kubernetes.io/name";
    pub const INSTANCE: &str = "app.kubernetes.io/instance";
    pub const COMPONENT: &str = "app.kubernetes.io/component";
    pub const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
    pub const VERSION: &str = "app.kubernetes.io/version";

    /// Chronik-specific labels.
    pub const NODE_ID: &str = "chronik.io/node-id";
    pub const CLUSTER_NAME: &str = "chronik.io/cluster-name";
}

/// Label values.
pub mod values {
    pub const APP_NAME: &str = "chronik-stream";
    pub const MANAGED_BY: &str = "chronik-operator";
    pub const COMPONENT_STANDALONE: &str = "standalone";
    pub const COMPONENT_CLUSTER_NODE: &str = "cluster-node";
}

/// Finalizer name for graceful cleanup.
pub const FINALIZER: &str = "chronik.io/operator-cleanup";

/// Default ports.
pub mod ports {
    pub const KAFKA: i32 = 9092;
    pub const WAL: i32 = 9291;
    pub const RAFT: i32 = 5001;
    pub const UNIFIED_API: i32 = 6092;
    pub const ADMIN_API_BASE: i32 = 10000;
    pub const METRICS_BASE: i32 = 13000;
}

/// Default resource values.
pub mod defaults {
    pub const IMAGE: &str = "ghcr.io/chronik-stream/chronik-server:latest";
    pub const DATA_DIR: &str = "/data";
    pub const STORAGE_SIZE: &str = "50Gi";
    pub const WAL_PROFILE: &str = "medium";
    pub const PRODUCE_PROFILE: &str = "balanced";
    pub const LOG_LEVEL: &str = "info";
    pub const REPLICATION_FACTOR: i32 = 3;
    pub const MIN_INSYNC_REPLICAS: i32 = 2;
    pub const MIN_CLUSTER_REPLICAS: i32 = 3;

    /// Requeue intervals in seconds.
    pub const REQUEUE_HEALTHY_SECS: u64 = 60;
    pub const REQUEUE_NOT_READY_SECS: u64 = 10;
    pub const REQUEUE_CLUSTER_RUNNING_SECS: u64 = 30;
    pub const REQUEUE_CLUSTER_SCALING_SECS: u64 = 5;
    pub const REQUEUE_CLUSTER_DEGRADED_SECS: u64 = 10;
    pub const REQUEUE_TOPIC_SECS: u64 = 60;
    pub const REQUEUE_USER_SECS: u64 = 120;
    pub const REQUEUE_AUTOSCALER_SECS: u64 = 30;
}

/// CRD API group.
pub const API_GROUP: &str = "chronik.io";
