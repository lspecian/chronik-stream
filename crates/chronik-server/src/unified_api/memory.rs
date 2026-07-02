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
    AuditEvent, AuthError, Channel, EndpointKind, Memory, MemoryRegistry, MetricEndpoint,
    RateDecision, RateLimiter, RecallResult, SynthesizedAnswer, TenantRegistry, TextGenerator, Turn,
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

/// Pull `(tenant, api_key)` from headers.
///
/// Modes (most-strict last wins):
///   1. **Passthrough** (default): just extract whatever's in the headers.
///   2. **`require_auth=true`**: reject (401) when either header is missing.
///   3. **`tenants` registry populated** (AM-2.5): also validate the
///      tenant exists and the api_key matches.
///
/// Namespace authorization is a separate call ([`authorize_namespace`])
/// because the target namespace lives in the request body, not headers.
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

/// AM-2.5: When a tenant registry is wired into state, validate the request
/// fully: `(tenant_id, api_key)` must match a registered tenant AND that
/// tenant must own `target_namespace`. Returns `Ok(())` when no registry is
/// configured (passthrough mode).
fn authorize_namespace(
    tenants: Option<&Arc<TenantRegistry>>,
    caller_tenant: Option<&str>,
    api_key: Option<&str>,
    target_namespace: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let Some(registry) = tenants else {
        return Ok(()); // passthrough — no registry configured
    };
    match chronik_memory::validate_request(
        registry.as_ref(),
        caller_tenant,
        api_key,
        target_namespace,
    ) {
        Ok(_) => Ok(()),
        Err(AuthError::MissingCredentials) => Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new(
                "unauthorized",
                "X-Tenant-Id and X-API-Key headers are required",
            )),
        )),
        Err(AuthError::UnknownTenant(t)) => Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new(
                "unauthorized",
                format!("unknown tenant {t:?}"),
            )),
        )),
        Err(AuthError::InvalidKey(_)) => Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new("unauthorized", "invalid API key")),
        )),
        Err(AuthError::ForbiddenNamespace(t, ns)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "forbidden",
                format!("tenant {t:?} is not authorized for namespace {ns:?}"),
            )),
        )),
    }
}

/// AM-2.5 rate-limit gate. Returns `Ok(())` when the limiter is not wired
/// (passthrough) or when the caller has no matching tenant (unauthenticated
/// or unknown). Returns `429 Too Many Requests` with a `Retry-After` hint
/// when the token bucket is empty.
fn check_rate_limit(
    state: &UnifiedApiState,
    caller_tenant: Option<&str>,
    kind: EndpointKind,
    size: usize,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let (Some(limiter), Some(tenants)) =
        (state.memory_rate_limiter.as_ref(), state.memory_tenants.as_ref())
    else {
        return Ok(()); // passthrough — no limiter or no tenants configured
    };
    let Some(caller_tid) = caller_tenant else {
        return Ok(()); // passthrough for anonymous callers (auth will reject later)
    };
    let Some(tenant) = tenants.get(caller_tid) else {
        return Ok(()); // unknown tenant — authorize_namespace will 401
    };
    match limiter.try_acquire(&tenant, kind, size) {
        RateDecision::Allowed => Ok(()),
        RateDecision::Denied { retry_after } => {
            let secs = retry_after.as_secs_f64();
            Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse::new(
                    "rate_limited",
                    format!(
                        "tenant {caller_tid:?} exceeded quota; retry after {secs:.2}s"
                    ),
                )),
            ))
        }
    }
}

/// AM-2.5 storage quota gate. Consults the per-tenant
/// [`chronik_memory::StorageTracker`] against
/// [`chronik_memory::TenantQuotas::storage_bytes`]. Returns `Ok(())`
/// when the tracker or tenants registry is not wired (passthrough) or
/// the caller has no matching tenant. Returns `413 Payload Too Large`
/// with a descriptive body when the write would push the tenant over
/// its cap.
fn check_storage_quota(
    state: &UnifiedApiState,
    caller_tenant: Option<&str>,
    estimated_bytes: u64,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let (Some(tracker), Some(tenants)) = (
        state.memory_storage_tracker.as_ref(),
        state.memory_tenants.as_ref(),
    ) else {
        return Ok(());
    };
    let Some(caller_tid) = caller_tenant else {
        return Ok(());
    };
    let Some(tenant) = tenants.get(caller_tid) else {
        return Ok(());
    };
    let limit = tenant.quotas.storage_bytes;
    match tracker.try_reserve(caller_tid, estimated_bytes, limit) {
        chronik_memory::StorageDecision::Allowed => Ok(()),
        chronik_memory::StorageDecision::Denied {
            used_bytes,
            limit_bytes,
            requested_bytes,
        } => Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse::new(
                "storage_quota_exceeded",
                format!(
                    "tenant {caller_tid:?} would exceed storage quota: \
                     used={used_bytes} + requested={requested_bytes} > \
                     limit={limit_bytes}"
                ),
            )),
        )),
    }
}

