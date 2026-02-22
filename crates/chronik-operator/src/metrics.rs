use std::sync::LazyLock;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{error, info};

// --- Global metrics registered with the default prometheus registry ---

static RECONCILIATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        Opts::new(
            "chronik_operator_reconciliations_total",
            "Total number of reconciliations",
        ),
        &["controller", "result"],
    )
    .expect("metric can be created");
    prometheus::register(Box::new(counter.clone())).expect("metric can be registered");
    counter
});

static RECONCILE_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    let hist = HistogramVec::new(
        HistogramOpts::new(
            "chronik_operator_reconciliation_duration_seconds",
            "Duration of reconciliation in seconds",
        ),
        &["controller"],
    )
    .expect("metric can be created");
    prometheus::register(Box::new(hist.clone())).expect("metric can be registered");
    hist
});

static LEADER_STATUS: LazyLock<IntGauge> = LazyLock::new(|| {
    let gauge = IntGauge::new(
        "chronik_operator_leader",
        "Whether this instance is the leader (1=leader, 0=standby)",
    )
    .expect("metric can be created");
    prometheus::register(Box::new(gauge.clone())).expect("metric can be registered");
    gauge
});

/// Record a completed reconciliation.
pub fn record_reconciliation(controller: &str, result: &str) {
    RECONCILIATIONS
        .with_label_values(&[controller, result])
        .inc();
}

/// Observe reconciliation duration.
pub fn observe_reconcile_duration(controller: &str, duration_secs: f64) {
    RECONCILE_DURATION
        .with_label_values(&[controller])
        .observe(duration_secs);
}

/// Update the leader gauge.
pub fn set_leader(is_leader: bool) {
    LEADER_STATUS.set(if is_leader { 1 } else { 0 });
}

/// Encode all registered metrics as Prometheus text format.
fn encode_metrics() -> Result<Vec<u8>, String> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder
        .encode(&metric_families, &mut buffer)
        .map_err(|e| format!("Failed to encode metrics: {e}"))?;
    Ok(buffer)
}

/// Start the metrics and health HTTP server.
///
/// Serves:
/// - `GET /metrics` — Prometheus metrics
/// - `GET /healthz` — Liveness probe (always 200)
/// - `GET /readyz`  — Readiness probe (always 200)
pub async fn serve(addr: String) {
    let listener = TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        panic!("Failed to bind metrics server to {addr}: {e}");
    });
    info!("Metrics server listening on {addr}");

    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let request = String::from_utf8_lossy(&buf[..n]);

                    // Parse the request path from the first line.
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");

                    let (status, content_type, body) = match path {
                        "/metrics" => match encode_metrics() {
                            Ok(data) => {
                                ("200 OK", "text/plain; version=0.0.4; charset=utf-8", data)
                            }
                            Err(e) => ("500 Internal Server Error", "text/plain", e.into_bytes()),
                        },
                        "/healthz" => ("200 OK", "text/plain", b"ok".to_vec()),
                        "/readyz" => ("200 OK", "text/plain", b"ok".to_vec()),
                        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
                    };

                    let header = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );

                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
            Err(e) => {
                error!("Failed to accept metrics connection: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_reconciliation() {
        record_reconciliation("standalone", "success");
        record_reconciliation("standalone", "error");
        record_reconciliation("cluster", "success");

        let val = RECONCILIATIONS
            .with_label_values(&["standalone", "success"])
            .get();
        assert!(val >= 1);
    }

    #[test]
    fn test_observe_duration() {
        observe_reconcile_duration("standalone", 0.5);
        observe_reconcile_duration("standalone", 1.2);

        let count = RECONCILE_DURATION
            .with_label_values(&["standalone"])
            .get_sample_count();
        assert!(count >= 2);
    }

    #[test]
    fn test_leader_gauge() {
        set_leader(true);
        assert_eq!(LEADER_STATUS.get(), 1);
        set_leader(false);
        assert_eq!(LEADER_STATUS.get(), 0);
    }

    #[test]
    fn test_encode_metrics() {
        // Ensure metrics can be encoded without error.
        let result = encode_metrics();
        assert!(result.is_ok());
        let body = result.unwrap();
        assert!(!body.is_empty());
    }
}
