//! AM-1.8: Recall latency bench harness.
//!
//! Sustained-load probe against `/memory/v1/recall`. Reports p50 / p95 / p99
//! / max latency at a configurable concurrency level. Replaces the planned
//! k6 script in AM-1.5 (kept pure-Rust so it runs inside `cargo test`).
//!
//! Gated by `CHRONIK_INTEGRATION=1` (needs a live server). Tunable via:
//!
//! - `CHRONIK_MEMORY_TEST_API`     — base URL (default: http://localhost:6092)
//! - `CHRONIK_BENCH_CONCURRENCY`   — VUs (default: 100)
//! - `CHRONIK_BENCH_TOTAL`         — total requests (default: 1000)
//! - `CHRONIK_BENCH_NAMESPACE`     — namespace to query (default: bench:am18:bot:_:_)
//! - `CHRONIK_BENCH_P99_MS_MAX`    — fail if p99 > this (default: 1000)
//! - `CHRONIK_BENCH_ERROR_RATE_MAX` — fail if error rate exceeds (default: 0.001)
//!
//! Run:
//! ```sh
//! CHRONIK_INTEGRATION=1 \
//! CHRONIK_BENCH_TOTAL=2000 \
//! CHRONIK_BENCH_CONCURRENCY=200 \
//!   cargo test -p chronik-memory --test bench_recall_latency -- --nocapture
//! ```
//!
//! Targets (per Testing Strategy in `docs/ROADMAP_AGENT_MEMORY.md`):
//! - Recall: 1000 VUs against `/memory/v1/recall`, p99 < 1s, error rate < 0.1%

use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

