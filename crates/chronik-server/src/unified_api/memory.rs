//! Axum handlers for `/memory/v1/*` (AM-1.7).
//!
//! Thin shim over [`chronik_memory`]: parses wire types, looks up a per-namespace
//! `Memory` via [`MemoryRegistry`], dispatches the call, and converts back. No
//! business logic — that lives in the crate.
//!
//! Auth (passthrough in Phase 1, real in AM-2.5): the handlers accept
//! `X-Tenant-Id` and `X-API-Key` headers and pass the tenant through. When the
//! server is started with `CHRONIK_MEMORY_REQUIRE_AUTH=true`, requests missing
//! either header are rejected with 401.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use chronik_memory::{
    schema::{Body, MemoryRecord, MemoryType, Source},
    Channel, MemoryRegistry, RecallResult, SynthesizedAnswer, TextGenerator, Turn,
};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

/// Generate a ULID-shaped batch identifier. We don't depend on the `ulid`
/// crate just for one ID; a 26-char Crockford base32 of `(timestamp_ms || rand)`
/// would round-trip, but for a batch_id we only need uniqueness, not parseability.
/// UUIDv4 is already in scope via the broader workspace.
fn new_batch_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

use super::memory_types::*;
use super::UnifiedApiState;

// ───────────────────────── Auth ─────────────────────────

/// Pull `(tenant, api_key)` from headers. In Phase 1 these are advisory;
/// AM-2.5 will validate against `mem.tenants`. When `require_auth=true`,
/// either missing header yields 401.
fn extract_auth(
    headers: &HeaderMap,
    require_auth: bool,
) -> Result<(Option<String>, Option<String>), (StatusCode, Json<ErrorResponse>)> {
    let tenant = headers
        .get("x-tenant-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let api_key = headers
        .get("x-api-key")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    if require_auth && (tenant.is_none() || api_key.is_none()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new(
                "unauthorized",
                "X-Tenant-Id and X-API-Key headers are required",
            )),
        ));
    }
    Ok((tenant, api_key))
}

fn require_registry(
    state: &UnifiedApiState,
) -> Result<Arc<MemoryRegistry>, (StatusCode, Json<ErrorResponse>)> {
    state.memory_registry.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "agent memory is not enabled on this server \
                 (set CHRONIK_MEMORY_KAFKA + CHRONIK_MEMORY_API)",
            )),
        )
    })
}

fn parse_memory_type(s: &str) -> Result<MemoryType, (StatusCode, Json<ErrorResponse>)> {
    match s {
        "fact" => Ok(MemoryType::Fact),
        "event" => Ok(MemoryType::Event),
        "instruction" => Ok(MemoryType::Instruction),
        "task" => Ok(MemoryType::Task),
        "concept" => Ok(MemoryType::Concept),
        other => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "bad_request",
                format!("unknown memory type {other:?}"),
            )),
        )),
    }
}

fn parse_channel(s: &str) -> Result<Channel, (StatusCode, Json<ErrorResponse>)> {
    match s {
        "bm25" => Ok(Channel::Bm25),
        "vector" => Ok(Channel::Vector),
        "key_match" => Ok(Channel::KeyMatch),
        "hyde" => Ok(Channel::Hyde),
        "sql" => Ok(Channel::Sql),
        other => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "bad_request",
                format!("unknown channel {other:?}"),
            )),
        )),
    }
}

fn map_memory_error(e: chronik_memory::MemoryError) -> (StatusCode, Json<ErrorResponse>) {
    use chronik_memory::MemoryError as ME;
    let (code, status) = match &e {
        ME::Config(_) | ME::InvalidArgument(_) => ("bad_request", StatusCode::BAD_REQUEST),
        ME::Kafka(_) | ME::Http(_) => ("service_unavailable", StatusCode::SERVICE_UNAVAILABLE),
        _ => ("internal", StatusCode::INTERNAL_SERVER_ERROR),
    };
    (status, Json(ErrorResponse::new(code, e.to_string())))
}

// ───────────────────────── INGEST ─────────────────────────

