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

fn require_edge_index(
    state: &UnifiedApiState,
) -> Result<Arc<chronik_ontology::RelationshipIndex>, ApiError> {
    state.edge_index.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "edge index is not enabled on this server (set CHRONIK_ONTOLOGY_ENABLED=true)",
            )),
        )
    })
}

// ───────────────────────── neighbors (O-1 edge index) ─────────────────────────
//
// Forward AND reverse one-hop lookup over the materialized `RelationshipIndex`.
// `direction=incoming` answers "what points AT this node" — the reverse lookup
// the on-demand `/traverse` (BM25 on subject) structurally can't do cheaply.

#[derive(Debug, Deserialize)]
pub struct NeighborsRequest {
    /// Namespace the edges belong to (the record's `namespace` field).
    pub namespace: String,
    /// The node to look up.
    pub node: String,
    /// `outgoing` (default: edges FROM node) or `incoming` (edges TO node).
    #[serde(default)]
    pub direction: Option<String>,
    /// Optional edge-type filter (e.g. `blocked_by`); omit for all edge types.
    #[serde(default)]
    pub edge_type: Option<String>,
    /// Number of hops to walk (default 1). >1 does a cycle-safe BFS over the
    /// materialized index in `direction`; each returned edge carries its depth.
    #[serde(default)]
    pub depth: Option<usize>,
    /// Optional RFC3339 point-in-time (bi-temporal edge validity).
    #[serde(default)]
    pub as_of: Option<String>,
}

pub async fn neighbors(
    State(state): State<UnifiedApiState>,
    Json(req): Json<NeighborsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let index = require_edge_index(&state)?;
    let as_of = parse_as_of(req.as_of.as_deref())?;
    let edge_type = req.edge_type.as_deref();
    let dir = req.direction.as_deref().unwrap_or("outgoing");
    let direction = match dir {
        "outgoing" | "out" | "forward" => chronik_ontology::Direction::Outgoing,
        "incoming" | "in" | "reverse" => chronik_ontology::Direction::Incoming,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "bad_request",
                    format!("direction must be 'outgoing' or 'incoming' (got {other:?})"),
                )),
            ))
        }
    };
    let depth = req.depth.unwrap_or(1).clamp(1, 5);
    // Multi-hop BFS (depth-tagged); depth=1 is a single hop.
    let walked = index.walk(&req.namespace, &req.node, edge_type, direction, depth, as_of);
    let edges: Vec<serde_json::Value> = walked
        .into_iter()
        .map(|(hop, e)| {
            let mut v = serde_json::to_value(&e).unwrap_or(json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("depth".to_string(), json!(hop));
            }
            v
        })
        .collect();
    Ok(Json(json!({
        "namespace": req.namespace,
        "node": req.node,
        "direction": dir,
        "edge_type": req.edge_type,
        "depth": depth,
        "count": edges.len(),
        "edges": edges,
    })))
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

// ───────────────────────── MCP tools surface (O-2) ─────────────────────────
//
// A minimal Model Context Protocol server over a single JSON-RPC endpoint
// (`POST /ontology/v1/mcp`): `initialize`, `tools/list`, `tools/call`. Agents
// invoke the ontology in domain nouns/verbs — objects, links, as-of — without
// touching storage primitives (roadmap O-2). The tools dispatch to the same
// resolution logic as the REST endpoints.

fn mcp_tools_spec() -> serde_json::Value {
    json!([
        {
            "name": "get_object",
            "description": "Resolve one ontology object instance with per-attribute provenance, optionally as-of a point in time.",
            "inputSchema": {"type":"object","required":["namespace","type","id"],"properties":{
                "namespace":{"type":"string"},"type":{"type":"string"},"id":{"type":"string"},
                "as_of":{"type":"string","description":"RFC3339 point-in-time"}}}
        },
        {
            "name": "query_objects",
            "description": "List every instance of an ObjectType in a namespace, each resolved with provenance.",
            "inputSchema": {"type":"object","required":["namespace","type"],"properties":{
                "namespace":{"type":"string"},"type":{"type":"string"},"as_of":{"type":"string"}}}
        },
        {
            "name": "traverse",
            "description": "Follow (from)-[edge_type]->(to) links derived from the fact graph, 1..=3 hops. edge_type='*' follows all outgoing edges.",
            "inputSchema": {"type":"object","required":["namespace","from"],"properties":{
                "namespace":{"type":"string"},"from":{"type":"string"},
                "edge_type":{"type":"string","default":"*"},"depth":{"type":"integer","default":1},
                "as_of":{"type":"string"}}}
        },
        {
            "name": "neighbors",
            "description": "One- or multi-hop links over the materialized edge index, in either direction. direction='incoming' answers 'what points AT node' — the reverse walk /traverse can't do. depth>1 walks the graph.",
            "inputSchema": {"type":"object","required":["namespace","node"],"properties":{
                "namespace":{"type":"string"},"node":{"type":"string"},
                "direction":{"type":"string","enum":["outgoing","incoming"],"default":"outgoing"},
                "edge_type":{"type":"string"},"depth":{"type":"integer","default":1},
                "as_of":{"type":"string"}}}
        },
        {
            "name": "explain",
            "description": "Explain WHY an object holds its values: each attribute with the source event(s) that justify it, a provenance_complete flag, and the object's incoming/outgoing edges.",
            "inputSchema": {"type":"object","required":["namespace","type","id"],"properties":{
                "namespace":{"type":"string"},"type":{"type":"string"},"id":{"type":"string"},
                "as_of":{"type":"string"}}}
        },
        {
            "name": "list_types",
            "description": "List the ObjectTypes registered for a namespace's tenant.",
            "inputSchema": {"type":"object","required":["namespace"],"properties":{"namespace":{"type":"string"}}}
        }
    ])
}