/// AM-2.5 storage tracker bump. Adds the actual write size to the
/// tenant's counter after a successful produce. No-op when the tracker
/// is not wired.
fn record_storage(state: &UnifiedApiState, caller_tenant: Option<&str>, bytes: u64) {
    if bytes == 0 {
        return;
    }
    if let (Some(tracker), Some(caller_tid)) =
        (state.memory_storage_tracker.as_ref(), caller_tenant)
    {
        tracker.add(caller_tid, bytes);
    }
}

/// AM-2.5 storage-quota byte estimator for ingest.
///
/// Sums `role.len() + content.len() + optional field lens` per turn and
/// adds a small per-turn overhead constant to approximate Kafka framing
/// + JSON envelope. **Estimation, not measurement** — good enough for
/// quota gating; the tracker records the same estimate as "used" so
/// callers see a consistent number.
fn estimate_ingest_bytes(turns: &[TurnInput]) -> u64 {
    const PER_TURN_OVERHEAD: u64 = 128;
    turns
        .iter()
        .map(|t| {
            let base = (t.role.len() + t.content.len()) as u64;
            let channel = t.channel.as_deref().map(|s| s.len() as u64).unwrap_or(0);
            let external_id = t.external_id.as_deref().map(|s| s.len() as u64).unwrap_or(0);
            base + channel + external_id + PER_TURN_OVERHEAD
        })
        .sum()
}

