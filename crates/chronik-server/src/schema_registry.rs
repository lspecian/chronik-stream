//! Schema Registry implementation for Chronik Stream
//!
//! Provides Confluent Schema Registry compatible API for managing schemas
//! used in Kafka message serialization/deserialization.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Deserialize, Serialize};
use chronik_common::{Result, Error};
use tracing::{info, debug, warn};

/// Schema types supported by the registry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SchemaType {
    Avro,
    Json,
    Protobuf,
}

impl Default for SchemaType {
    fn default() -> Self {
        SchemaType::Avro
    }
}

/// Compatibility modes for schema evolution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompatibilityMode {
    None,
    Backward,
    Forward,
    Full,
    BackwardTransitive,
    ForwardTransitive,
    FullTransitive,
}

impl Default for CompatibilityMode {
    fn default() -> Self {
        CompatibilityMode::Backward
    }
}

/// Schema definition stored in the registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub id: i32,
    pub version: i32,
    pub subject: String,
    pub schema: String,
    #[serde(rename = "schemaType")]
    pub schema_type: SchemaType,
    pub references: Vec<SchemaReference>,
}

/// Reference to another schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaReference {
    pub name: String,
    pub subject: String,
    pub version: i32,
}

/// Subject configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectConfig {
    pub subject: String,
    pub compatibility: CompatibilityMode,
}

/// Schema Registry storage backend
pub struct SchemaRegistry {
    /// Next available schema ID
    next_id: Arc<RwLock<i32>>,
    /// Schema storage by ID
    schemas_by_id: Arc<RwLock<HashMap<i32, Schema>>>,
    /// Schema storage by subject and version
    pub(crate) schemas_by_subject: Arc<RwLock<HashMap<String, Vec<Schema>>>>,
    /// Subject configurations
    subject_configs: Arc<RwLock<HashMap<String, SubjectConfig>>>,
    /// Global compatibility setting
    pub(crate) global_compatibility: Arc<RwLock<CompatibilityMode>>,
    /// Metadata store for persistence
    metadata_store: Arc<dyn chronik_common::metadata::traits::MetadataStore>,
}

impl SchemaRegistry {
    /// Create a new Schema Registry
    pub async fn new(metadata_store: Arc<dyn chronik_common::metadata::traits::MetadataStore>) -> Result<Self> {
        info!("Initializing Schema Registry");

        // Load existing schemas from metadata store
        let (next_id, schemas_by_id, schemas_by_subject) = Self::load_schemas(&metadata_store).await?;
        let subject_configs = Self::load_configs(&metadata_store).await?;

        Ok(Self {
            next_id: Arc::new(RwLock::new(next_id)),
            schemas_by_id: Arc::new(RwLock::new(schemas_by_id)),
            schemas_by_subject: Arc::new(RwLock::new(schemas_by_subject)),
            subject_configs: Arc::new(RwLock::new(subject_configs)),
            global_compatibility: Arc::new(RwLock::new(CompatibilityMode::Backward)),
            metadata_store,
        })
    }

    /// Load schemas from metadata store
    async fn load_schemas(
        _metadata_store: &Arc<dyn chronik_common::metadata::traits::MetadataStore>,
    ) -> Result<(i32, HashMap<i32, Schema>, HashMap<String, Vec<Schema>>)> {
        // TODO: Implement loading from metadata store
        // For now, return empty collections
        Ok((1, HashMap::new(), HashMap::new()))
    }

    /// Load subject configurations from metadata store
    async fn load_configs(
        _metadata_store: &Arc<dyn chronik_common::metadata::traits::MetadataStore>,
    ) -> Result<HashMap<String, SubjectConfig>> {
        // TODO: Implement loading from metadata store
        Ok(HashMap::new())
    }

    /// Register a new schema or return existing schema ID if it already exists
    pub async fn register_schema(
        &self,
        subject: String,
        schema: String,
        schema_type: SchemaType,
        references: Vec<SchemaReference>,
    ) -> Result<i32> {
        debug!("Registering schema for subject: {}", subject);

        // Check if schema already exists for this subject
        let schemas = self.schemas_by_subject.read().await;
        if let Some(subject_schemas) = schemas.get(&subject) {
            for existing in subject_schemas {
                if existing.schema == schema && existing.schema_type == schema_type {
                    debug!("Schema already exists with ID: {}", existing.id);
                    return Ok(existing.id);
                }
            }
        }
        drop(schemas);

        // Check compatibility if needed
        if let Err(e) = self.check_compatibility(&subject, &schema, schema_type).await {
            warn!("Schema compatibility check failed: {:?}", e);
            return Err(e);
        }

        // Assign new ID and version
        let mut next_id = self.next_id.write().await;
        let schema_id = *next_id;
        *next_id += 1;
        drop(next_id);

        let mut schemas_by_subject = self.schemas_by_subject.write().await;
        let version = schemas_by_subject
            .get(&subject)
            .map(|s| s.len() as i32 + 1)
            .unwrap_or(1);

        let new_schema = Schema {
            id: schema_id,
            version,
            subject: subject.clone(),
            schema: schema.clone(),
            schema_type,
            references,
        };

        // Store in memory
        let mut schemas_by_id = self.schemas_by_id.write().await;
        schemas_by_id.insert(schema_id, new_schema.clone());

        schemas_by_subject
            .entry(subject.clone())
            .or_insert_with(Vec::new)
            .push(new_schema.clone());

        // TODO: Persist to metadata store
        info!("Registered new schema ID {} for subject {} version {}", schema_id, subject, version);

        Ok(schema_id)
    }

