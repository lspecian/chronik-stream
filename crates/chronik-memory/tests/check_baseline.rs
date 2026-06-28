//! AM-1.8: Regression-floor enforcement for LongMemEval-S nightly runs.
//!
//! Reads a "current" metrics JSON (from `eval_longmemeval.rs` output) and
//! diffs against `tests/baselines/longmemeval-s.json`. Fails the test (and
//! therefore CI) when any gated metric drops more than the baseline's
//! `tolerance` below the floor.
//!
//! Run:
//! ```sh
//! LONGMEMEVAL_RESULTS_JSON=/tmp/run.json \
//!   cargo test -p chronik-memory --test check_baseline -- --nocapture
//! ```
//!
//! Baseline updates: see `docs/agent-memory/regression-protocol.md`.
//! Short version — require an explicit commit-message tag
//! `baseline-update: <reason>` AND a deliberate edit to the baseline JSON.
//! This test does not auto-update; it only checks.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct Baseline {
    #[allow(dead_code)]
    schema_version: u32,
    #[allow(dead_code)]
    benchmark: String,
    tolerance: f64,
    metrics: BTreeMap<String, f64>,
    #[serde(default)]
    #[allow(dead_code)]
    per_category_synth_judge: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct CurrentRun {
    metrics: BTreeMap<String, f64>,
}

fn baseline_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR set by cargo");
    PathBuf::from(manifest)
        .join("tests")
        .join("baselines")
        .join("longmemeval-s.json")
}

fn load_baseline() -> Baseline {
    let path = baseline_path();
    let s = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read baseline {path:?}: {e}"));
    serde_json::from_str(&s)
        .unwrap_or_else(|e| panic!("parse baseline {path:?}: {e}"))
}

fn load_current() -> Option<CurrentRun> {
    let path = std::env::var("LONGMEMEVAL_RESULTS_JSON").ok()?;
    let s = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[check_baseline] LONGMEMEVAL_RESULTS_JSON={path}: read error {e}");
            return None;
        }
    };
    match serde_json::from_str(&s) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[check_baseline] parse {path}: {e}");
            None
        }
    }
}

/// Diff one current metrics map against the baseline.
///
/// Returns a list of `(metric, current, floor, diff)` tuples for every metric
/// that dropped more than `tolerance` below the floor. Missing metrics in
/// `current` are skipped silently (a nightly run that doesn't cover every
/// dimension shouldn't fail the floor check for that reason).
fn regressions(
    baseline: &Baseline,
    current: &CurrentRun,
) -> Vec<(String, f64, f64, f64)> {
    let mut out = Vec::new();
    for (metric, &floor) in &baseline.metrics {
        let Some(&cur) = current.metrics.get(metric) else {
            continue;
        };
        let drop = floor - cur;
        if drop > baseline.tolerance {
            out.push((metric.clone(), cur, floor, drop));
        }
    }
    out
}

#[test]
fn baseline_file_is_loadable() {
    // Always runs — sanity-check the baseline file itself.
    let b = load_baseline();
    assert!(b.tolerance > 0.0, "tolerance must be positive");
    assert!(
        b.metrics.contains_key("synth_judge_rate"),
        "synth_judge_rate must be present in baseline metrics"
    );
}

#[test]
fn current_run_passes_baseline_floor() {
    let Some(current) = load_current() else {
        eprintln!(
            "[check_baseline] LONGMEMEVAL_RESULTS_JSON not set or unreadable; \
             test passes by default. Set the env var to a JSON file produced \
             by eval_longmemeval.rs to enforce the floor."
        );
        return;
    };
    let baseline = load_baseline();
    let regressions = regressions(&baseline, &current);

    if regressions.is_empty() {
        eprintln!(
            "[check_baseline] OK — {} metrics within tolerance ({})",
            baseline.metrics.len(),
            baseline.tolerance
        );
        return;
    }

    // Report every regression before panicking so the CI log shows the full picture.
    eprintln!("[check_baseline] REGRESSIONS:");
    for (metric, cur, floor, drop) in &regressions {
        eprintln!(
            "  {:<25} current={:.4} floor={:.4} drop={:.4} (tolerance={})",
            metric, cur, floor, drop, baseline.tolerance
        );
    }
    panic!(
        "{} metric(s) regressed below the baseline floor by more than {}. \
         If this is intentional (e.g. a deliberate trade-off), update \
         `tests/baselines/longmemeval-s.json` AND include the tag \
         `baseline-update: <reason>` in your commit message. \
         See docs/agent-memory/regression-protocol.md.",
        regressions.len(),
        baseline.tolerance
    );
}

#[test]
fn synthetic_regression_is_flagged() {
    // Unit test for the diff logic — synthesize a regression and confirm
    // `regressions()` reports it.
    let baseline = Baseline {
        schema_version: 1,
        benchmark: "test".into(),
        tolerance: 0.05,
        metrics: BTreeMap::from([
            ("synth_judge_rate".to_string(), 0.556),
            ("raw_judge_rate".to_string(), 0.389),
        ]),
        per_category_synth_judge: BTreeMap::new(),
    };
    let current = CurrentRun {
        metrics: BTreeMap::from([
            ("synth_judge_rate".to_string(), 0.500), // -0.056 → over tolerance
            ("raw_judge_rate".to_string(), 0.370),   // -0.019 → within tolerance
        ]),
    };
    let r = regressions(&baseline, &current);
    assert_eq!(r.len(), 1, "expected exactly one regression: {r:?}");
    assert_eq!(r[0].0, "synth_judge_rate");
    assert!((r[0].3 - 0.056).abs() < 1e-6, "drop should be 0.056: {}", r[0].3);
}

#[test]
fn improvement_is_not_a_regression() {
    let baseline = Baseline {
        schema_version: 1,
        benchmark: "test".into(),
        tolerance: 0.05,
        metrics: BTreeMap::from([("synth_judge_rate".to_string(), 0.556)]),
        per_category_synth_judge: BTreeMap::new(),
    };
    let current = CurrentRun {
        metrics: BTreeMap::from([("synth_judge_rate".to_string(), 0.700)]),
    };
    assert!(
        regressions(&baseline, &current).is_empty(),
        "improvement must not flag as regression"
    );
}

#[test]
fn missing_metric_in_current_is_skipped() {
    // A nightly that didn't run extraction shouldn't fail the floor on
    // extraction_p_at_5. Skip metrics absent from the current run.
    let baseline = Baseline {
        schema_version: 1,
        benchmark: "test".into(),
        tolerance: 0.05,
        metrics: BTreeMap::from([
            ("synth_judge_rate".to_string(), 0.556),
            ("extraction_p_at_5".to_string(), 0.85),
        ]),
        per_category_synth_judge: BTreeMap::new(),
    };
    let current = CurrentRun {
        metrics: BTreeMap::from([("synth_judge_rate".to_string(), 0.560)]),
    };
    assert!(
        regressions(&baseline, &current).is_empty(),
        "missing metrics in current run should not flag as regressions"
    );
}
