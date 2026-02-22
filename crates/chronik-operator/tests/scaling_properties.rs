//! Property-based tests for the scaling decision engine.
//!
//! These tests verify that scaling decisions always satisfy key invariants
//! regardless of input values.

use chronik_operator::crds::autoscaler::{
    ChronikAutoScalerSpec, ClusterRef, ScalingMetric, ScalingMetricType,
};
use chronik_operator::scaling::{
    evaluate_scaling, ClusterMetrics, ScalingAction, StabilizationState,
};

/// Helper to build a spec with given bounds.
fn make_spec(min: i32, max: i32, target_cpu: u64) -> ChronikAutoScalerSpec {
    ChronikAutoScalerSpec {
        cluster_ref: ClusterRef {
            name: "test".into(),
        },
        min_replicas: min,
        max_replicas: max,
        metrics: vec![ScalingMetric {
            metric_type: ScalingMetricType::Cpu,
            target_value: target_cpu,
            tolerance_percent: 10,
        }],
        scale_up_cooldown_secs: 0,
        scale_down_cooldown_secs: 0,
        scale_up_stabilization_count: 1,
        scale_down_stabilization_count: 1,
    }
}

/// INVARIANT: Scaling actions never produce replicas outside [min, max].
#[test]
fn test_scaling_never_exceeds_bounds() {
    for min in [1, 3, 5] {
        for max in [min, min + 1, min + 5, min + 10] {
            for current in [min - 1, min, min + 1, max - 1, max, max + 1] {
                for cpu in [0.0, 10.0, 50.0, 75.0, 90.0, 100.0] {
                    let spec = make_spec(min, max, 70);
                    let metrics = ClusterMetrics {
                        avg_cpu_percent: Some(cpu),
                        ..Default::default()
                    };
                    let mut state = StabilizationState::default();
                    // Warm up stabilization by running multiple cycles.
                    for _ in 0..10 {
                        let action = evaluate_scaling(
                            &metrics, &spec, current, &mut state, None, None, None,
                        );
                        match action {
                            ScalingAction::ScaleUp { desired, .. } => {
                                assert!(
                                    desired <= max,
                                    "ScaleUp to {desired} exceeds max {max} \
                                     (min={min}, current={current}, cpu={cpu})"
                                );
                                assert!(desired >= min, "ScaleUp to {desired} below min {min}");
                            }
                            ScalingAction::ScaleDown { desired, .. } => {
                                assert!(
                                    desired >= min,
                                    "ScaleDown to {desired} below min {min} \
                                     (max={max}, current={current}, cpu={cpu})"
                                );
                                assert!(desired <= max, "ScaleDown to {desired} exceeds max {max}");
                            }
                            ScalingAction::NoChange => {}
                        }
                    }
                }
            }
        }
    }
}

/// INVARIANT: Scale actions are always +1 or -1 (never larger jumps).
#[test]
fn test_scaling_step_size_is_one() {
    for current in [3, 5, 7, 9] {
        for cpu in [10.0, 50.0, 95.0] {
            let spec = make_spec(3, 12, 70);
            let metrics = ClusterMetrics {
                avg_cpu_percent: Some(cpu),
                ..Default::default()
            };
            let mut state = StabilizationState::default();
            for _ in 0..10 {
                let action =
                    evaluate_scaling(&metrics, &spec, current, &mut state, None, None, None);
                match action {
                    ScalingAction::ScaleUp {
                        current: c,
                        desired: d,
                    } => {
                        assert_eq!(d, c + 1, "Scale up should be +1, got {c}->{d}");
                    }
                    ScalingAction::ScaleDown {
                        current: c,
                        desired: d,
                    } => {
                        assert_eq!(d, c - 1, "Scale down should be -1, got {c}->{d}");
                    }
                    ScalingAction::NoChange => {}
                }
            }
        }
    }
}

/// INVARIANT: Within tolerance band, no scaling occurs.
#[test]
fn test_within_tolerance_no_scaling() {
    let spec = make_spec(3, 9, 70); // target=70, tolerance=10%
                                    // Tolerance band: 63 to 77
    for cpu in [63.0, 65.0, 70.0, 75.0, 77.0] {
        let metrics = ClusterMetrics {
            avg_cpu_percent: Some(cpu),
            ..Default::default()
        };
        let mut state = StabilizationState::default();
        for _ in 0..20 {
            let action = evaluate_scaling(&metrics, &spec, 5, &mut state, None, None, None);
            assert!(
                matches!(action, ScalingAction::NoChange),
                "Expected NoChange for cpu={cpu} within tolerance, got {:?}",
                action
            );
        }
    }
}

/// INVARIANT: Cooldown prevents rapid scaling.
#[test]
fn test_cooldown_prevents_rapid_scaling() {
    let mut spec = make_spec(3, 9, 70);
    spec.scale_up_cooldown_secs = 600;
    spec.scale_up_stabilization_count = 1;

    let metrics = ClusterMetrics {
        avg_cpu_percent: Some(95.0), // Way above target
        ..Default::default()
    };
    let mut state = StabilizationState::default();

    // First call with recent scale time should be blocked by cooldown.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let recent_time = (now - 60).to_string(); // 60 seconds ago

    let action = evaluate_scaling(
        &metrics,
        &spec,
        5,
        &mut state,
        Some(&recent_time),
        Some("up"),
        None,
    );
    assert!(
        matches!(action, ScalingAction::NoChange),
        "Expected NoChange during cooldown, got {:?}",
        action
    );
}

/// INVARIANT: No metrics → NoChange.
#[test]
fn test_no_metrics_no_scaling() {
    let spec = make_spec(3, 9, 70);
    let metrics = ClusterMetrics::default(); // All None
    let mut state = StabilizationState::default();

    for _ in 0..10 {
        let action = evaluate_scaling(&metrics, &spec, 5, &mut state, None, None, None);
        assert!(
            matches!(action, ScalingAction::NoChange),
            "Expected NoChange with no metrics"
        );
    }
}

/// INVARIANT: Multiple metric types all contribute to scaling decisions.
#[test]
fn test_multiple_metrics_all_evaluated() {
    let spec = ChronikAutoScalerSpec {
        cluster_ref: ClusterRef {
            name: "test".into(),
        },
        min_replicas: 3,
        max_replicas: 9,
        metrics: vec![
            ScalingMetric {
                metric_type: ScalingMetricType::Cpu,
                target_value: 70,
                tolerance_percent: 10,
            },
            ScalingMetric {
                metric_type: ScalingMetricType::Disk,
                target_value: 80,
                tolerance_percent: 5,
            },
        ],
        scale_up_cooldown_secs: 0,
        scale_down_cooldown_secs: 0,
        scale_up_stabilization_count: 1,
        scale_down_stabilization_count: 1,
    };

    // CPU fine, disk critical → should scale up
    let metrics = ClusterMetrics {
        avg_cpu_percent: Some(50.0),  // Within tolerance
        avg_disk_percent: Some(95.0), // Way above target
        ..Default::default()
    };
    let mut state = StabilizationState::default();

    let action = evaluate_scaling(&metrics, &spec, 5, &mut state, None, None, None);
    assert!(
        matches!(action, ScalingAction::ScaleUp { .. }),
        "Expected ScaleUp when disk is critical, got {:?}",
        action
    );
}
