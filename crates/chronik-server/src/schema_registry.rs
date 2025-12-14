//! Schema Registry for Avro, JSON Schema, and Protobuf schemas
//!
//! This module provides a Confluent-compatible Schema Registry that:
//! - Stores and retrieves schemas with unique IDs
//! - Manages subjects (topic-value, topic-key naming)
//! - Supports schema evolution with compatibility checking
//! - Provides REST API endpoints
//!
//! # Usage
//!
//! The Schema Registry runs as an HTTP service (default port 8081).
//!
//! ```bash
//! # Enable Schema Registry
//! CHRONIK_SCHEMA_REGISTRY_ENABLED=true
//! CHRONIK_SCHEMA_REGISTRY_PORT=8081
//!
//! # Register a schema
//! curl -X POST http://localhost:8081/subjects/my-topic-value/versions \
//!   -H "Content-Type: application/vnd.schemaregistry.v1+json" \
//!   -d '{"schema": "{\"type\":\"record\",\"name\":\"User\",\"fields\":[{\"name\":\"name\",\"type\":\"string\"}]}"}'
//!
//! # Get schema by ID
//! curl http://localhost:8081/schemas/ids/1
//!
//! # Get latest schema for subject
//! curl http://localhost:8081/subjects/my-topic-value/versions/latest
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use serde::{Deserialize, Serialize};

/// Schema types supported by the registry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SchemaType {
    /// Apache Avro schema
    Avro,
    /// JSON Schema
    Json,
    /// Protocol Buffers schema
    Protobuf,
}

impl Default for SchemaType {
    fn default() -> Self {
        SchemaType::Avro
    }
}

impl std::fmt::Display for SchemaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaType::Avro => write!(f, "AVRO"),
            SchemaType::Json => write!(f, "JSON"),
            SchemaType::Protobuf => write!(f, "PROTOBUF"),
        }
    }
}

/// Compatibility levels for schema evolution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompatibilityLevel {
    /// No compatibility checking
    None,
    /// New schema can read data written by old schema
    Backward,
    /// New schema can read data written by all previous schemas
    BackwardTransitive,
    /// Old schema can read data written by new schema
    Forward,
    /// Old schema can read data written by all new schemas
    ForwardTransitive,
    /// Both backward and forward compatible
    Full,
    /// Full compatibility with all previous versions
    FullTransitive,
}

impl Default for CompatibilityLevel {
    fn default() -> Self {
        CompatibilityLevel::Backward
    }
}

/// A registered schema with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredSchema {
    /// Unique schema ID (global across all subjects)
    pub id: u32,
    /// The schema string (JSON for Avro/JSON Schema, proto for Protobuf)
    pub schema: String,
    /// Schema type
    #[serde(rename = "schemaType", default)]
    pub schema_type: SchemaType,
    /// Optional references to other schemas
    #[serde(default)]
    pub references: Vec<SchemaReference>,
}

/// Reference to another schema (for Protobuf imports, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaReference {
    /// Reference name
    pub name: String,
    /// Subject containing the referenced schema
    pub subject: String,
    /// Version of the referenced schema
    pub version: u32,
}

/// Schema version within a subject
#[derive(Debug, Clone)]
pub struct SchemaVersion {
    /// Version number (1-based, incrementing per subject)
    pub version: u32,
    /// Global schema ID
    pub schema_id: u32,
    /// The schema content
    pub schema: String,
    /// Schema type
    pub schema_type: SchemaType,
    /// References
    pub references: Vec<SchemaReference>,
}

/// Subject metadata
#[derive(Debug, Clone)]
pub struct Subject {
    /// Subject name (e.g., "my-topic-value")
    pub name: String,
    /// Compatibility level for this subject
    pub compatibility: CompatibilityLevel,
    /// Schema versions (version number -> schema version)
    pub versions: HashMap<u32, SchemaVersion>,
    /// Latest version number
    pub latest_version: u32,
}

