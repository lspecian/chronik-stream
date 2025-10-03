//! Schema Registry HTTP API implementation
//!
//! Provides Confluent Schema Registry compatible REST API endpoints.

use axum::{
    Router,
    extract::{Path, State, Json},
    response::IntoResponse,
    http::StatusCode,
    routing::{get, post, put, delete},
};
use std::sync::Arc;
use serde::Deserialize;
use tracing::{info, debug, error};

use crate::schema_registry::{
    SchemaRegistry, SchemaType, CompatibilityMode,
    api::{
        RegisterSchemaRequest, RegisterSchemaResponse,
        SchemaResponse, SubjectVersionResponse,
        CompatibilityRequest, CompatibilityResponse,
        ErrorResponse,
    },
};

/// Schema Registry API state
#[derive(Clone)]
pub struct SchemaRegistryApi {
    registry: Arc<SchemaRegistry>,
}

impl SchemaRegistryApi {
    /// Create a new Schema Registry API instance
    pub fn new(registry: Arc<SchemaRegistry>) -> Self {
        Self { registry }
    }

    /// Create the Axum router for the Schema Registry API
    pub fn router(self) -> Router {
        Router::new()
            // Schema operations
            .route("/schemas/ids/:id", get(get_schema_by_id))
            .route("/schemas/ids/:id/versions", get(get_schema_versions))

            // Subject operations
            .route("/subjects", get(list_subjects))
            .route("/subjects/:subject", delete(delete_subject))
            .route("/subjects/:subject/versions", get(list_versions).post(register_schema))
            .route("/subjects/:subject/versions/:version", get(get_schema_by_version).delete(delete_schema_version))
            .route("/subjects/:subject/versions/latest", get(get_latest_schema))

            // Compatibility operations
            .route("/compatibility/subjects/:subject/versions/:version", post(test_compatibility))
            .route("/config", get(get_global_config).put(set_global_config))
            .route("/config/:subject", get(get_subject_config).put(set_subject_config).delete(delete_subject_config))

            // Mode operations
            .route("/mode", get(get_mode).put(set_mode))
            .route("/mode/:subject", get(get_subject_mode).put(set_subject_mode).delete(delete_subject_mode))

            .with_state(Arc::new(self))
    }
}

// Handler functions

/// Get schema by ID
async fn get_schema_by_id(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(id): Path<i32>,
) -> Result<Json<SchemaResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting schema by ID: {}", id);

    match api.registry.get_schema(id).await {
        Ok(schema) => Ok(Json(SchemaResponse {
            schema: schema.schema,
            schema_type: Some(format!("{:?}", schema.schema_type).to_uppercase()),
            references: if schema.references.is_empty() {
                None
            } else {
                Some(schema.references)
            },
        })),
        Err(e) => {
            error!("Failed to get schema {}: {:?}", id, e);
            Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error_code: 40403,
                    message: format!("Schema not found: {}", id),
                }),
            ))
        }
    }
}

/// List all subjects
async fn list_subjects(
    State(api): State<Arc<SchemaRegistryApi>>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Listing all subjects");

    match api.registry.list_subjects().await {
        Ok(subjects) => Ok(Json(subjects)),
        Err(e) => {
            error!("Failed to list subjects: {:?}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error_code: 50001,
                    message: "Error listing subjects".to_string(),
                }),
            ))
        }
    }
}

/// Register a new schema
async fn register_schema(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(subject): Path<String>,
    Json(request): Json<RegisterSchemaRequest>,
) -> Result<Json<RegisterSchemaResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Registering schema for subject: {}", subject);

    let schema_type = request.schema_type
        .as_deref()
        .and_then(|s| match s.to_uppercase().as_str() {
            "AVRO" => Some(SchemaType::Avro),
            "JSON" => Some(SchemaType::Json),
            "PROTOBUF" => Some(SchemaType::Protobuf),
            _ => None,
        })
        .unwrap_or_default();

    let references = request.references.unwrap_or_default();

    match api.registry.register_schema(subject.clone(), request.schema, schema_type, references).await {
        Ok(id) => {
            info!("Registered schema ID {} for subject {}", id, subject);
            Ok(Json(RegisterSchemaResponse { id }))
        }
        Err(e) => {
            error!("Failed to register schema for {}: {:?}", subject, e);
            Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error_code: 42201,
                    message: format!("Invalid schema: {:?}", e),
                }),
            ))
        }
    }
}

