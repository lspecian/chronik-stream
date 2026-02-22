use thiserror::Error;

#[derive(Error, Debug)]
pub enum OperatorError {
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("TOML serialization error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Reconciliation error: {0}")]
    Reconcile(String),

    #[error("Admin API error: {0}")]
    AdminApi(String),

    #[error("Cluster quorum violation: {0}")]
    QuorumViolation(String),

    #[error("Resource not found: {0}")]
    NotFound(String),

    #[error("Finalizer error: {0}")]
    Finalizer(String),
}

pub type Result<T> = std::result::Result<T, OperatorError>;

impl OperatorError {
    /// Whether this error is transient and the reconciliation should be retried.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            OperatorError::Kube(_) | OperatorError::Http(_) | OperatorError::AdminApi(_)
        )
    }
}