fn integration_enabled() -> bool {
    std::env::var("CHRONIK_INTEGRATION").as_deref() == Ok("1")
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn base_url() -> String {
    std::env::var("CHRONIK_MEMORY_TEST_API")
        .unwrap_or_else(|_| "http://localhost:6092".to_string())
}

fn namespace() -> String {
    std::env::var("CHRONIK_BENCH_NAMESPACE")
        .unwrap_or_else(|_| "bench:am18:bot:_:_".to_string())
}

#[derive(Default, Debug)]
struct LatencyStats {
    samples_ms: Vec<u64>,
    errors: u64,
    total_attempts: u64,
}

impl LatencyStats {
    fn percentile(&self, p: f64) -> u64 {
        if self.samples_ms.is_empty() {
            return 0;
        }
        let mut sorted = self.samples_ms.clone();
        sorted.sort_unstable();
        let idx = ((sorted.len() as f64) * p).clamp(0.0, sorted.len() as f64 - 1.0);
        sorted[idx as usize]
    }
    fn max(&self) -> u64 {
        self.samples_ms.iter().copied().max().unwrap_or(0)
    }
    fn error_rate(&self) -> f64 {
        if self.total_attempts == 0 {
            0.0
        } else {
            self.errors as f64 / self.total_attempts as f64
        }
    }
}

#[tokio::test]
async fn bench_recall_latency() {
    if !integration_enabled() {
        eprintln!(
            "skipping bench_recall_latency — set CHRONIK_INTEGRATION=1 \
             (and start a server) to run"
        );
        return;
    }

    let api = base_url();
    let ns = namespace();
    let concurrency = env_u64("CHRONIK_BENCH_CONCURRENCY", 100) as usize;
    let total = env_u64("CHRONIK_BENCH_TOTAL", 1000) as usize;
    let p99_ms_max = env_u64("CHRONIK_BENCH_P99_MS_MAX", 1000);
    let err_rate_max = env_f64("CHRONIK_BENCH_ERROR_RATE_MAX", 0.001);

    eprintln!(
        "[bench_recall_latency] api={api} ns={ns} concurrency={concurrency} total={total} \
         gates: p99<={p99_ms_max}ms err_rate<={err_rate_max}"
    );

    let http = Arc::new(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(concurrency * 2)
            .build()
            .expect("http"),
    );
    let sem = Arc::new(Semaphore::new(concurrency));

    // Seed one memory so the namespace exists and recall has something to chew on.
    // Best-effort: ignore failures (the bench is measuring recall latency, not
    // remember success).
    let _ = http
        .post(format!("{api}/memory/v1/remember"))
        .json(&json!({
            "namespace": ns,
            "type": "fact",
            "key": "bench|seed",
            "body": {
                "subject": "bench",
                "predicate": "seed",
                "object": "ok",
                "text": "bench seed memory for AM-1.8 recall latency probe"
            },
            "confidence": 1.0
        }))
        .send()
        .await;

    // Brief settle so the indexer flips at least once.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let queries = [
        "bench seed",
        "anything",
        "what is the bench",
        "memory probe",
        "AM-1.8 recall latency",
    ];

    let mut handles = Vec::with_capacity(total);
    let started_all = Instant::now();
    for i in 0..total {
        let sem = sem.clone();
        let http = http.clone();
        let api = api.clone();
        let ns = ns.clone();
        let q = queries[i % queries.len()].to_string();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore");
            let started = Instant::now();
            let resp = http
                .post(format!("{api}/memory/v1/recall"))
                .json(&json!({
                    "namespace": ns,
                    "query": q,
                    "k": 5,
                    "channels": ["bm25"]
                }))
                .send()
                .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match resp {
                Ok(r) if r.status().is_success() => Ok(elapsed_ms),
                Ok(r) => Err(format!("HTTP {}", r.status())),
                Err(e) => Err(format!("send: {e}")),
            }
        }));
    }

    let mut stats = LatencyStats::default();
    for h in handles {
        stats.total_attempts += 1;
        match h.await {
            Ok(Ok(ms)) => stats.samples_ms.push(ms),
            Ok(Err(e)) => {
                stats.errors += 1;
                eprintln!("[bench_recall_latency] err: {e}");
            }
            Err(join) => {
                stats.errors += 1;
                eprintln!("[bench_recall_latency] join: {join}");
            }
        }
    }
    let wall_s = started_all.elapsed().as_secs_f64();
    let throughput = (stats.samples_ms.len() as f64) / wall_s.max(1e-9);
    let p50 = stats.percentile(0.50);
    let p95 = stats.percentile(0.95);
    let p99 = stats.percentile(0.99);
    let max = stats.max();
    let err_rate = stats.error_rate();

    eprintln!(
        "[bench_recall_latency] SUMMARY\n  \
         total      = {}\n  \
         successful = {}\n  \
         errors     = {} ({:.3}%)\n  \
         wall       = {:.2}s\n  \
         throughput = {:.1} req/s\n  \
         p50        = {}ms\n  \
         p95        = {}ms\n  \
         p99        = {}ms\n  \
         max        = {}ms",
        stats.total_attempts,
        stats.samples_ms.len(),
        stats.errors,
        err_rate * 100.0,
        wall_s,
        throughput,
        p50,
        p95,
        p99,
        max,
    );

    assert!(
        err_rate <= err_rate_max,
        "error rate {:.4} > {}: bench failed",
        err_rate, err_rate_max
    );
    assert!(
        p99 <= p99_ms_max,
        "p99 = {}ms > {}ms gate",
        p99, p99_ms_max
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_basic_math() {
        let s = LatencyStats {
            samples_ms: vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100],
            errors: 0,
            total_attempts: 10,
        };
        assert_eq!(s.percentile(0.50), 60);
        assert_eq!(s.percentile(0.95), 100);
        assert_eq!(s.max(), 100);
        assert_eq!(s.error_rate(), 0.0);
    }

    #[test]
    fn empty_stats_are_safe() {
        let s = LatencyStats::default();
        assert_eq!(s.percentile(0.99), 0);
        assert_eq!(s.max(), 0);
        assert_eq!(s.error_rate(), 0.0);
    }

    #[test]
    fn error_rate_when_all_fail() {
        let s = LatencyStats {
            samples_ms: vec![],
            errors: 5,
            total_attempts: 5,
        };
        assert_eq!(s.error_rate(), 1.0);
    }
}
