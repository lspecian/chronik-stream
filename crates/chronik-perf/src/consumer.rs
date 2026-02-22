use axum::{extract::State, http::StatusCode, routing, Json, Router};
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::topic_partition_list::TopicPartitionList;
use rdkafka::Message;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct AppState {
    consumed: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    started_at: Instant,
}

#[derive(Serialize)]
struct MetricsResponse {
    consumed: u64,
    bytes: u64,
    errors: u64,
    elapsed_secs: f64,
    rate_msg_per_sec: f64,
    rate_mb_per_sec: f64,
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn metrics_prometheus(State(state): State<Arc<AppState>>) -> (StatusCode, [(String, String); 1], String) {
    let consumed = state.consumed.load(Ordering::Relaxed);
    let bytes = state.bytes.load(Ordering::Relaxed);
    let errors = state.errors.load(Ordering::Relaxed);
    let elapsed = state.started_at.elapsed().as_secs_f64();
    let rate_msg = if elapsed > 0.0 { consumed as f64 / elapsed } else { 0.0 };
    let rate_mb = if elapsed > 0.0 { bytes as f64 / 1_048_576.0 / elapsed } else { 0.0 };

    let body = format!(
        "# HELP chronik_perf_consumer_consumed_total Total messages consumed\n\
         # TYPE chronik_perf_consumer_consumed_total counter\n\
         chronik_perf_consumer_consumed_total {consumed}\n\
         # HELP chronik_perf_consumer_bytes_total Total bytes consumed\n\
         # TYPE chronik_perf_consumer_bytes_total counter\n\
         chronik_perf_consumer_bytes_total {bytes}\n\
         # HELP chronik_perf_consumer_errors_total Total consume errors\n\
         # TYPE chronik_perf_consumer_errors_total counter\n\
         chronik_perf_consumer_errors_total {errors}\n\
         # HELP chronik_perf_consumer_elapsed_seconds Seconds since consumer started\n\
         # TYPE chronik_perf_consumer_elapsed_seconds gauge\n\
         chronik_perf_consumer_elapsed_seconds {elapsed:.1}\n\
         # HELP chronik_perf_consumer_rate_msg_per_sec Current message consumption rate\n\
         # TYPE chronik_perf_consumer_rate_msg_per_sec gauge\n\
         chronik_perf_consumer_rate_msg_per_sec {rate_msg:.2}\n\
         # HELP chronik_perf_consumer_rate_mb_per_sec Current MB/s consumption rate\n\
         # TYPE chronik_perf_consumer_rate_mb_per_sec gauge\n\
         chronik_perf_consumer_rate_mb_per_sec {rate_mb:.4}\n"
    );

    (
        StatusCode::OK,
        [("content-type".to_string(), "text/plain; version=0.0.4; charset=utf-8".to_string())],
        body,
    )
}

async fn metrics(State(state): State<Arc<AppState>>) -> Json<MetricsResponse> {
    let consumed = state.consumed.load(Ordering::Relaxed);
    let bytes = state.bytes.load(Ordering::Relaxed);
    let elapsed = state.started_at.elapsed().as_secs_f64();
    let rate_msg = if elapsed > 0.0 {
        consumed as f64 / elapsed
    } else {
        0.0
    };
    let rate_mb = if elapsed > 0.0 {
        bytes as f64 / 1_048_576.0 / elapsed
    } else {
        0.0
    };

    Json(MetricsResponse {
        consumed,
        bytes,
        errors: state.errors.load(Ordering::Relaxed),
        elapsed_secs: elapsed,
        rate_msg_per_sec: rate_msg,
        rate_mb_per_sec: rate_mb,
    })
}

pub async fn run(
    bootstrap_servers: String,
    topic: String,
    _group: String,
    port: u16,
) -> anyhow::Result<()> {
    tracing::info!(
        bootstrap = %bootstrap_servers,
        topic = %topic,
        port = port,
        "Starting consumer (partition-assign mode)"
    );

    // Use BaseConsumer with manual partition assignment to avoid consumer group protocol issues
    let consumer: BaseConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", &format!("perf-assign-{}", std::process::id()))
        .set("enable.auto.commit", "false")
        .set("fetch.min.bytes", "1")
        .set("fetch.wait.max.ms", "100")
        .set("queued.min.messages", "100000")
        .create()?;

    // Get topic metadata to find partition count
    let metadata = consumer.fetch_metadata(Some(&topic), Duration::from_secs(10))?;
    let topic_metadata = metadata.topics().iter().find(|t| t.name() == topic);
    let partition_count = topic_metadata.map_or(1, |t| t.partitions().len() as i32);
    tracing::info!(partitions = partition_count, "Discovered partitions");

    // Assign all partitions starting from offset 0
    let mut tpl = TopicPartitionList::new();
    for p in 0..partition_count {
        tpl.add_partition_offset(&topic, p, rdkafka::Offset::Beginning)?;
    }
    consumer.assign(&tpl)?;
    tracing::info!("Assigned {} partitions", partition_count);

    let state = Arc::new(AppState {
        consumed: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        started_at: Instant::now(),
    });

    // Background consume loop using blocking poll in a dedicated thread
    let consume_state = state.clone();
    let consume_handle = tokio::task::spawn_blocking(move || {
        let mut last_report = Instant::now();

        loop {
            match consumer.poll(Duration::from_millis(100)) {
                Some(Ok(msg)) => {
                    let payload_len = msg.payload().map_or(0, |p| p.len()) as u64;
                    consume_state.consumed.fetch_add(1, Ordering::Relaxed);
                    consume_state.bytes.fetch_add(payload_len, Ordering::Relaxed);

                    if last_report.elapsed() > Duration::from_secs(10) {
                        let total = consume_state.consumed.load(Ordering::Relaxed);
                        let elapsed = consume_state.started_at.elapsed().as_secs_f64();
                        tracing::info!(
                            total_consumed = total,
                            rate = format!("{:.0} msg/s", total as f64 / elapsed),
                            "Consumer progress"
                        );
                        last_report = Instant::now();
                    }
                }
                Some(Err(e)) => {
                    consume_state.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("Consume error: {e}");
                }
                None => {}
            }
        }
    });

    // HTTP metrics server
    let app = Router::new()
        .route("/health", routing::get(health))
        .route("/metrics", routing::get(metrics))
        .route("/metrics/prometheus", routing::get(metrics_prometheus))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Metrics server listening on {addr}");

    let server = axum::Server::bind(&addr.parse()?)
        .serve(app.into_make_service());

    tokio::select! {
        result = server => result?,
        _ = consume_handle => tracing::warn!("Consumer poll loop ended"),
    }

    Ok(())
}