    /// Get schema by ID
    pub async fn get_schema(&self, id: i32) -> Result<Schema> {
        let schemas = self.schemas_by_id.read().await;
        schemas
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("Schema with ID {} not found", id)))
    }

    /// Get latest schema for a subject
    pub async fn get_latest_schema(&self, subject: &str) -> Result<Schema> {
        let schemas = self.schemas_by_subject.read().await;
        schemas
            .get(subject)
            .and_then(|s| s.last().cloned())
            .ok_or_else(|| Error::NotFound(format!("No schema found for subject {}", subject)))
    }

    /// Get specific version of a schema
    pub async fn get_schema_by_version(&self, subject: &str, version: i32) -> Result<Schema> {
        let schemas = self.schemas_by_subject.read().await;
        schemas
            .get(subject)
            .and_then(|s| s.iter().find(|schema| schema.version == version).cloned())
            .ok_or_else(|| Error::NotFound(format!("Schema version {} not found for subject {}", version, subject)))
    }

    /// List all subjects
    pub async fn list_subjects(&self) -> Result<Vec<String>> {
        let schemas = self.schemas_by_subject.read().await;
        Ok(schemas.keys().cloned().collect())
    }

    /// Delete a subject and all its schemas
    pub async fn delete_subject(&self, subject: &str) -> Result<Vec<i32>> {
        debug!("Deleting subject: {}", subject);

        let mut schemas_by_subject = self.schemas_by_subject.write().await;
        let mut schemas_by_id = self.schemas_by_id.write().await;

        if let Some(schemas) = schemas_by_subject.remove(subject) {
            let versions: Vec<i32> = schemas.iter().map(|s| s.version).collect();
            for schema in schemas {
                schemas_by_id.remove(&schema.id);
            }

            // TODO: Persist deletion to metadata store
            info!("Deleted subject {} with {} versions", subject, versions.len());
            Ok(versions)
        } else {
            Err(Error::NotFound(format!("Subject {} not found", subject)))
        }
    }

    /// Set compatibility mode for a subject
    pub async fn set_compatibility(&self, subject: String, mode: CompatibilityMode) -> Result<()> {
        debug!("Setting compatibility mode for subject {}: {:?}", subject, mode);

        let mut configs = self.subject_configs.write().await;
        configs.insert(
            subject.clone(),
            SubjectConfig {
                subject: subject.clone(),
                compatibility: mode,
            },
        );

        // TODO: Persist to metadata store
        info!("Set compatibility mode {:?} for subject {}", mode, subject);
        Ok(())
    }

    /// Get compatibility mode for a subject
    pub async fn get_compatibility(&self, subject: &str) -> Result<CompatibilityMode> {
        let configs = self.subject_configs.read().await;
        if let Some(config) = configs.get(subject) {
            Ok(config.compatibility)
        } else {
            // Return global default if no subject-specific config
            Ok(*self.global_compatibility.read().await)
        }
    }

    /// Set global compatibility mode
    pub async fn set_global_compatibility(&self, mode: CompatibilityMode) -> Result<()> {
        debug!("Setting global compatibility mode: {:?}", mode);
        *self.global_compatibility.write().await = mode;

        // TODO: Persist to metadata store
        info!("Set global compatibility mode to {:?}", mode);
        Ok(())
    }

    /// Check if a new schema is compatible with existing schemas
    async fn check_compatibility(
        &self,
        subject: &str,
        _schema: &str,
        _schema_type: SchemaType,
    ) -> Result<()> {
        let compatibility = self.get_compatibility(subject).await?;

        if compatibility == CompatibilityMode::None {
            return Ok(());
        }

        let schemas = self.schemas_by_subject.read().await;
        if let Some(existing_schemas) = schemas.get(subject) {
            if !existing_schemas.is_empty() {
                // TODO: Implement actual compatibility checking based on schema type
                // For now, we'll allow all schemas in development
                debug!("Compatibility check passed for subject {} (mode: {:?})", subject, compatibility);
            }
        }

        Ok(())
    }

    /// Test compatibility of a schema without registering it
    pub async fn test_compatibility(
        &self,
        subject: &str,
        _schema: &str,
        _schema_type: SchemaType,
    ) -> Result<bool> {
        self.check_compatibility(subject, _schema, _schema_type).await
            .map(|_| true)
            .or_else(|_| Ok(false))
    }
}

/// Schema Registry HTTP API responses
pub mod api {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct RegisterSchemaRequest {
        pub schema: String,
        #[serde(rename = "schemaType", skip_serializing_if = "Option::is_none")]
        pub schema_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub references: Option<Vec<SchemaReference>>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct RegisterSchemaResponse {
        pub id: i32,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct SchemaResponse {
        pub schema: String,
        #[serde(rename = "schemaType", skip_serializing_if = "Option::is_none")]
        pub schema_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub references: Option<Vec<SchemaReference>>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct SubjectVersionResponse {
        pub subject: String,
        pub id: i32,
        pub version: i32,
        pub schema: String,
        #[serde(rename = "schemaType", skip_serializing_if = "Option::is_none")]
        pub schema_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub references: Option<Vec<SchemaReference>>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct CompatibilityRequest {
        pub compatibility: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct CompatibilityResponse {
        pub compatibility: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct ErrorResponse {
        pub error_code: i32,
        pub message: String,
    }
}