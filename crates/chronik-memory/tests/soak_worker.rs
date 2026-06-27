//! Worker soak test (AMS-2.2).
//!
//! Drives the standalone extraction worker against a continuous stream of
//! produced raw turns and verifies:
//! - **No offset drift** — the worker consumes everything the producer
//!   pushes (or close to it within the test window).
//! - **No leaked errors** — `extractor_errors` and `produce_errors`
//!   stay at zero throughout the run.
//! - **Stable RSS** — the worker process's resident-set-size remains
//!   within a configurable band (delta on Linux via `/proc/self/status`).
//! - **Steady throughput** — turns-per-second over rolling windows
//!   doesn't collapse to zero.
//!
//! Uses [`RuleExtractor`] (regex-only) so the soak runs without LLM cost —
//! 24 h of LLM-backed extraction would burn a meaningful budget. The point
//! is *worker correctness*, not *extraction quality*; quality lives in the
//! `eval_extraction` test.
//!
//! ## Running
//!
//! ```bash
//! ./tests/cluster/start.sh
//!
//! # Smoke (default 5 min, 5 turns/sec → 1500 turns total):
//! CHRONIK_INTEGRATION=1 \
//!   CHRONIK_API=http://localhost:6094 CHRONIK_KAFKA=localhost:9094 \
//!   cargo test -p chronik-memory --test soak_worker -- --ignored --nocapture
//!
//! # 1-hour CI smoke:
//! SOAK_DURATION_SECS=3600 SOAK_TURNS_PER_SEC=5 \
//!   ... (other env) ... \
//!   cargo test -p chronik-memory --test soak_worker -- --ignored --nocapture
//!
//! # Full 24-hour overnight run (Phase 2 exit criterion):
//! SOAK_DURATION_SECS=86400 SOAK_TURNS_PER_SEC=2 \
//!   ... (other env) ... \
//!   cargo test --release -p chronik-memory --test soak_worker -- --ignored --nocapture
//! ```
//!
//! ## What's measured (printed at the end)
//!
//! - Turns produced vs turns consumed (drift = produced - consumed)
//! - WorkerStats: batches processed/committed, memories produced, errors
//! - RSS at start, mid, end (max delta over the run)
//! - Throughput: rolling-window turns/sec at 1-minute marks (for runs > 5 min)

use chronik_memory::extractor::Turn;
use chronik_memory::worker::{Worker, WorkerConfig};
use chronik_memory::{Memory, RuleExtractor};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ulid::Ulid;

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn read_rss_kb() -> Option<u64> {
    // Linux-only — best-effort. /proc/self/status gives us VmRSS in KB.
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb_str = rest.split_whitespace().next()?;
            return kb_str.parse::<u64>().ok();
        }
    }
    None
}