/// Run one MCP tool by name against `arguments`; returns the structured result
/// value (the caller wraps it in MCP `content`).
async fn mcp_run_tool(
    state: &UnifiedApiState,
    name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let registry = state
        .ontology_types
        .clone()
        .ok_or_else(|| "ontology is not enabled (set CHRONIK_ONTOLOGY_ENABLED=true)".to_string())?;
    let ns = args.get("namespace").and_then(|v| v.as_str()).ok_or("missing namespace")?;
    let tenant = tenant_of(ns);
    let as_of = match args.get("as_of").and_then(|v| v.as_str()) {
        None => None,
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| format!("invalid as_of: {e}"))?
                .with_timezone(&chrono::Utc),
        ),
    };
    let http = reqwest::Client::new();
    let api = self_api_base();
    match name {
        "list_types" => Ok(json!({"types": registry.list_for_tenant(tenant)})),
        "get_object" => {
            let ty = registry
                .get(tenant, args.get("type").and_then(|v| v.as_str()).ok_or("missing type")?)
                .ok_or("no such ObjectType")?;
            let id = args.get("id").and_then(|v| v.as_str()).ok_or("missing id")?;
            chronik_ontology::resolve_object(&http, &api, ns, &ty, id, 10_000, as_of)
                .await
                .map_err(|e| e.to_string())?
                .map(|inst| serde_json::to_value(inst).unwrap_or_else(|_| json!({})))
                .ok_or_else(|| format!("no {} instance {id:?}", ty.type_name))
        }
        "query_objects" => {
            let ty = registry
                .get(tenant, args.get("type").and_then(|v| v.as_str()).ok_or("missing type")?)
                .ok_or("no such ObjectType")?;
            let objs = chronik_ontology::query_objects(&http, &api, ns, &ty, 10_000, as_of)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({"count": objs.len(), "objects": objs}))
        }
        "traverse" => {
            let from = args.get("from").and_then(|v| v.as_str()).ok_or("missing from")?;
            let edge_type = args.get("edge_type").and_then(|v| v.as_str()).unwrap_or("*");
            let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let edges = chronik_ontology::traverse(
                &http, &api, "mem.fact", ns, from, edge_type, depth.clamp(1, 3), 10_000, as_of,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(json!({"edges": edges}))
        }
        "explain" => {
            let ty = registry
                .get(tenant, args.get("type").and_then(|v| v.as_str()).ok_or("missing type")?)
                .ok_or("no such ObjectType")?;
            let id = args.get("id").and_then(|v| v.as_str()).ok_or("missing id")?;
            let inst = chronik_ontology::resolve_object(&http, &api, ns, &ty, id, 10_000, as_of)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no {} instance {id:?}", ty.type_name))?;
            let provenance_complete = !inst.attributes.is_empty()
                && inst
                    .attributes
                    .iter()
                    .all(|a| a.provenance.iter().any(|p| !p.offsets.is_empty()));
            let edges = state.edge_index.as_ref().map(|idx| {
                json!({
                    "outgoing": idx.outgoing(ns, id, None, as_of),
                    "incoming": idx.incoming(ns, id, None, as_of),
                })
            });
            Ok(json!({
                "object": serde_json::to_value(&inst).unwrap_or_else(|_| json!({})),
                "provenance_complete": provenance_complete,
                "edges": edges,
            }))
        }
        "neighbors" => {
            let idx = state.edge_index.as_ref().ok_or("edge index not enabled")?;
            let node = args.get("node").and_then(|v| v.as_str()).ok_or("missing node")?;
            let edge_type = args.get("edge_type").and_then(|v| v.as_str());
            let dir = args.get("direction").and_then(|v| v.as_str()).unwrap_or("outgoing");
            let direction = match dir {
                "incoming" | "in" | "reverse" => chronik_ontology::Direction::Incoming,
                _ => chronik_ontology::Direction::Outgoing,
            };
            let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let walked = idx.walk(ns, node, edge_type, direction, depth.clamp(1, 5), as_of);
            let edges: Vec<serde_json::Value> = walked
                .into_iter()
                .map(|(d, e)| {
                    let mut v = serde_json::to_value(&e).unwrap_or_else(|_| json!({}));
                    if let Some(o) = v.as_object_mut() {
                        o.insert("depth".to_string(), json!(d));
                    }
                    v
                })
                .collect();
            Ok(json!({"direction": dir, "count": edges.len(), "edges": edges}))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// `POST /ontology/v1/mcp` — JSON-RPC 2.0 MCP tools endpoint.
pub async fn mcp(
    State(state): State<UnifiedApiState>,
    body: String,
) -> Json<serde_json::Value> {
    let req: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // Notifications (no id) get an empty ack.
    if method.starts_with("notifications/") {
        return Json(json!({}));
    }
    let rpc_ok = |result: serde_json::Value| json!({"jsonrpc":"2.0","id":id,"result":result});
    let rpc_err =
        |code: i64, msg: String| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":msg}});

    match method {
        "initialize" => Json(rpc_ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "chronik-ontology", "version": env!("CARGO_PKG_VERSION")}
        }))),
        "tools/list" => Json(rpc_ok(json!({"tools": mcp_tools_spec()}))),
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match mcp_run_tool(&state, name, &args).await {
                Ok(v) => {
                    let text = serde_json::to_string(&v).unwrap_or_default();
                    Json(rpc_ok(json!({
                        "content": [{"type": "text", "text": text}],
                        "structuredContent": v,
                        "isError": false
                    })))
                }
                // Tool errors are reported in-band per MCP (isError), not as RPC errors.
                Err(e) => Json(rpc_ok(json!({
                    "content": [{"type": "text", "text": e}],
                    "isError": true
                }))),
            }
        }
        "" => Json(rpc_err(-32600, "invalid request".into())),
        other => Json(rpc_err(-32601, format!("method not found: {other}"))),
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