/// Get latest schema for a subject
async fn get_latest_schema(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(subject): Path<String>,
) -> Result<Json<SubjectVersionResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting latest schema for subject: {}", subject);

    match api.registry.get_latest_schema(&subject).await {
        Ok(schema) => Ok(Json(SubjectVersionResponse {
            subject: schema.subject,
            id: schema.id,
            version: schema.version,
            schema: schema.schema,
            schema_type: Some(format!("{:?}", schema.schema_type).to_uppercase()),
            references: if schema.references.is_empty() {
                None
            } else {
                Some(schema.references)
            },
        })),
        Err(e) => {
            error!("Failed to get latest schema for {}: {:?}", subject, e);
            Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error_code: 40401,
                    message: format!("Subject '{}' not found", subject),
                }),
            ))
        }
    }
}

/// Get specific version of a schema
async fn get_schema_by_version(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path((subject, version)): Path<(String, String)>,
) -> Result<Json<SubjectVersionResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting schema version {} for subject: {}", version, subject);

    let version_num = match version.as_str() {
        "latest" => {
            // Get latest version
            match api.registry.get_latest_schema(&subject).await {
                Ok(schema) => return Ok(Json(SubjectVersionResponse {
                    subject: schema.subject,
                    id: schema.id,
                    version: schema.version,
                    schema: schema.schema,
                    schema_type: Some(format!("{:?}", schema.schema_type).to_uppercase()),
                    references: if schema.references.is_empty() {
                        None
                    } else {
                        Some(schema.references)
                    },
                })),
                Err(e) => {
                    error!("Failed to get latest schema for {}: {:?}", subject, e);
                    return Err((
                        StatusCode::NOT_FOUND,
                        Json(ErrorResponse {
                            error_code: 40402,
                            message: format!("Version '{}' not found", version),
                        }),
                    ));
                }
            }
        }
        v => match v.parse::<i32>() {
            Ok(n) => n,
            Err(_) => {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(ErrorResponse {
                        error_code: 42202,
                        message: format!("Invalid version: {}", version),
                    }),
                ));
            }
        }
    };

    match api.registry.get_schema_by_version(&subject, version_num).await {
        Ok(schema) => Ok(Json(SubjectVersionResponse {
            subject: schema.subject,
            id: schema.id,
            version: schema.version,
            schema: schema.schema,
            schema_type: Some(format!("{:?}", schema.schema_type).to_uppercase()),
            references: if schema.references.is_empty() {
                None
            } else {
                Some(schema.references)
            },
        })),
        Err(e) => {
            error!("Failed to get schema version {} for {}: {:?}", version_num, subject, e);
            Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error_code: 40402,
                    message: format!("Version '{}' not found", version),
                }),
            ))
        }
    }
}

/// List all versions for a subject
async fn list_versions(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(subject): Path<String>,
) -> Result<Json<Vec<i32>>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Listing versions for subject: {}", subject);

    let schemas = api.registry.schemas_by_subject.read().await;
    if let Some(subject_schemas) = schemas.get(&subject) {
        let versions: Vec<i32> = subject_schemas.iter().map(|s| s.version).collect();
        Ok(Json(versions))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error_code: 40401,
                message: format!("Subject '{}' not found", subject),
            }),
        ))
    }
}

/// Delete a subject
async fn delete_subject(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(subject): Path<String>,
) -> Result<Json<Vec<i32>>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Deleting subject: {}", subject);

    match api.registry.delete_subject(&subject).await {
        Ok(versions) => {
            info!("Deleted subject {} with versions: {:?}", subject, versions);
            Ok(Json(versions))
        }
        Err(e) => {
            error!("Failed to delete subject {}: {:?}", subject, e);
            Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error_code: 40401,
                    message: format!("Subject '{}' not found", subject),
                }),
            ))
        }
    }
}