impl Subject {
    fn new(name: String) -> Self {
        Self {
            name,
            compatibility: CompatibilityLevel::default(),
            versions: HashMap::new(),
            latest_version: 0,
        }
    }
}

/// Schema Registry configuration
#[derive(Debug, Clone)]
pub struct SchemaRegistryConfig {
    /// Whether the registry is enabled
    pub enabled: bool,
    /// HTTP port for the REST API
    pub port: u16,
    /// Default compatibility level
    pub default_compatibility: CompatibilityLevel,
    /// Whether HTTP Basic Auth is required
    pub auth_enabled: bool,
    /// Users allowed to access (username -> password)
    /// Format: "user1:pass1,user2:pass2"
    pub users: HashMap<String, String>,
}

impl Default for SchemaRegistryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8081,
            default_compatibility: CompatibilityLevel::Backward,
            auth_enabled: false,
            users: HashMap::new(),
        }
    }
}

impl SchemaRegistryConfig {
    /// Create configuration from environment variables
    ///
    /// # Environment Variables
    ///
    /// - `CHRONIK_SCHEMA_REGISTRY_ENABLED` - Enable Schema Registry (default: false)
    /// - `CHRONIK_SCHEMA_REGISTRY_PORT` - HTTP port (default: 8081, but uses Admin API port in practice)
    /// - `CHRONIK_SCHEMA_REGISTRY_COMPATIBILITY` - Default compatibility level (default: BACKWARD)
    /// - `CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED` - Enable HTTP Basic Auth (default: false)
    /// - `CHRONIK_SCHEMA_REGISTRY_USERS` - Comma-separated user:password pairs (e.g., "admin:secret,readonly:pass")
    pub fn from_env() -> Self {
        let enabled = std::env::var("CHRONIK_SCHEMA_REGISTRY_ENABLED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        let port = std::env::var("CHRONIK_SCHEMA_REGISTRY_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8081);

        let default_compatibility = std::env::var("CHRONIK_SCHEMA_REGISTRY_COMPATIBILITY")
            .ok()
            .and_then(|v| match v.to_uppercase().as_str() {
                "NONE" => Some(CompatibilityLevel::None),
                "BACKWARD" => Some(CompatibilityLevel::Backward),
                "BACKWARD_TRANSITIVE" => Some(CompatibilityLevel::BackwardTransitive),
                "FORWARD" => Some(CompatibilityLevel::Forward),
                "FORWARD_TRANSITIVE" => Some(CompatibilityLevel::ForwardTransitive),
                "FULL" => Some(CompatibilityLevel::Full),
                "FULL_TRANSITIVE" => Some(CompatibilityLevel::FullTransitive),
                _ => None,
            })
            .unwrap_or(CompatibilityLevel::Backward);

        // Parse authentication settings
        let auth_enabled = std::env::var("CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        // Parse users from CHRONIK_SCHEMA_REGISTRY_USERS
        // Format: "user1:pass1,user2:pass2"
        let users: HashMap<String, String> = std::env::var("CHRONIK_SCHEMA_REGISTRY_USERS")
            .ok()
            .map(|v| {
                v.split(',')
                    .filter_map(|pair| {
                        let parts: Vec<&str> = pair.splitn(2, ':').collect();
                        if parts.len() == 2 {
                            Some((parts[0].trim().to_string(), parts[1].trim().to_string()))
                        } else {
                            warn!("Invalid user:password pair in CHRONIK_SCHEMA_REGISTRY_USERS: {}", pair);
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        if auth_enabled {
            if users.is_empty() {
                warn!("⚠ CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED=true but no users configured!");
                warn!("⚠ Set CHRONIK_SCHEMA_REGISTRY_USERS=user:password to add users");
            } else {
                info!("✓ Schema Registry HTTP Basic Auth enabled ({} users)", users.len());
            }
        }

        Self {
            enabled,
            port,
            default_compatibility,
            auth_enabled,
            users,
        }
    }

    /// Check if credentials are valid
    pub fn validate_credentials(&self, username: &str, password: &str) -> bool {
        if !self.auth_enabled {
            return true; // No auth required
        }
        self.users.get(username).map(|p| p == password).unwrap_or(false)
    }

    /// Check if authentication is required
    pub fn is_auth_required(&self) -> bool {
        self.auth_enabled && !self.users.is_empty()
    }
}

/// Schema Registry store
pub struct SchemaRegistry {
    /// Configuration
    config: SchemaRegistryConfig,
    /// Next schema ID
    next_id: AtomicU32,
    /// Schemas by ID
    schemas_by_id: RwLock<HashMap<u32, RegisteredSchema>>,
    /// Subjects (subject name -> Subject)
    subjects: RwLock<HashMap<String, Subject>>,
    /// Schema hash to ID mapping (for deduplication)
    schema_hashes: RwLock<HashMap<u64, u32>>,
    /// Global compatibility level
    global_compatibility: RwLock<CompatibilityLevel>,
}

impl Default for SchemaRegistry {
    fn default() -> Self {
        Self::new(SchemaRegistryConfig::default())
    }
}

impl SchemaRegistry {
    /// Create a new schema registry
    pub fn new(config: SchemaRegistryConfig) -> Self {
        let global_compatibility = config.default_compatibility;

        if config.enabled {
            info!("Schema Registry enabled on port {}", config.port);
            info!("Default compatibility level: {:?}", global_compatibility);
        } else {
            info!("Schema Registry disabled");
        }

        Self {
            config,
            next_id: AtomicU32::new(1),
            schemas_by_id: RwLock::new(HashMap::new()),
            subjects: RwLock::new(HashMap::new()),
            schema_hashes: RwLock::new(HashMap::new()),
            global_compatibility: RwLock::new(global_compatibility),
        }
    }

    /// Check if the registry is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Get the configured port
    pub fn port(&self) -> u16 {
        self.config.port
    }

    /// Check if authentication is required for Schema Registry
    pub fn is_auth_required(&self) -> bool {
        self.config.is_auth_required()
    }

    /// Validate credentials for Schema Registry access
    pub fn validate_credentials(&self, username: &str, password: &str) -> bool {
        self.config.validate_credentials(username, password)
    }

    /// Register a new schema under a subject
    ///
    /// Returns the schema ID (existing if schema already registered)
    pub async fn register_schema(
        &self,
        subject: &str,
        schema: &str,
        schema_type: SchemaType,
        references: Vec<SchemaReference>,
    ) -> Result<u32, SchemaRegistryError> {
        // Normalize and validate schema
        let normalized_schema = self.normalize_schema(schema, schema_type)?;

        // Check if schema already exists (by content hash)
        let schema_hash = self.hash_schema(&normalized_schema);
        {
            let hashes = self.schema_hashes.read().await;
            if let Some(&existing_id) = hashes.get(&schema_hash) {
                debug!("Schema already registered with ID {}", existing_id);

                // Still need to register version under this subject if not already
                self.register_version(subject, existing_id, &normalized_schema, schema_type, references.clone())
                    .await?;

                return Ok(existing_id);
            }
        }

        // Check compatibility with existing schemas
        self.check_compatibility(subject, &normalized_schema, schema_type).await?;

        // Allocate new schema ID
        let schema_id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // Store schema
        {
            let mut schemas = self.schemas_by_id.write().await;
            schemas.insert(
                schema_id,
                RegisteredSchema {
                    id: schema_id,
                    schema: normalized_schema.clone(),
                    schema_type,
                    references: references.clone(),
                },
            );
        }

        // Store hash mapping
        {
            let mut hashes = self.schema_hashes.write().await;
            hashes.insert(schema_hash, schema_id);
        }

        // Register version under subject
        self.register_version(subject, schema_id, &normalized_schema, schema_type, references)
            .await?;

        info!(
            "Registered schema ID {} under subject '{}' (type: {})",
            schema_id, subject, schema_type
        );

        Ok(schema_id)
    }

    /// Register a schema version under a subject
    async fn register_version(
        &self,
        subject_name: &str,
        schema_id: u32,
        schema: &str,
        schema_type: SchemaType,
        references: Vec<SchemaReference>,
    ) -> Result<u32, SchemaRegistryError> {
        let mut subjects = self.subjects.write().await;

        let subject = subjects
            .entry(subject_name.to_string())
            .or_insert_with(|| Subject::new(subject_name.to_string()));

        // Check if this schema is already registered under this subject
        for (version, sv) in &subject.versions {
            if sv.schema_id == schema_id {
                return Ok(*version);
            }
        }

        // Add new version
        let version = subject.latest_version + 1;
        subject.versions.insert(
            version,
            SchemaVersion {
                version,
                schema_id,
                schema: schema.to_string(),
                schema_type,
                references,
            },
        );
        subject.latest_version = version;

        debug!(
            "Registered version {} for subject '{}' (schema ID {})",
            version, subject_name, schema_id
        );

        Ok(version)
    }

    /// Get schema by ID
    pub async fn get_schema(&self, id: u32) -> Option<RegisteredSchema> {
        let schemas = self.schemas_by_id.read().await;
        schemas.get(&id).cloned()
    }

    /// Get schema by subject and version
    pub async fn get_schema_by_subject(
        &self,
        subject: &str,
        version: SchemaVersionRef,
    ) -> Option<SchemaVersion> {
        let subjects = self.subjects.read().await;
        let subj = subjects.get(subject)?;

        match version {
            SchemaVersionRef::Latest => {
                subj.versions.get(&subj.latest_version).cloned()
            }
            SchemaVersionRef::Version(v) => {
                subj.versions.get(&v).cloned()
            }
        }
    }

    /// List all subjects
    pub async fn list_subjects(&self) -> Vec<String> {
        let subjects = self.subjects.read().await;
        subjects.keys().cloned().collect()
    }

    /// List versions for a subject
    pub async fn list_versions(&self, subject: &str) -> Option<Vec<u32>> {
        let subjects = self.subjects.read().await;
        subjects.get(subject).map(|s| {
            let mut versions: Vec<_> = s.versions.keys().cloned().collect();
            versions.sort();
            versions
        })
    }

    /// Delete a subject
    pub async fn delete_subject(&self, subject: &str, permanent: bool) -> Result<Vec<u32>, SchemaRegistryError> {
        let mut subjects = self.subjects.write().await;

        if let Some(subj) = subjects.remove(subject) {
            let versions: Vec<u32> = subj.versions.keys().cloned().collect();

            if permanent {
                // Also remove schemas if not referenced by other subjects
                // For now, we keep schemas to maintain ID stability
                debug!("Permanently deleted subject '{}' ({} versions)", subject, versions.len());
            } else {
                debug!("Soft-deleted subject '{}' ({} versions)", subject, versions.len());
            }

            info!("Deleted subject '{}' with {} versions", subject, versions.len());
            Ok(versions)
        } else {
            Err(SchemaRegistryError::SubjectNotFound(subject.to_string()))
        }
    }

    /// Delete a specific version of a subject
    pub async fn delete_version(
        &self,
        subject: &str,
        version: u32,
        _permanent: bool,
    ) -> Result<u32, SchemaRegistryError> {
        let mut subjects = self.subjects.write().await;

        let subj = subjects
            .get_mut(subject)
            .ok_or_else(|| SchemaRegistryError::SubjectNotFound(subject.to_string()))?;

        if subj.versions.remove(&version).is_some() {
            // Update latest_version if needed
            if version == subj.latest_version {
                subj.latest_version = subj.versions.keys().max().copied().unwrap_or(0);
            }

            info!("Deleted version {} of subject '{}'", version, subject);
            Ok(version)
        } else {
            Err(SchemaRegistryError::VersionNotFound(subject.to_string(), version))
        }
    }

    /// Get compatibility level for a subject
    pub async fn get_compatibility(&self, subject: Option<&str>) -> CompatibilityLevel {
        if let Some(subject_name) = subject {
            let subjects = self.subjects.read().await;
            if let Some(subj) = subjects.get(subject_name) {
                return subj.compatibility;
            }
        }

        *self.global_compatibility.read().await
    }

    /// Set compatibility level
    pub async fn set_compatibility(
        &self,
        subject: Option<&str>,
        level: CompatibilityLevel,
    ) -> Result<(), SchemaRegistryError> {
        if let Some(subject_name) = subject {
            let mut subjects = self.subjects.write().await;
            let subj = subjects
                .get_mut(subject_name)
                .ok_or_else(|| SchemaRegistryError::SubjectNotFound(subject_name.to_string()))?;

            subj.compatibility = level;
            info!("Set compatibility for subject '{}' to {:?}", subject_name, level);
        } else {
            let mut global = self.global_compatibility.write().await;
            *global = level;
            info!("Set global compatibility to {:?}", level);
        }

        Ok(())
    }

    /// Check if a new schema is compatible with existing schemas
    async fn check_compatibility(
        &self,
        subject: &str,
        _new_schema: &str,
        _schema_type: SchemaType,
    ) -> Result<(), SchemaRegistryError> {
        let compatibility = self.get_compatibility(Some(subject)).await;

        if compatibility == CompatibilityLevel::None {
            return Ok(());
        }

        let subjects = self.subjects.read().await;
        if let Some(subj) = subjects.get(subject) {
            if subj.versions.is_empty() {
                return Ok(());
            }

            // TODO: Implement actual schema compatibility checking
            // For now, we just check that the schema can be parsed
            // Full implementation would require:
            // - Avro: apache-avro crate for schema parsing and resolution
            // - JSON Schema: jsonschema crate for validation
            // - Protobuf: prost crate for proto parsing

            debug!(
                "Compatibility check for subject '{}' (level: {:?}) - PASSED (basic)",
                subject, compatibility
            );
        }

        Ok(())
    }

    /// Normalize a schema (remove whitespace, sort fields, etc.)
    fn normalize_schema(&self, schema: &str, schema_type: SchemaType) -> Result<String, SchemaRegistryError> {
        match schema_type {
            SchemaType::Avro | SchemaType::Json => {
                // Parse and re-serialize JSON to normalize
                match serde_json::from_str::<serde_json::Value>(schema) {
                    Ok(value) => Ok(serde_json::to_string(&value)
                        .map_err(|e| SchemaRegistryError::InvalidSchema(e.to_string()))?),
                    Err(e) => Err(SchemaRegistryError::InvalidSchema(format!(
                        "Invalid JSON schema: {}",
                        e
                    ))),
                }
            }
            SchemaType::Protobuf => {
                // For Protobuf, just trim whitespace for now
                // Full normalization would require parsing the proto file
                Ok(schema.trim().to_string())
            }
        }
    }

    /// Hash a schema for deduplication
    fn hash_schema(&self, schema: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        schema.hash(&mut hasher);
        hasher.finish()
    }
}

/// Reference to a schema version
#[derive(Debug, Clone, Copy)]
pub enum SchemaVersionRef {
    /// Latest version
    Latest,
    /// Specific version number
    Version(u32),
}

impl std::str::FromStr for SchemaVersionRef {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("latest") {
            Ok(SchemaVersionRef::Latest)
        } else {
            s.parse().map(SchemaVersionRef::Version)
        }
    }
}

/// Schema Registry errors
#[derive(Debug, Clone, thiserror::Error)]
pub enum SchemaRegistryError {
    #[error("Schema not found: {0}")]
    SchemaNotFound(u32),

    #[error("Subject not found: {0}")]
    SubjectNotFound(String),

    #[error("Version not found: {0} version {1}")]
    VersionNotFound(String, u32),

    #[error("Invalid schema: {0}")]
    InvalidSchema(String),

    #[error("Schema incompatible: {0}")]
    Incompatible(String),

    #[error("Subject already exists: {0}")]
    SubjectAlreadyExists(String),
}

// =============================================================================
// REST API Types (for HTTP handler integration)
// =============================================================================

/// Request to register a schema
#[derive(Debug, Deserialize)]
pub struct RegisterSchemaRequest {
    /// The schema string
    pub schema: String,
    /// Schema type (defaults to AVRO)
    #[serde(rename = "schemaType", default)]
    pub schema_type: SchemaType,
    /// References to other schemas
    #[serde(default)]
    pub references: Vec<SchemaReference>,
}

/// Response from registering a schema
#[derive(Debug, Serialize)]
pub struct RegisterSchemaResponse {
    /// The assigned schema ID
    pub id: u32,
}

/// Response containing a schema
#[derive(Debug, Serialize)]
pub struct GetSchemaResponse {
    /// The schema string
    pub schema: String,
    /// Schema type
    #[serde(rename = "schemaType")]
    pub schema_type: SchemaType,
    /// References
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<SchemaReference>,
}

/// Response containing schema with subject info
#[derive(Debug, Serialize)]
pub struct GetSubjectVersionResponse {
    /// Subject name
    pub subject: String,
    /// Version number
    pub version: u32,
    /// Schema ID
    pub id: u32,
    /// The schema string
    pub schema: String,
    /// Schema type
    #[serde(rename = "schemaType")]
    pub schema_type: SchemaType,
}

/// Compatibility config response
#[derive(Debug, Serialize, Deserialize)]
pub struct CompatibilityConfig {
    #[serde(rename = "compatibilityLevel")]
    pub compatibility_level: CompatibilityLevel,
}

/// Error response
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error_code: u32,
    pub message: String,
}

impl From<SchemaRegistryError> for ErrorResponse {
    fn from(err: SchemaRegistryError) -> Self {
        let (code, message) = match &err {
            SchemaRegistryError::SchemaNotFound(_) => (40403, err.to_string()),
            SchemaRegistryError::SubjectNotFound(_) => (40401, err.to_string()),
            SchemaRegistryError::VersionNotFound(_, _) => (40402, err.to_string()),
            SchemaRegistryError::InvalidSchema(_) => (42201, err.to_string()),
            SchemaRegistryError::Incompatible(_) => (409, err.to_string()),
            SchemaRegistryError::SubjectAlreadyExists(_) => (409, err.to_string()),
        };

        ErrorResponse {
            error_code: code,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_registry() -> SchemaRegistry {
        SchemaRegistry::new(SchemaRegistryConfig {
            enabled: true,
            port: 8081,
            default_compatibility: CompatibilityLevel::Backward,
        })
    }

    #[tokio::test]
    async fn test_register_schema() {
        let registry = create_test_registry();

        let schema = r#"{"type":"record","name":"User","fields":[{"name":"name","type":"string"}]}"#;

        let id = registry
            .register_schema("test-topic-value", schema, SchemaType::Avro, vec![])
            .await
            .unwrap();

        assert_eq!(id, 1);

        // Register same schema again - should return same ID
        let id2 = registry
            .register_schema("test-topic-value", schema, SchemaType::Avro, vec![])
            .await
            .unwrap();

        assert_eq!(id2, 1);
    }

    #[tokio::test]
    async fn test_get_schema_by_id() {
        let registry = create_test_registry();

        let schema = r#"{"type":"string"}"#;
        let id = registry
            .register_schema("test-subject", schema, SchemaType::Avro, vec![])
            .await
            .unwrap();

        let retrieved = registry.get_schema(id).await.unwrap();
        assert_eq!(retrieved.id, id);
        assert_eq!(retrieved.schema_type, SchemaType::Avro);
    }

    #[tokio::test]
    async fn test_get_schema_by_subject() {
        let registry = create_test_registry();

        let schema = r#"{"type":"int"}"#;
        registry
            .register_schema("my-subject", schema, SchemaType::Avro, vec![])
            .await
            .unwrap();

        let sv = registry
            .get_schema_by_subject("my-subject", SchemaVersionRef::Latest)
            .await
            .unwrap();

        assert_eq!(sv.version, 1);
    }

    #[tokio::test]
    async fn test_list_subjects() {
        let registry = create_test_registry();

        registry
            .register_schema("subject-a", r#"{"type":"string"}"#, SchemaType::Avro, vec![])
            .await
            .unwrap();
        registry
            .register_schema("subject-b", r#"{"type":"int"}"#, SchemaType::Avro, vec![])
            .await
            .unwrap();

        let subjects = registry.list_subjects().await;
        assert_eq!(subjects.len(), 2);
        assert!(subjects.contains(&"subject-a".to_string()));
        assert!(subjects.contains(&"subject-b".to_string()));
    }

    #[tokio::test]
    async fn test_multiple_versions() {
        let registry = create_test_registry();

        // Register first version
        let schema1 = r#"{"type":"record","name":"User","fields":[{"name":"name","type":"string"}]}"#;
        let id1 = registry
            .register_schema("user-value", schema1, SchemaType::Avro, vec![])
            .await
            .unwrap();

        // Register second version (different schema)
        let schema2 = r#"{"type":"record","name":"User","fields":[{"name":"name","type":"string"},{"name":"age","type":"int"}]}"#;
        let id2 = registry
            .register_schema("user-value", schema2, SchemaType::Avro, vec![])
            .await
            .unwrap();

        assert_ne!(id1, id2);

        let versions = registry.list_versions("user-value").await.unwrap();
        assert_eq!(versions, vec![1, 2]);

        // Get version 1
        let v1 = registry
            .get_schema_by_subject("user-value", SchemaVersionRef::Version(1))
            .await
            .unwrap();
        assert_eq!(v1.schema_id, id1);

        // Get latest (should be version 2)
        let latest = registry
            .get_schema_by_subject("user-value", SchemaVersionRef::Latest)
            .await
            .unwrap();
        assert_eq!(latest.schema_id, id2);
        assert_eq!(latest.version, 2);
    }

    #[tokio::test]
    async fn test_delete_subject() {
        let registry = create_test_registry();

        registry
            .register_schema("to-delete", r#"{"type":"string"}"#, SchemaType::Avro, vec![])
            .await
            .unwrap();

        let deleted_versions = registry.delete_subject("to-delete", false).await.unwrap();
        assert_eq!(deleted_versions.len(), 1);

        // Subject should be gone
        let subjects = registry.list_subjects().await;
        assert!(!subjects.contains(&"to-delete".to_string()));
    }

    #[tokio::test]
    async fn test_compatibility_levels() {
        let registry = create_test_registry();

        // Check default global compatibility
        let global = registry.get_compatibility(None).await;
        assert_eq!(global, CompatibilityLevel::Backward);

        // Set subject-specific compatibility
        registry
            .register_schema("compat-test", r#"{"type":"string"}"#, SchemaType::Avro, vec![])
            .await
            .unwrap();

        registry
            .set_compatibility(Some("compat-test"), CompatibilityLevel::Full)
            .await
            .unwrap();

        let subject_compat = registry.get_compatibility(Some("compat-test")).await;
        assert_eq!(subject_compat, CompatibilityLevel::Full);

        // Global should still be Backward
        let global = registry.get_compatibility(None).await;
        assert_eq!(global, CompatibilityLevel::Backward);
    }

    #[tokio::test]
    async fn test_json_schema_type() {
        let registry = create_test_registry();

        let schema = r#"{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"name":{"type":"string"}}}"#;

        let id = registry
            .register_schema("json-test", schema, SchemaType::Json, vec![])
            .await
            .unwrap();

        let retrieved = registry.get_schema(id).await.unwrap();
        assert_eq!(retrieved.schema_type, SchemaType::Json);
    }

    #[tokio::test]
    async fn test_invalid_schema() {
        let registry = create_test_registry();

        let result = registry
            .register_schema("invalid", "not valid json {", SchemaType::Avro, vec![])
            .await;

        assert!(matches!(result, Err(SchemaRegistryError::InvalidSchema(_))));
    }
}