/// AM-2.5 metrics helper. Increments the per-tenant counter (if the
/// registry is wired). Uses `"anonymous"` when the caller sent no
/// `X-Tenant-Id` header. Zero-cost when the registry is None.
fn record_metric(
    state: &UnifiedApiState,
    caller_tenant: Option<&str>,
    endpoint: MetricEndpoint,
    status_code: u16,
) {
    if let Some(metrics) = state.memory_tenant_metrics.as_ref() {
        let tid = caller_tenant.unwrap_or("anonymous");
        metrics.record_status(tid, endpoint, status_code);
    }
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

/// Emit an audit event for an operation. Best-effort — Kafka failure is
/// logged and swallowed so the user-visible response stays clean (AM-2.6).
/// The `caller_tenant` arg is the post-passthrough `X-Tenant-Id` header.
async fn audit_emit(
    mem: &Memory,
    op: &str,
    namespace: &str,
    caller_tenant: Option<&str>,
    status_code: u16,
    error_code: Option<&str>,
    query: Option<&str>,
    memory_ids: Vec<String>,
    latency_ms: u64,
) {
    let mut event = AuditEvent {
        ts: chrono::Utc::now(),
        tenant: mem.tenant().to_string(),
        namespace: namespace.to_string(),
        op: op.to_string(),
        query: None,
        memory_ids: vec![],
        caller_tenant: None,
        status_code,
        error_code: error_code.map(|s| s.to_string()),
        latency_ms,
    };
    if let Some(q) = query {
        event = event.with_query(q);
    }
    if !memory_ids.is_empty() {
        event = event.with_memory_ids(memory_ids);
    }
    if let Some(ct) = caller_tenant {
        event = event.with_caller_tenant(ct);
    }
    if let Err(e) = mem.audit(&event).await {
        // Already logged at warn inside emit_audit; keep this trace cheap.
        tracing::trace!(error = %e, "audit emit failed");
    }
}

// ───────────────────────── INGEST ─────────────────────────

pub async fn ingest(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<IngestRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let started = Instant::now();
    let (caller_tenant, api_key) = extract_auth(&headers, state.memory_require_auth)?;
    authorize_namespace(
        state.memory_tenants.as_ref(),
        caller_tenant.as_deref(),
        api_key.as_deref(),
        &req.namespace,
    )?;
    check_rate_limit(
        &state,
        caller_tenant.as_deref(),
        EndpointKind::Ingest,
        req.turns.len().max(1),
    )?;
    let estimated_bytes = estimate_ingest_bytes(&req.turns);
    check_storage_quota(&state, caller_tenant.as_deref(), estimated_bytes)?;
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

    let n = internal.len();
    let acks = mem.ingest_batch(internal).await.map_err(map_memory_error)?;
    // Charge the tenant only for records that actually landed (dedup skips
    // don't cost storage). Estimated at the average per-turn cost.
    let accepted = acks.iter().filter(|a| !a.deduped).count();
    if accepted > 0 && n > 0 {
        let attributed = estimated_bytes.saturating_mul(accepted as u64) / (n as u64);
        record_storage(&state, caller_tenant.as_deref(), attributed);
    }

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

    audit_emit(
        &mem,
        "ingest",
        &req.namespace,
        caller_tenant.as_deref(),
        202,
        None,
        Some(&format!("turns={n}")),
        vec![],
        started.elapsed().as_millis() as u64,
    )
    .await;
    record_metric(&state, caller_tenant.as_deref(), MetricEndpoint::Ingest, 202);

    Ok((StatusCode::ACCEPTED, Json(response)))
}

// ───────────────────────── REMEMBER ─────────────────────────

pub async fn remember(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<RememberRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let started = Instant::now();
    let (caller_tenant, api_key) = extract_auth(&headers, state.memory_require_auth)?;
    authorize_namespace(
        state.memory_tenants.as_ref(),
        caller_tenant.as_deref(),
        api_key.as_deref(),
        &req.namespace,
    )?;
    check_rate_limit(&state, caller_tenant.as_deref(), EndpointKind::Write, 1)?;
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

    let memory_id = format!("{}@{}", ack.topic, ack.offset);

    audit_emit(
        &mem,
        "remember",
        &req.namespace,
        caller_tenant.as_deref(),
        200,
        None,
        req.key.as_deref(),
        vec![memory_id.clone()],
        started.elapsed().as_millis() as u64,
    )
    .await;
    record_metric(&state, caller_tenant.as_deref(), MetricEndpoint::Remember, 200);

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
    let started = Instant::now();
    let (caller_tenant, api_key) = extract_auth(&headers, state.memory_require_auth)?;
    authorize_namespace(
        state.memory_tenants.as_ref(),
        caller_tenant.as_deref(),
        api_key.as_deref(),
        &req.namespace,
    )?;
    check_rate_limit(&state, caller_tenant.as_deref(), EndpointKind::Write, 1)?;
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

    let target_id = req
        .memory_id
        .clone()
        .or_else(|| req.key.clone())
        .unwrap_or_default();

    audit_emit(
        &mem,
        "forget",
        &req.namespace,
        caller_tenant.as_deref(),
        200,
        None,
        Some(&format!("type={} target={target_id}", req.r#type)),
        vec![target_id],
        started.elapsed().as_millis() as u64,
    )
    .await;
    record_metric(&state, caller_tenant.as_deref(), MetricEndpoint::Forget, 200);

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
    let (caller_tenant, api_key) = extract_auth(&headers, state.memory_require_auth)?;
    authorize_namespace(
        state.memory_tenants.as_ref(),
        caller_tenant.as_deref(),
        api_key.as_deref(),
        &req.namespace,
    )?;
    check_rate_limit(&state, caller_tenant.as_deref(), EndpointKind::Recall, 1)?;
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

    let memory_ids: Vec<String> = results.iter().map(|r| r.memory.memory_id.clone()).collect();
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

    audit_emit(
        &mem,
        "recall",
        &req.namespace,
        caller_tenant.as_deref(),
        200,
        None,
        Some(&req.query),
        memory_ids,
        latency_ms,
    )
    .await;
    record_metric(&state, caller_tenant.as_deref(), MetricEndpoint::Recall, 200);

    Ok(Json(response))
}

/// `GET /memory/v1/metrics` — Prometheus text-format payload.
///
/// Emits `memory_ops_total{tenant, endpoint, status}` counters for every
/// tenant that hit `/memory/v1/*`. When the registry is not wired, returns
/// just the `# HELP` / `# TYPE` headers with no data (so scrapers don't
/// error).
///
/// Cardinality cap is controlled via `CHRONIK_MEMORY_METRICS_CAP` env var
/// on the server (default: unlimited). Read at the server level and
/// passed via [`UnifiedApiState`]; that plumbing is a follow-up — for now
/// the endpoint always emits every tenant.
pub async fn metrics(State(state): State<UnifiedApiState>) -> impl IntoResponse {
    let body = state
        .memory_tenant_metrics
        .as_ref()
        .map(|m| m.format_prometheus(None))
        .unwrap_or_else(|| {
            "# HELP memory_ops_total Total /memory/v1/* operations per tenant.\n\
             # TYPE memory_ops_total counter\n"
                .to_string()
        });
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
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

/// `GET /memory/v1/{memory_id}/source` — provenance walk (AM-2.6).
///
/// Returns the source pointer for `memory_id` from the in-memory
/// [`chronik_memory::MemoryIndex`]. The pointer is
/// `{topic, offsets[], extractor}`; the caller can fetch the actual raw
/// turns via its own Kafka client or via `/_sql SELECT * FROM {topic}`.
///
/// Server-side raw-turn resolution is deliberately deferred — a Kafka
/// consumer-based reader over N offsets is a whole subsystem and not
/// needed for the immediate provenance / compliance use case (which is
/// "prove where this fact came from"). `raw_turns` is left empty.
///
/// Responses:
/// - `503 service_unavailable` — index not wired (server started without
///   `CHRONIK_MEMORY_INDEX_ENABLED=true`).
/// - `404 not_found` — memory_id has not been observed by the index
///   consumer. This is a possibly-transient state; a retry after the
///   consumer catches up may succeed.
/// - `200 OK` — pointer returned.
pub async fn source(
    State(state): State<UnifiedApiState>,
    Path(memory_id): Path<String>,
) -> Result<Json<SourceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let Some(index) = state.memory_index.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "memory-index consumer is not enabled on this server \
                 (set CHRONIK_MEMORY_INDEX_ENABLED=true)",
            )),
        ));
    };
    let src = index.lookup(&memory_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "not_found",
                format!(
                    "memory_id {memory_id:?} not in the index (either unknown \
                     or the consumer has not caught up yet — retry"
                ),
            )),
        )
    })?;
    debug!(%memory_id, "source: pointer returned");
    Ok(Json(SourceResponse {
        memory_id,
        source: SourceRef {
            topic: src.topic.clone(),
            offsets: src.offsets.clone(),
            extractor: src.extractor.clone(),
        },
        raw_turns: Vec::new(),
        extractor: src.extractor,
    }))
}

