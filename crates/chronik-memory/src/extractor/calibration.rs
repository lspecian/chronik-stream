//! Per-`(extractor_version, memory_type)` confidence-calibration multipliers.
//!
//! Models routinely over- or under-report confidence. Once we have a labeled
//! eval set (Phase 2 dogfood), running the extractor + grader yields per-type
//! calibration multipliers that scale raw model confidence into a value that
//! reflects empirical precision. The applied confidence is:
//!
//! ```text
//! applied = raw_confidence * calibration_multiplier(extractor_version, type)
//! ```
//!
//! Multipliers are baked into the SDK release as a `const` table — the SDK
//! does not rely on a server-side config so calibration is reproducible per
//! release. Bumping the prompt version (e.g. `anthropic-v2` → `anthropic-v3`)
//! also bumps the calibration namespace; previously-tuned multipliers don't
//! silently apply to a new prompt.
//!
//! **Default (uncalibrated): 1.0 across the board.** Replace with measured
//! values once dogfood data is in.

use crate::schema::MemoryType;

/// Look up the calibration multiplier for a given extractor version and memory
/// type. Returns 1.0 when no calibration data exists (uncalibrated baseline).
pub fn calibration_multiplier(extractor_version: &str, kind: MemoryType) -> f32 {
    match (extractor_version, kind) {
        // Phase 1 anthropic-v1 — all uncalibrated. v1 won't get calibration
        // tuning since v2 supersedes it for Phase 2 onward.
        ("anthropic-v1", _) => 1.0,

// Phase 2 anthropic-v2 — initial uncalibrated baseline.
        ("anthropic-v2", MemoryType::Fact) => 1.0,
        ("anthropic-v2", MemoryType::Event) => 1.0,
        ("anthropic-v2", MemoryType::Instruction) => 1.0,
        ("anthropic-v2", MemoryType::Task) => 1.0,

        // Phase 2 calibration prompt anthropic-v3 — uncalibrated baseline.
        ("anthropic-v3", MemoryType::Fact) => 1.0,
        ("anthropic-v3", MemoryType::Event) => 1.0,
        ("anthropic-v3", MemoryType::Instruction) => 1.0,
        ("anthropic-v3", MemoryType::Task) => 1.0,

        // Phase 2 actor-symmetry prompt anthropic-v4 — uncalibrated baseline.
        ("anthropic-v4", MemoryType::Fact) => 1.0,
        ("anthropic-v4", MemoryType::Event) => 1.0,
        ("anthropic-v4", MemoryType::Instruction) => 1.0,
        ("anthropic-v4", MemoryType::Task) => 1.0,

        // Phase 2 / 3 concrete-noun prompt anthropic-v5 — uncalibrated baseline.
        ("anthropic-v5", MemoryType::Fact) => 1.0,
        ("anthropic-v5", MemoryType::Event) => 1.0,
        ("anthropic-v5", MemoryType::Instruction) => 1.0,
        ("anthropic-v5", MemoryType::Task) => 1.0,

        // OpenAI / Ollama v2 / v3 / v4 / v5 — same uncalibrated baseline.
        // Provider parity at the calibration level lands once we have eval
        // data for each (provider, prompt) pair.
        ("openai-v1", _) => 1.0,
        ("openai-v2", _) => 1.0,
        ("openai-v3", _) => 1.0,
        ("openai-v4", _) => 1.0,
        ("openai-v5", _) => 1.0,
        ("ollama-v1", _) => 1.0,
        ("ollama-v2", _) => 1.0,
        ("ollama-v3", _) => 1.0,
        ("ollama-v4", _) => 1.0,
        ("ollama-v5", _) => 1.0,

        // Unknown extractor version — pass confidence through unchanged.
        _ => 1.0,
    }
}

/// Apply calibration to a raw confidence, clamping into [0.0, 1.0].
pub fn apply_calibration(
    raw: f32,
    extractor_version: &str,
    kind: MemoryType,
) -> f32 {
    let m = calibration_multiplier(extractor_version, kind);
    (raw * m).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_unity() {
        for kind in [
            MemoryType::Fact,
            MemoryType::Event,
            MemoryType::Instruction,
            MemoryType::Task,
        ] {
            assert!((calibration_multiplier("anthropic-v1", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("anthropic-v2", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("anthropic-v3", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("openai-v3", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("ollama-v3", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("anthropic-v4", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("openai-v4", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("ollama-v4", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("anthropic-v5", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("openai-v5", kind) - 1.0).abs() < 1e-9);
            assert!((calibration_multiplier("ollama-v5", kind) - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn unknown_version_passes_through() {
        assert!(
            (calibration_multiplier("totally-made-up-v99", MemoryType::Fact) - 1.0).abs() < 1e-9
        );
    }

    #[test]
    fn apply_clamps_to_unit_interval() {
        // Even if a future calibration returned a multiplier > 1, the output
        // must stay in [0, 1].
        assert!(apply_calibration(0.95, "anthropic-v2", MemoryType::Fact) >= 0.0);
        assert!(apply_calibration(0.95, "anthropic-v2", MemoryType::Fact) <= 1.0);
        assert!(apply_calibration(1.5, "anthropic-v2", MemoryType::Fact) <= 1.0);
        assert!(apply_calibration(-0.5, "anthropic-v2", MemoryType::Fact) >= 0.0);
    }
}