pub async fn ingest(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<IngestRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let _auth = extract_auth(&headers, state.memory_require_auth)?;
    let registry = require_registry(&state)?;

    if req.turns.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("bad_request", "turns must be non-empty")),
        ));
    }

    let mem = registry
        .get_or_create(&req.namespace)
        .await
        .map_err(map_memory_error)?;

    let internal: Vec<Turn> = req
        .turns
        .into_iter()
        .map(|t| Turn {
            role: t.role,
            content: t.content,
            ts: t.ts,
            channel: t.channel,
            external_id: t.external_id,
        })
        .collect();

    let acks = mem.ingest_batch(internal).await.map_err(map_memory_error)?;

    let skipped = acks.iter().filter(|a| a.deduped).count();
    let response = IngestResponse {
        accepted: acks.len() - skipped,
        skipped_duplicates: skipped,
        batch_id: new_batch_id(),
        acks: acks
            .into_iter()
            .map(|a| IngestAckResponse {
                topic: a.topic,
                partition: a.partition,
                offset: a.offset,
                deduped: a.deduped,
            })
            .collect(),
    };
    Ok((StatusCode::ACCEPTED, Json(response)))
}

// ───────────────────────── REMEMBER ─────────────────────────

pub async fn remember(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<RememberRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let _auth = extract_auth(&headers, state.memory_require_auth)?;
    let registry = require_registry(&state)?;

    // Round-trip wire {type, body} → Body enum via serde's tagged repr.
    let body_json = json!({ "type": req.r#type, "body": req.body });
    let body: Body = serde_json::from_value(body_json).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "bad_request",
                format!("body does not match type {:?}: {e}", req.r#type),
            )),
        )
    })?;

    let mem = registry
        .get_or_create(&req.namespace)
        .await
        .map_err(map_memory_error)?;

    let ack = mem
        .remember(body, req.key.clone(), req.confidence)
        .await
        .map_err(map_memory_error)?;

    // The `remember` ack doesn't expose memory_id directly, but the offset is
    // the canonical pointer. Derive a memory_id surrogate from (topic, offset)
    // for the response — callers should treat it as opaque.
    let memory_id = format!("{}@{}", ack.topic, ack.offset);
    Ok((
        StatusCode::OK,
        Json(RememberResponse {
            memory_id,
            topic: ack.topic,
            partition: ack.partition,
            offset: ack.offset,
        }),
    ))
}

// ───────────────────────── FORGET ─────────────────────────

pub async fn forget(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<ForgetRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let _auth = extract_auth(&headers, state.memory_require_auth)?;
    let registry = require_registry(&state)?;
    let kind = parse_memory_type(&req.r#type)?;

    if req.key.is_none() && req.memory_id.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "bad_request",
                "exactly one of `key` or `memory_id` is required",
            )),
        ));
    }

    let mem = registry
        .get_or_create(&req.namespace)
        .await
        .map_err(map_memory_error)?;

    let ack = mem
        .forget(kind, req.key.as_deref(), req.memory_id.as_deref())
        .await
        .map_err(map_memory_error)?;

    Ok(Json(ForgetResponse {
        tombstoned: true,
        topic: ack.topic,
        partition: ack.partition,
        offset: ack.offset,
    }))
}

// ───────────────────────── RECALL ─────────────────────────

pub async fn recall(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<RecallRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let _auth = extract_auth(&headers, state.memory_require_auth)?;
    let registry = require_registry(&state)?;

    let mem = registry
        .get_or_create(&req.namespace)
        .await
        .map_err(map_memory_error)?;

    // Parse channels + types up front so we report 400 cleanly.
    let mut requested_channels: Vec<Channel> = Vec::new();
    if let Some(ref chs) = req.channels {
        for c in chs {
            requested_channels.push(parse_channel(c)?);
        }
    }
    let mut requested_types: Vec<MemoryType> = Vec::new();
    if let Some(ref ts) = req.types {
        for t in ts {
            requested_types.push(parse_memory_type(t)?);
        }
    }

    let mut builder = mem.recall(req.query.clone()).k(req.k);
    if !requested_channels.is_empty() {
        builder = builder.channels(&requested_channels);
    } else {
        // Default channels: BM25 (always on) + Vector. key_match/hyde/sql are opt-in.
        builder = builder.with_vector();
    }
    if !requested_types.is_empty() {
        builder = builder.types(&requested_types);
    }
    if let Some(min_c) = req.min_confidence {
        builder = builder.min_confidence(min_c);
    }
    if let Some(as_of) = req.as_of {
        builder = builder.since(as_of);
    }
    if req.include_concepts {
        builder = builder.include_concepts(true);
    }

    // Optionally attach a text generator if the caller asked for synthesis.
    let synthesize = req.synthesize;
    let generator: Option<Arc<dyn TextGenerator>> = if synthesize {
        state.memory_text_generator.clone()
    } else {
        None
    };

    if synthesize && generator.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "synthesis requested but no text generator is configured \
                 (set CHRONIK_MEMORY_SYNTHESIS_PROVIDER + ANTHROPIC_API_KEY)",
            )),
        ));
    }

    if synthesize {
        builder = builder.with_text_generator(generator.clone().unwrap());
    }

    let started = Instant::now();
    let (results, synth) = if synthesize {
        // synthesize() consumes the builder; it runs send() internally and
        // returns SynthesizedAnswer { answer, abstained, supporting }.
        let answer = builder
            .synthesize(generator.unwrap())
            .await
            .map_err(map_memory_error)?;
        let supporting = answer.supporting.clone();
        (supporting, Some(answer))
    } else {
        let results = builder.send().await.map_err(map_memory_error)?;
        (results, None)
    };
    let latency_ms = started.elapsed().as_millis() as u64;

    let response = RecallResponse {
        results: results.into_iter().map(into_wire_result).collect(),
        synthesis: synth.map(into_wire_synthesis),
        latency_ms,
        // We don't track per-channel execution status yet — echo what was requested.
        channels_executed: requested_channels
            .iter()
            .map(|c| channel_to_str(*c).to_string())
            .collect(),
    };
    Ok(Json(response))
}