// ───────────────────────── explain (O-2 provenance/lineage) ─────────────────
//
// Resolve an object and surface WHY each attribute holds — the source event(s)
// (topic + offsets) that justify it — plus the object's incoming/outgoing edges
// when the edge index is present. This is the auditability tool: an agent (or a
// human) can see the derivation of every value, and `provenance_complete` is the
// ≥95% cite gate in one boolean.

pub async fn explain(
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
                format!("no ObjectType {:?} registered for tenant {:?}", req.type_name, tenant),
            )),
        ));
    };
    let as_of = parse_as_of(req.as_of.as_deref())?;
    let http = reqwest::Client::new();
    let api_base = self_api_base();
    let max = req.max_facts.unwrap_or(10_000);

    let inst = match chronik_ontology::resolve_object(
        &http, &api_base, &req.namespace, &ty, &req.id, max, as_of,
    )
    .await
    {
        Ok(Some(inst)) => inst,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new(
                    "not_found",
                    format!("no {} instance with id {:?}", req.type_name, req.id),
                )),
            ))
        }
        Err(e) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new("resolve_failed", e.to_string())),
            ))
        }
    };

    // Provenance-first attribute view + the cite-completeness gate.
    let attributes: Vec<serde_json::Value> = inst
        .attributes
        .iter()
        .map(|a| {
            let cited = a.provenance.iter().any(|p| !p.offsets.is_empty());
            json!({
                "name": a.name,
                "values": a.values,
                "sources": a.provenance,
                "cited": cited,
            })
        })
        .collect();
    let provenance_complete = !inst.attributes.is_empty()
        && inst
            .attributes
            .iter()
            .all(|a| a.provenance.iter().any(|p| !p.offsets.is_empty()));

    // Lineage: incoming/outgoing edges from the materialized index, if wired.
    let edges = state.edge_index.as_ref().map(|idx| {
        json!({
            "outgoing": idx.outgoing(&req.namespace, &req.id, None, as_of),
            "incoming": idx.incoming(&req.namespace, &req.id, None, as_of),
        })
    });

    Ok(Json(json!({
        "namespace": inst.namespace,
        "type": inst.type_name,
        "id": inst.id,
        "backing_records": inst.backing_records,
        "attributes": attributes,
        "provenance_complete": provenance_complete,
        "edges": edges,
    })))
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