/// Delete a specific version of a schema
async fn delete_schema_version(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path((subject, version)): Path<(String, String)>,
) -> Result<Json<i32>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Deleting schema version {} for subject: {}", version, subject);

    // TODO: Implement version deletion
    Err((
        StatusCode::NOT_IMPLEMENTED,
        Json(ErrorResponse {
            error_code: 50101,
            message: "Version deletion not yet implemented".to_string(),
        }),
    ))
}

/// Get schema versions that reference this ID
async fn get_schema_versions(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(id): Path<i32>,
) -> Result<Json<Vec<SubjectVersionResponse>>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting versions for schema ID: {}", id);

    // Find all subjects/versions that use this schema ID
    let mut results = Vec::new();
    let schemas = api.registry.schemas_by_subject.read().await;

    for (_, subject_schemas) in schemas.iter() {
        for schema in subject_schemas {
            if schema.id == id {
                results.push(SubjectVersionResponse {
                    subject: schema.subject.clone(),
                    id: schema.id,
                    version: schema.version,
                    schema: schema.schema.clone(),
                    schema_type: Some(format!("{:?}", schema.schema_type).to_uppercase()),
                    references: if schema.references.is_empty() {
                        None
                    } else {
                        Some(schema.references.clone())
                    },
                });
            }
        }
    }

    if results.is_empty() {
        Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error_code: 40403,
                message: format!("Schema ID {} not found", id),
            }),
        ))
    } else {
        Ok(Json(results))
    }
}

/// Test compatibility of a schema
async fn test_compatibility(
    State(_api): State<Arc<SchemaRegistryApi>>,
    Path((subject, version)): Path<(String, String)>,
    Json(request): Json<RegisterSchemaRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Testing compatibility for subject {} version {}", subject, version);

    let schema_type = request.schema_type
        .as_deref()
        .and_then(|s| match s.to_uppercase().as_str() {
            "AVRO" => Some(SchemaType::Avro),
            "JSON" => Some(SchemaType::Json),
            "PROTOBUF" => Some(SchemaType::Protobuf),
            _ => None,
        })
        .unwrap_or_default();

    match _api.registry.test_compatibility(&subject, &request.schema, schema_type).await {
        Ok(is_compatible) => Ok(Json(serde_json::json!({ "is_compatible": is_compatible }))),
        Err(e) => {
            error!("Failed to test compatibility: {:?}", e);
            Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error_code: 42201,
                    message: format!("Compatibility test failed: {:?}", e),
                }),
            ))
        }
    }
}

/// Get global compatibility configuration
async fn get_global_config(
    State(api): State<Arc<SchemaRegistryApi>>,
) -> Result<Json<CompatibilityResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting global compatibility configuration");

    let mode = *api.registry.global_compatibility.read().await;
    Ok(Json(CompatibilityResponse {
        compatibility: format!("{:?}", mode).to_uppercase().replace("_", "_"),
    }))
}

/// Set global compatibility configuration
async fn set_global_config(
    State(api): State<Arc<SchemaRegistryApi>>,
    Json(request): Json<CompatibilityRequest>,
) -> Result<Json<CompatibilityResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Setting global compatibility to: {}", request.compatibility);

    let mode = parse_compatibility_mode(&request.compatibility)?;

    match api.registry.set_global_compatibility(mode).await {
        Ok(()) => {
            info!("Set global compatibility to {:?}", mode);
            Ok(Json(CompatibilityResponse {
                compatibility: request.compatibility,
            }))
        }
        Err(e) => {
            error!("Failed to set global compatibility: {:?}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error_code: 50001,
                    message: "Failed to set compatibility".to_string(),
                }),
            ))
        }
    }
}

/// Get subject compatibility configuration
async fn get_subject_config(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(subject): Path<String>,
) -> Result<Json<CompatibilityResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting compatibility configuration for subject: {}", subject);

    match api.registry.get_compatibility(&subject).await {
        Ok(mode) => Ok(Json(CompatibilityResponse {
            compatibility: format!("{:?}", mode).to_uppercase().replace("_", "_"),
        })),
        Err(e) => {
            error!("Failed to get compatibility for {}: {:?}", subject, e);
            Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error_code: 40401,
                    message: format!("Subject '{}' not found", subject),
                }),
            ))
        }
    }
}

