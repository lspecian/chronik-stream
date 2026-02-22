use axum::{extract::State, http::StatusCode, routing, Json, Router};
use futures_util::future::join_all;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct AppState {
    producer: FutureProducer,
    default_topic: String,
    produced: AtomicU64,
    errors: AtomicU64,
}

#[derive(Deserialize)]
struct ProduceRequest {
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    key: Option<String>,
    value: String,
}

#[derive(Serialize)]
struct ProduceResponse {
    partition: i32,
    offset: i64,
}

#[derive(Serialize)]
struct BatchResponse {
    produced: usize,
    errors: usize,
}

#[derive(Serialize)]
struct MetricsResponse {
    produced: u64,
    errors: u64,
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn metrics(State(state): State<Arc<AppState>>) -> Json<MetricsResponse> {
    Json(MetricsResponse {
        produced: state.produced.load(Ordering::Relaxed),
        errors: state.errors.load(Ordering::Relaxed),
    })
}

async fn metrics_prometheus(State(state): State<Arc<AppState>>) -> (StatusCode, [(String, String); 1], String) {
    let produced = state.produced.load(Ordering::Relaxed);
    let errors = state.errors.load(Ordering::Relaxed);

    let body = format!(
        "# HELP chronik_perf_ingestor_produced_total Total messages produced to Kafka\n\
         # TYPE chronik_perf_ingestor_produced_total counter\n\
         chronik_perf_ingestor_produced_total {produced}\n\
         # HELP chronik_perf_ingestor_errors_total Total produce errors\n\
         # TYPE chronik_perf_ingestor_errors_total counter\n\
         chronik_perf_ingestor_errors_total {errors}\n"
    );

    (
        StatusCode::OK,
        [("content-type".to_string(), "text/plain; version=0.0.4; charset=utf-8".to_string())],
        body,
    )
}

async fn produce(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ProduceRequest>,
) -> Result<Json<ProduceResponse>, StatusCode> {
    let topic = req.topic.as_deref().unwrap_or(&state.default_topic);
    let mut record = FutureRecord::to(topic).payload(req.value.as_bytes());
    let key_str;
    if let Some(k) = &req.key {
        key_str = k.clone();
        record = record.key(key_str.as_bytes());
    }

    match state.producer.send(record, Duration::from_secs(5)).await {
        Ok((partition, offset)) => {
            state.produced.fetch_add(1, Ordering::Relaxed);
            Ok(Json(ProduceResponse { partition, offset }))
        }
        Err((err, _)) => {
            state.errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!("Produce error: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn produce_batch(
    State(state): State<Arc<AppState>>,
    Json(reqs): Json<Vec<ProduceRequest>>,
) -> Json<BatchResponse> {
    let futs: Vec<_> = reqs
        .iter()
        .map(|req| {
            let topic = req.topic.as_deref().unwrap_or(&state.default_topic);
            let mut record = FutureRecord::to(topic).payload(req.value.as_bytes());
            if let Some(k) = &req.key {
                record = record.key(k.as_bytes());
            }
            state.producer.send(record, Duration::from_secs(5))
        })
        .collect();

    // Await all futures concurrently instead of sequentially
    let results = join_all(futs).await;
    let mut produced = 0usize;
    let mut errors = 0usize;
    for result in results {
        match result {
            Ok(_) => produced += 1,
            Err(_) => errors += 1,
        }
    }

    state
        .produced
        .fetch_add(produced as u64, Ordering::Relaxed);
    state.errors.fetch_add(errors as u64, Ordering::Relaxed);

    Json(BatchResponse { produced, errors })
}

/// Fire-and-forget batch endpoint: enqueues messages into rdkafka's buffer
/// and returns immediately without waiting for delivery confirmation.
/// Delivery tracking happens in a background task.
async fn produce_fire_batch(
    State(state): State<Arc<AppState>>,
    Json(reqs): Json<Vec<ProduceRequest>>,
) -> Json<BatchResponse> {
    let count = reqs.len();

    // Move owned data into background task for 'static lifetime
    tokio::spawn(async move {
        // Enqueue all messages — send() copies data into rdkafka's internal buffer
        let futs: Vec<_> = reqs
            .iter()
            .map(|req| {
                let topic = req.topic.as_deref().unwrap_or(&state.default_topic);
                let mut record = FutureRecord::to(topic).payload(req.value.as_bytes());
                if let Some(k) = &req.key {
                    record = record.key(k.as_bytes());
                }
                state.producer.send(record, Duration::from_secs(5))
            })
            .collect();

        // Track delivery results
        let results = join_all(futs).await;
        let mut produced = 0u64;
        let mut errors = 0u64;
        for result in results {
            match result {
                Ok(_) => produced += 1,
                Err(_) => errors += 1,
            }
        }
        state.produced.fetch_add(produced, Ordering::Relaxed);
        state.errors.fetch_add(errors, Ordering::Relaxed);
    });

    // Return immediately — count is "accepted", not "delivered"
    Json(BatchResponse {
        produced: count,
        errors: 0,
    })
}

pub async fn run(bootstrap_servers: String, topic: String, port: u16) -> anyhow::Result<()> {
    let acks = std::env::var("KAFKA_ACKS").unwrap_or_else(|_| "1".to_string());

    tracing::info!(
        bootstrap = %bootstrap_servers,
        topic = %topic,
        port = port,
        acks = %acks,
        "Starting ingestor"
    );

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "10000")
        .set("queue.buffering.max.messages", "1000000")
        .set("queue.buffering.max.kbytes", "1048576")
        .set("batch.num.messages", "10000")
        .set("linger.ms", "5")
        .set("compression.type", "snappy")
        .set("acks", &acks)
        .create()?;

    let state = Arc::new(AppState {
        producer,
        default_topic: topic,
        produced: AtomicU64::new(0),
        errors: AtomicU64::new(0),
    });

    let app = Router::new()
        .route("/health", routing::get(health))
        .route("/metrics", routing::get(metrics))
        .route("/metrics/prometheus", routing::get(metrics_prometheus))
        .route("/produce", routing::post(produce))
        .route("/produce/batch", routing::post(produce_batch))
        .route("/produce/fire/batch", routing::post(produce_fire_batch))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Listening on {addr}");
    axum::Server::bind(&addr.parse()?)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