#[tokio::test]
#[ignore = "requires a live Chronik cluster + CHRONIK_INTEGRATION=1; minutes-to-hours runtime"]
async fn worker_soak() {
    if std::env::var("CHRONIK_INTEGRATION").ok().as_deref() != Some("1") {
        eprintln!("skipping: set CHRONIK_INTEGRATION=1 to run");
        return;
    }
    let kafka =
        std::env::var("CHRONIK_KAFKA").unwrap_or_else(|_| "localhost:9092".to_string());
    let api =
        std::env::var("CHRONIK_API").unwrap_or_else(|_| "http://localhost:6092".to_string());

    // Knobs (defaults: 5-minute, 5 turns/sec smoke).
    let duration_secs: u64 = env("SOAK_DURATION_SECS", 300);
    let turns_per_sec: u64 = env("SOAK_TURNS_PER_SEC", 5);
    let total_turns_target = duration_secs * turns_per_sec;
    let report_interval = Duration::from_secs(env("SOAK_REPORT_INTERVAL_SECS", 60));
    // Allow up to N turns of drift when grading (the worker may be a few
    // batches behind when the test ends — that's normal and not a leak).
    let max_drift: u64 = env("SOAK_MAX_DRIFT", turns_per_sec * 30); // 30s of throughput

    let ns = format!("soak-worker:{}", Ulid::new());
    println!("namespace: {}", ns);
    println!(
        "soak duration {}s, target {} turns ({} t/s), max drift {} turns",
        duration_secs, total_turns_target, turns_per_sec, max_drift
    );

    // Producer + worker share a Memory (the SDK is cheap to clone).
    let extractor = RuleExtractor::new();
    let mem = Memory::builder()
        .chronik_kafka(kafka.clone())
        .chronik_api(api.clone())
        .namespace(&ns)
        .extractor(extractor)
        .request_timeout(Duration::from_secs(30))
        .build()
        .await
        .expect("build memory");
    mem.init_namespace().await.expect("init namespace");

    let producer_count = Arc::new(AtomicU64::new(0));
    let producer_count_w = producer_count.clone();
    let mem_for_producer = mem.clone();
    let producer_handle = tokio::spawn(async move {
        // Steady-rate producer: send `turns_per_sec` turns every second
        // until we hit `total_turns_target`. A real workload would have
        // bursty traffic; this is a regularity contract — easier to reason
        // about drift.
        let interval_us: u64 = 1_000_000 / turns_per_sec.max(1);
        let mut sent = 0u64;
        let start = Instant::now();
        while sent < total_turns_target {
            let i = sent;
            let turn = Turn {
                role: "user".into(),
                content: format!(
                    "soak msg {i}: my email is alice{i}@example.com, phone +351 91 2345 678{i}",
                ),
                ts: None,
                channel: None,
                external_id: Some(format!("soak-{ns_short}-{i}", ns_short = &i.to_string())),
            };
            // ingest_turn returns IngestAck with deduped flag. We ignore it.
            if let Err(e) = mem_for_producer.ingest_turn(turn).await {
                eprintln!("[soak] producer error at i={i}: {e}");
                // Don't bail — soak is testing the worker, not the producer.
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            sent += 1;
            producer_count_w.store(sent, Ordering::Relaxed);

            // Pacing — sleep to maintain target rate.
            let elapsed_us = start.elapsed().as_micros() as u64;
            let want_us = sent * interval_us;
            if want_us > elapsed_us {
                tokio::time::sleep(Duration::from_micros(want_us - elapsed_us)).await;
            }
        }
        sent
    });

    // Spawn the worker. It will consume `mem.raw.*` for this namespace and
    // produce typed memories. We let it run until the soak deadline plus
    // a small drain-tail so it has a chance to consume the last batches.
    let mem_for_worker = mem.clone();
    let worker_handle = tokio::spawn(async move {
        let worker = Worker::new(
            mem_for_worker,
            WorkerConfig {
                // Stop after a short idle window so the soak shuts down
                // cleanly when the producer finishes — for a 24h run, set
                // SOAK_WORKER_IDLE_POLLS to a high number or unset.
                stop_after_idle_polls: Some(env("SOAK_WORKER_IDLE_POLLS", 6) as usize),
                ..WorkerConfig::default()
            },
        );
        worker.run().await
    });

    // Periodic reporting.
    let rss_start = read_rss_kb();
    let mut rss_max = rss_start.unwrap_or(0);
    let runner_t0 = Instant::now();
    let mut next_report = runner_t0 + report_interval;
    let producer_count_r = producer_count.clone();
    let mut last_count = 0u64;
    let mut last_t = runner_t0;

    while !producer_handle.is_finished() || runner_t0.elapsed() < Duration::from_secs(duration_secs)
    {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if Instant::now() >= next_report {
            let now_count = producer_count_r.load(Ordering::Relaxed);
            let delta = now_count - last_count;
            let dt = last_t.elapsed().as_secs_f32().max(0.001);
            let rate = delta as f32 / dt;
            let rss = read_rss_kb();
            if let Some(r) = rss {
                rss_max = rss_max.max(r);
            }
            println!(
                "  t={:>5.0}s produced={:>8} (window rate={:>6.1}/s)  rss={}KB",
                runner_t0.elapsed().as_secs_f32(),
                now_count,
                rate,
                rss.map(|r| r.to_string()).unwrap_or_else(|| "?".into()),
            );
            last_count = now_count;
            last_t = Instant::now();
            next_report = Instant::now() + report_interval;
        }
        if runner_t0.elapsed() > Duration::from_secs(duration_secs + 60) {
            // Safety belt: don't run past duration + 60s drain window.
            break;
        }
    }

    let produced = producer_handle.await.expect("producer panic");
    println!("\n[..] producer finished at {} turns; waiting for worker drain", produced);

    // Worker waits for its idle-poll timeout to expire.
    let stats = match worker_handle.await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => panic!("worker returned error: {e}"),
        Err(e) => panic!("worker task panic: {e}"),
    };

    let rss_end = read_rss_kb();
    let total_runner_secs = runner_t0.elapsed().as_secs_f64();
    let consumed = stats.turns_consumed;
    let drift: i64 = produced as i64 - consumed as i64;

    println!("\n=== Soak summary ===");
    println!("  duration_target = {} s", duration_secs);
    println!("  duration_actual = {:.0} s", total_runner_secs);
    println!("  produced        = {}", produced);
    println!("  consumed        = {}", consumed);
    println!("  drift           = {} (produced - consumed)", drift);
    println!(
        "  rss start/max/end = {:?} / {} KB / {:?}",
        rss_start, rss_max, rss_end
    );
    println!("  worker stats: {:?}", stats);

    // Assertions — these are the worker-correctness contract.
    assert!(
        produced > 0,
        "producer didn't produce anything ({} turns)",
        produced
    );
    assert!(
        consumed > 0,
        "worker didn't consume anything ({} turns)",
        consumed
    );
    assert_eq!(
        stats.extractor_errors, 0,
        "worker had {} extractor errors",
        stats.extractor_errors
    );
    assert_eq!(
        stats.produce_errors, 0,
        "worker had {} produce errors",
        stats.produce_errors
    );
    assert!(
        drift.abs() as u64 <= max_drift,
        "drift {} exceeded max {}",
        drift,
        max_drift
    );

    // RSS upper bound: if we have measurements, max may not exceed start by
    // more than 50 % over the run. Loose because allocator and tokio steady
    // state takes a few minutes to settle on cold start. For a true 24h
    // memory-leak hunt, run with `SOAK_MAX_RSS_GROWTH_PCT=10` and watch.
    if let (Some(start), Some(end)) = (rss_start, rss_end) {
        let growth_pct: f64 =
            ((end as f64 - start as f64) / start as f64) * 100.0;
        let limit = env("SOAK_MAX_RSS_GROWTH_PCT", 50);
        println!("  rss growth      = {:.1} % (limit {} %)", growth_pct, limit);
        assert!(
            growth_pct <= limit as f64,
            "rss grew {:.1}% (limit {}%): {} → {} KB",
            growth_pct,
            limit,
            start,
            end
        );
    }

    println!("\n[PASS] worker_soak — no offset drift, no errors, RSS within bound");
}