fn channel_to_str(c: Channel) -> &'static str {
    match c {
        Channel::Bm25 => "bm25",
        Channel::Vector => "vector",
        Channel::KeyMatch => "key_match",
        Channel::Hyde => "hyde",
        Channel::Sql => "sql",
    }
}

fn into_wire_result(r: RecallResult) -> RecallResultResponse {
    let MemoryRecord {
        memory_id,
        body,
        confidence,
        version,
        valid_from,
        source,
        key,
        ..
    } = r.memory;
    let r#type = body.kind().as_str().to_string();
    let body_json = serde_json::to_value(&body)
        .unwrap_or_else(|_| serde_json::Value::Null);
    // body_json is `{"type": "...", "body": {...}}` — unwrap to just the body.
    let body_inner = body_json
        .get("body")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let channels_hit: Vec<String> = r
        .channels
        .keys()
        .map(|c| channel_to_str(*c).to_string())
        .collect();
    RecallResultResponse {
        memory_id,
        r#type,
        key,
        body: body_inner,
        score: r.score,
        channels_hit,
        confidence,
        version,
        valid_from,
        source: SourceRef {
            topic: source.topic,
            offsets: source.offsets,
            extractor: source.extractor,
        },
    }
}

fn into_wire_synthesis(s: SynthesizedAnswer) -> SynthesisResponse {
    let cited: Vec<String> = s
        .supporting
        .into_iter()
        .map(|r| r.memory.memory_id)
        .collect();
    SynthesisResponse {
        answer: s.answer,
        abstained: s.abstained,
        cited_memory_ids: cited,
    }
}

// ───────────────────────── SOURCE ─────────────────────────

/// `GET /memory/v1/{memory_id}/source` — stub returning 501 in v1.
///
/// Provenance walk requires an index from `memory_id → (topic, offset)`. That
/// index isn't built yet (AM-2.6). The endpoint is wired so the wire surface
/// stays complete; implementation lands as part of Phase 2 audit work.
pub async fn source(
    State(_state): State<UnifiedApiState>,
    Path(memory_id): Path<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    debug!(%memory_id, "source endpoint hit (not implemented)");
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(ErrorResponse::new(
            "not_implemented",
            "provenance walk not yet supported (tracked in AM-2.6)",
        )),
    )
}

// ───────────────────────── FEEDBACK ─────────────────────────

/// `POST /memory/v1/feedback` — minimal v1 impl: log the signal so it's
/// observable, return 200. Full pipeline to `mem.feedback.{tenant}` topic
/// + offline reranker training lands in AM-3.3.
pub async fn feedback(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<FeedbackRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let _auth = extract_auth(&headers, state.memory_require_auth)?;
    let _registry = require_registry(&state)?;
    info!(
        namespace = %req.namespace,
        memory_id = %req.memory_id,
        useful = req.useful,
        used_in_response = req.used_in_response,
        "memory feedback (v1: log-only, AM-3.3 wires this to mem.feedback.*)"
    );
    Ok(Json(FeedbackResponse { recorded: true }))
}

// ───────────────────────── ADMIN ─────────────────────────