/// Set subject compatibility configuration
async fn set_subject_config(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(subject): Path<String>,
    Json(request): Json<CompatibilityRequest>,
) -> Result<Json<CompatibilityResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Setting compatibility for subject {} to: {}", subject, request.compatibility);

    let mode = parse_compatibility_mode(&request.compatibility)?;

    match api.registry.set_compatibility(subject.clone(), mode).await {
        Ok(()) => {
            info!("Set compatibility for {} to {:?}", subject, mode);
            Ok(Json(CompatibilityResponse {
                compatibility: request.compatibility,
            }))
        }
        Err(e) => {
            error!("Failed to set compatibility for {}: {:?}", subject, e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error_code: 50001,
                    message: "Failed to set compatibility".to_string(),
                }),
            ))
        }
    }
}

/// Delete subject compatibility configuration
async fn delete_subject_config(
    State(api): State<Arc<SchemaRegistryApi>>,
    Path(subject): Path<String>,
) -> Result<Json<CompatibilityResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Deleting compatibility configuration for subject: {}", subject);

    // Reset to global default
    let global_mode = *api.registry.global_compatibility.read().await;

    match api.registry.set_compatibility(subject.clone(), global_mode).await {
        Ok(()) => {
            info!("Reset compatibility for {} to global default", subject);
            Ok(Json(CompatibilityResponse {
                compatibility: format!("{:?}", global_mode).to_uppercase().replace("_", "_"),
            }))
        }
        Err(e) => {
            error!("Failed to reset compatibility for {}: {:?}", subject, e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error_code: 50001,
                    message: "Failed to reset compatibility".to_string(),
                }),
            ))
        }
    }
}

/// Mode endpoints (for import/export mode)
async fn get_mode(
    State(_api): State<Arc<SchemaRegistryApi>>,
) -> impl IntoResponse {
    Json(serde_json::json!({ "mode": "READWRITE" }))
}

async fn set_mode(
    State(_api): State<Arc<SchemaRegistryApi>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let mode = body.get("mode").and_then(|m| m.as_str()).unwrap_or("READWRITE");
    Json(serde_json::json!({ "mode": mode }))
}

async fn get_subject_mode(
    State(_api): State<Arc<SchemaRegistryApi>>,
    Path(_subject): Path<String>,
) -> impl IntoResponse {
    Json(serde_json::json!({ "mode": "READWRITE" }))
}

async fn set_subject_mode(
    State(_api): State<Arc<SchemaRegistryApi>>,
    Path(_subject): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let mode = body.get("mode").and_then(|m| m.as_str()).unwrap_or("READWRITE");
    Json(serde_json::json!({ "mode": mode }))
}

async fn delete_subject_mode(
    State(_api): State<Arc<SchemaRegistryApi>>,
    Path(_subject): Path<String>,
) -> impl IntoResponse {
    Json(serde_json::json!({ "mode": "READWRITE" }))
}

// Helper functions

fn parse_compatibility_mode(mode_str: &str) -> Result<CompatibilityMode, (StatusCode, Json<ErrorResponse>)> {
    match mode_str.to_uppercase().as_str() {
        "NONE" => Ok(CompatibilityMode::None),
        "BACKWARD" => Ok(CompatibilityMode::Backward),
        "FORWARD" => Ok(CompatibilityMode::Forward),
        "FULL" => Ok(CompatibilityMode::Full),
        "BACKWARD_TRANSITIVE" => Ok(CompatibilityMode::BackwardTransitive),
        "FORWARD_TRANSITIVE" => Ok(CompatibilityMode::ForwardTransitive),
        "FULL_TRANSITIVE" => Ok(CompatibilityMode::FullTransitive),
        _ => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse {
                error_code: 42203,
                message: format!("Invalid compatibility level: {}", mode_str),
            }),
        )),
    }
}