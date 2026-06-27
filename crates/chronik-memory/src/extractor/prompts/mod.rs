//! Extraction prompts. Keyed by version so we can A/B different prompt revisions
//! without breaking the schema or the extractor identifier.
//!
//! - **v1** (AMS-1.3): facts + events only.
//! - **v2** (AMS-2.1): facts + events + instructions + tasks.
//! - **v3** (AMS-2.8): same shape as v2 + canonicalization rules for predicate
//!   names + tighter type-boundary tie-breakers. Bumped `EXTRACTOR_VERSION`
//!   to `*-v3`.
//! - **v4** (AMS-2.8 follow-up): same shape as v3 + actor-symmetry — facts
//!   can be about ANY entity (user, agent, third-party). Regressed
//!   LongMemEval-S judge_rate 0.389 → 0.222 by diluting user-fact density.
//!   Available opt-in only; **NOT** the default. Bumps to `*-v4`.
//! - **v5** (AMS-2.8 follow-up to v4): keeps v3's user-fact-first priority
//!   but adds explicit **concrete-noun extraction** for named third-party
//!   entities (places, foods, brands, quantities, physical descriptions)
//!   as ADDITIVE — without sacrificing user-fact density. Targets the
//!   extraction-gap items pilots 3-5 left behind (Andy, Roscioli,
//!   Met Museum, 2-3 eggs). Opt-in for measurement; promotion to default
//!   pending a pilot that beats v3's 0.389 baseline. Bumps to `*-v5`.

pub mod v1;
pub mod v2;
pub mod v3;
pub mod v4;
pub mod v5;
