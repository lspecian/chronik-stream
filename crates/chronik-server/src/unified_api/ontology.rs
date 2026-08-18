//! Axum handlers for `/ontology/v1/*` — Phase O-0 (Object Types).
//!
//! Thin shim over [`chronik_ontology`]: `get_object` looks up the requested
//! [`chronik_ontology::ObjectType`] in the registry, then resolves an instance
//! from its backing projection (over the Unified API's `/_search`, the same
//! client-to-broker pattern the memory SDK uses). `types` lists the registered
//! types. Conventions mirror `memory.rs`: 503 when the subsystem is unwired,
//! 404 on a miss, 200 with the value.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::memory_types::ErrorResponse;
use super::UnifiedApiState;

type ApiError = (StatusCode, Json<ErrorResponse>);

/// The Unified API base this server self-calls for backing `/_search` (same env
/// as the memory layer; see main.rs `try_create_memory_registry`).
fn self_api_base() -> String {
    std::env::var("CHRONIK_MEMORY_API").unwrap_or_else(|_| "http://127.0.0.1:6092".to_string())
}

fn require_ontology(
    state: &UnifiedApiState,
) -> Result<Arc<chronik_ontology::OntTypeIndex>, ApiError> {
    state.ontology_types.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "ontology is not enabled on this server (set CHRONIK_ONTOLOGY_ENABLED=true)",
            )),
        )
    })
}

// ───────────────────────── get_object ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct GetObjectRequest {
    /// Namespace/tenant — used for both the `ont.types.{namespace}` registry key
    /// and the backing `{topic_prefix}.{namespace}` topic.
    pub namespace: String,
    /// ObjectType name (e.g. "Entity").
    #[serde(rename = "type")]
    pub type_name: String,
    /// Instance id (e.g. the entity's subject).
    pub id: String,
    /// Optional cap on backing records fetched (default 10_000).
    #[serde(default)]
    pub max_facts: Option<usize>,
}

/// `POST /ontology/v1/get_object` — resolve one instance with provenance.
pub async fn get_object(
    State(state): State<UnifiedApiState>,
    Json(req): Json<GetObjectRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = require_ontology(&state)?;
    let Some(ty) = registry.get(&req.namespace, &req.type_name) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "not_found",
                format!(
                    "no ObjectType {:?} registered for namespace {:?}",
                    req.type_name, req.namespace
                ),
            )),
        ));
    };

    let http = reqwest::Client::new();
    let api_base = self_api_base();
    let max = req.max_facts.unwrap_or(10_000);
    match chronik_ontology::resolve_object(&http, &api_base, &req.namespace, &ty, &req.id, max).await
    {
        Ok(Some(inst)) => Ok(Json(serde_json::to_value(inst).unwrap_or_else(|_| json!({})))),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "not_found",
                format!("no {} instance with id {:?}", req.type_name, req.id),
            )),
        )),
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("resolve_failed", e.to_string())),
        )),
    }
}

// ───────────────────────── list types ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListTypesQuery {
    pub namespace: String,
}

/// `GET /ontology/v1/types?namespace=…` — the ObjectTypes registered for a
/// namespace. (Instance listing — `query_objects` — is roadmap phase O-2.)
pub async fn list_types(
    State(state): State<UnifiedApiState>,
    Query(q): Query<ListTypesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = require_ontology(&state)?;
    let types = registry.list_for_tenant(&q.namespace);
    Ok(Json(json!({ "namespace": q.namespace, "types": types })))
}