pub async fn admin_init_namespace(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<InitNamespaceRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let _auth = extract_auth(&headers, state.memory_require_auth)?;
    let registry = require_registry(&state)?;

    if req.tenant.is_empty() || req.agent.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "bad_request",
                "tenant and agent must be non-empty",
            )),
        ));
    }

    // Derive a canonical namespace path for the (tenant, agent) pair. The full
    // 5-segment form (`tenant:agent:bot:user:luis`) is per-user; for init we
    // provision at `tenant:agent:bot:_:_` and let per-user namespaces share
    // the same typed topics.
    let namespace = format!("{}:{}:bot:_:_", req.tenant, req.agent);
    let mem = registry
        .get_or_create(&namespace)
        .await
        .map_err(map_memory_error)?;

    mem.init_namespace_full().await.map_err(map_memory_error)?;
    let layout = mem.topic_layout();

    // Optionally restrict to specific types (best-effort — init_namespace_full
    // creates everything; we only echo the subset in the response).
    let requested: Option<HashSet<MemoryType>> = req
        .types
        .as_ref()
        .map(|ts| ts.iter().filter_map(|t| parse_memory_type(t).ok()).collect());
    let all_topics = vec![
        (MemoryType::Fact, layout.typed(MemoryType::Fact)),
        (MemoryType::Event, layout.typed(MemoryType::Event)),
        (MemoryType::Instruction, layout.typed(MemoryType::Instruction)),
        (MemoryType::Task, layout.typed(MemoryType::Task)),
    ];
    let topics_created: Vec<String> = all_topics
        .into_iter()
        .filter(|(t, _)| requested.as_ref().map_or(true, |s| s.contains(t)))
        .map(|(_, name)| name)
        .collect();

    Ok(Json(InitNamespaceResponse {
        namespace,
        topics_created,
    }))
}

// ───────────────────────── HEALTH ─────────────────────────

pub async fn health(State(state): State<UnifiedApiState>) -> impl IntoResponse {
    let registry = state.memory_registry.clone();
    let (status, namespaces_cached) = match &registry {
        Some(r) => ("ok", r.len()),
        None => ("disabled", 0),
    };
    let (provider, version) = state
        .memory_text_generator
        .as_ref()
        .map(|_g| (Some("anthropic".to_string()), Some("haiku-4.5".to_string())))
        .unwrap_or((None, None));
    Json(MemoryHealthResponse {
        status: status.to_string(),
        namespaces_cached,
        extractor_provider: provider,
        extractor_version: version,
    })
}

// silence unused warnings on types not surfaced yet
#[allow(dead_code)]
fn _unused_source_marker(_: Source) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_memory_type_round_trip() {
        for t in [
            MemoryType::Fact,
            MemoryType::Event,
            MemoryType::Instruction,
            MemoryType::Task,
            MemoryType::Concept,
        ] {
            let parsed = parse_memory_type(t.as_str()).expect("known type");
            assert_eq!(parsed.as_str(), t.as_str());
        }
    }

    #[test]
    fn parse_memory_type_rejects_unknown() {
        let (status, _) = parse_memory_type("nonsense").unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_channel_known_names() {
        for c in ["bm25", "vector", "key_match", "hyde", "sql"] {
            parse_channel(c).expect("known channel");
        }
        let (status, _) = parse_channel("nope").unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn channel_string_round_trip() {
        for c in [
            Channel::Bm25,
            Channel::Vector,
            Channel::KeyMatch,
            Channel::Hyde,
            Channel::Sql,
        ] {
            let s = channel_to_str(c);
            assert_eq!(parse_channel(s).unwrap(), c);
        }
    }

    #[test]
    fn extract_auth_missing_passthrough_ok() {
        let headers = HeaderMap::new();
        let (tenant, key) = extract_auth(&headers, false).expect("passthrough ok");
        assert!(tenant.is_none());
        assert!(key.is_none());
    }

    #[test]
    fn extract_auth_missing_requires_401() {
        let headers = HeaderMap::new();
        let (status, body) = extract_auth(&headers, true).unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.0.error.code, "unauthorized");
    }

    #[test]
    fn extract_auth_with_headers_returns_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-tenant-id", "acme".parse().unwrap());
        headers.insert("x-api-key", "secret".parse().unwrap());
        let (tenant, key) = extract_auth(&headers, true).expect("ok with both");
        assert_eq!(tenant.as_deref(), Some("acme"));
        assert_eq!(key.as_deref(), Some("secret"));
    }
}
