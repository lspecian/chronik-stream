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
    /// Optional point-in-time (RFC3339). Returns the instance as of this time
    /// (`valid_from <= as_of`). Exact only over append-only backing; best-effort
    /// over compacted backing (see chronik_ontology::resolve::filter_as_of).
    #[serde(default)]
    pub as_of: Option<String>,
    /// Optional cap on backing records fetched (default 10_000).
    #[serde(default)]
    pub max_facts: Option<usize>,
}

/// The ObjectType registry is TENANT-keyed (`ont.types.{tenant}`); the tenant is
/// the first ':'-segment of a namespace (a colon-free namespace is its own tenant).
fn tenant_of(namespace: &str) -> &str {
    namespace.split(':').next().unwrap_or(namespace)
}

/// Parse an optional RFC3339 `as_of` timestamp (400 on a bad format).
fn parse_as_of(s: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>, ApiError> {
    match s {
        None => Ok(None),
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| Some(dt.with_timezone(&chrono::Utc)))
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "bad_request",
                        format!("invalid as_of (expected RFC3339): {e}"),
                    )),
                )
            }),
    }
}

// ───────────────────────── query_objects (O-2) ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct QueryObjectsRequest {
    pub namespace: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub as_of: Option<String>,
    #[serde(default)]
    pub max_facts: Option<usize>,
}

/// `POST /ontology/v1/query_objects` — list instances of a type in a namespace,
/// each fully resolved with provenance. (O-2 read tool; filter/aggregate later.)
pub async fn query_objects(
    State(state): State<UnifiedApiState>,
    Json(req): Json<QueryObjectsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = require_ontology(&state)?;
    let tenant = tenant_of(&req.namespace);
    let Some(ty) = registry.get(tenant, &req.type_name) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "not_found",
                format!("no ObjectType {:?} registered for tenant {:?}", req.type_name, tenant),
            )),
        ));
    };
    let as_of = parse_as_of(req.as_of.as_deref())?;
    let http = reqwest::Client::new();
    let api_base = self_api_base();
    let max = req.max_facts.unwrap_or(10_000);
    match chronik_ontology::query_objects(&http, &api_base, &req.namespace, &ty, max, as_of).await {
        Ok(objects) => Ok(Json(json!({
            "namespace": req.namespace,
            "type": req.type_name,
            "count": objects.len(),
            "objects": objects,
        }))),
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("query_failed", e.to_string())),
        )),
    }
}

/// `POST /ontology/v1/get_object` — resolve one instance with provenance.
pub async fn get_object(
    State(state): State<UnifiedApiState>,
    Json(req): Json<GetObjectRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = require_ontology(&state)?;
    let tenant = tenant_of(&req.namespace);
    let Some(ty) = registry.get(tenant, &req.type_name) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "not_found",
                format!(
                    "no ObjectType {:?} registered for tenant {:?}",
                    req.type_name, tenant
                ),
            )),
        ));
    };

    // Parse the optional as-of timestamp.
    let as_of = match req.as_of.as_deref() {
        None => None,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Some(dt.with_timezone(&chrono::Utc)),
            Err(e) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "bad_request",
                        format!("invalid as_of (expected RFC3339): {e}"),
                    )),
                ))
            }
        },
    };

    let http = reqwest::Client::new();
    let api_base = self_api_base();
    let max = req.max_facts.unwrap_or(10_000);
    match chronik_ontology::resolve_object(
        &http,
        &api_base,
        &req.namespace,
        &ty,
        &req.id,
        max,
        as_of,
    )
    .await
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

// ───────────────────────── traverse (O-1) ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct TraverseRequest {
    pub namespace: String,
    /// Root node id (entity subject).
    pub from: String,
    /// Edge type = the fact predicate to follow; "*" = all outgoing edges.
    #[serde(default = "default_edge_type")]
    pub edge_type: String,
    /// Hop count, clamped to 1..=3 (roadmap O-1).
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// Optional point-in-time (RFC3339).
    #[serde(default)]
    pub as_of: Option<String>,
    #[serde(default)]
    pub max_per_hop: Option<usize>,
}
fn default_edge_type() -> String {
    "*".to_string()
}
fn default_depth() -> usize {
    1
}

/// `POST /ontology/v1/traverse` — O-1 Link Types. Follow `(from)--[edge_type]-->`
/// edges derived from the fact projection, 1..=3 hops, each edge provenance-carrying.
pub async fn traverse(
    State(state): State<UnifiedApiState>,
    Json(req): Json<TraverseRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Ontology must be enabled (edges are derived, but keep the subsystem gate
    // consistent with the other endpoints).
    let _ = require_ontology(&state)?;
    let as_of = match req.as_of.as_deref() {
        None => None,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Some(dt.with_timezone(&chrono::Utc)),
            Err(e) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "bad_request",
                        format!("invalid as_of (expected RFC3339): {e}"),
                    )),
                ))
            }
        },
    };
    let depth = req.depth.clamp(1, 3);
    let http = reqwest::Client::new();
    let api_base = self_api_base();
    let max = req.max_per_hop.unwrap_or(10_000);
    match chronik_ontology::traverse(
        &http,
        &api_base,
        "mem.fact",
        &req.namespace,
        &req.from,
        &req.edge_type,
        depth,
        max,
        as_of,
    )
    .await
    {
        Ok(edges) => Ok(Json(json!({
            "from": req.from,
            "namespace": req.namespace,
            "edge_type": req.edge_type,
            "depth": depth,
            "edges": edges,
        }))),
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("traverse_failed", e.to_string())),
        )),
    }
}

/// `GET /ontology/v1/types?namespace=…` — the ObjectTypes registered for a
/// namespace. (Instance listing — `query_objects` — is roadmap phase O-2.)
pub async fn list_types(
    State(state): State<UnifiedApiState>,
    Query(q): Query<ListTypesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = require_ontology(&state)?;
    let tenant = tenant_of(&q.namespace);
    let types = registry.list_for_tenant(tenant);
    Ok(Json(json!({ "namespace": q.namespace, "tenant": tenant, "types": types })))
}
