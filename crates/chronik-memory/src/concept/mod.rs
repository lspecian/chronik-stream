//! Concept-page synthesis (AMS-3.7 path B).
//!
//! Where on-demand synthesis ([`crate::recall::RecallBuilder::synthesize`])
//! fuses memories at query time, concept-page synthesis runs ahead of
//! recall: an entity (`user:luis`, `andy`, `place:lapa_lisbon`) gets a
//! markdown page rolled up from all atomic memories about it, written
//! back to `mem.concept.{tenant}` keyed by `concept:{entity_id}`. The
//! recall layer then inlines those pages above raw memories
//! ([`crate::recall::RecallBuilder::include_concepts`]).
//!
//! # Why pre-synthesis (path B) on top of on-demand (path A)
//!
//! On-demand synthesis is great when retrieval already surfaces the
//! relevant memories — but it can't do anything when retrieval is sparse,
//! and it pays the LLM cost on every query. Pre-synthesis amortises the
//! aggregation cost (one LLM call per entity per regen window) and
//! hardens the answer surface for multi-fact arithmetic ("$720 sum"),
//! temporal arithmetic ("21 days"), and long-tail entity facts that
//! retrieval can miss.
//!
//! # Module layout
//!
//! - [`synthesizer`] — the [`synthesize_concept`] free function: query
//!   atomic memories about an entity, build a synthesis prompt, call the
//!   LLM, parse the wikilinks, return a [`crate::ConceptBody`].
//! - [`templates`] — synthesis prompt templates.

pub mod synthesizer;
pub mod templates;

pub use synthesizer::{synthesize_concept, ConceptSynthesisError};