// ───────────────────────── COMPACT ─────────────────────────

/// `POST /memory/v1/compact` — trigger a synchronous [`CompactionRunner`]
/// pass over the in-memory candidate store.
///
/// Returns `503 service_unavailable` when
/// [`UnifiedApiState::memory_compaction`] is `None` (i.e. the lifecycle
/// controller / compaction runner is not wired in this deployment).
///
/// Returns `500 internal_error` when the pass errors out — typically an
/// embedding-provider outage under the hood; the report body is not
/// returned in that case.
pub async fn compact(
    State(state): State<UnifiedApiState>,
    headers: HeaderMap,
    Json(req): Json<CompactRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let (caller_tenant, _api_key) = extract_auth(&headers, state.memory_require_auth)?;
    let Some(runner) = state.memory_compaction.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "compaction runner is not enabled on this server \
                 (lifecycle controller not wired — set \
                 CHRONIK_MEMORY_LIFECYCLE_ENABLED=true)",
            )),
        ));
    };
    let namespace = req.namespace.as_deref();
    let dry_run = req.dry_run;
    info!(
        caller = ?caller_tenant,
        namespace = ?namespace,
        dry_run,
        "compact: starting synchronous pass"
    );
    let started = Instant::now();
    let report = runner.run(namespace, dry_run).await.map_err(|e| {
        error!(error = %e, "compact: runner errored");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::new(
                "internal_error",
                format!("compaction pass failed: {e}"),
            )),
        )
    })?;
    let elapsed = started.elapsed();
    info!(
        caller = ?caller_tenant,
        namespace = ?namespace,
        dry_run,
        groups_scanned = report.groups_scanned,
        records_scanned = report.records_scanned,
        keeps = report.keeps,
        drops = report.drops,
        supersedes = report.supersedes,
        decide_errors = report.decide_errors,
        emit_errors = report.emit_errors,
        elapsed_ms = elapsed.as_millis() as u64,
        "compact: pass complete"
    );
    Ok(Json(report))
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
