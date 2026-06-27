//! Concept-page synthesis (AMS-3.7 path B).
//!
//! [`synthesize_concept`] is the core operation: given an `entity_id`, it
//! recalls atomic memories about that entity, runs them through the LLM
//! with the synthesis template, parses the output's wikilinks, and
//! returns a [`ConceptBody`] ready to be written to `mem.concept.{tenant}`
//! via [`crate::Memory::remember_concept`].
//!
//! The function is intentionally a free function (not a method on
//! [`Memory`]) so the periodic worker can call it on a list of
//! `entity_id`s without holding `&mut Memory`.
//!
//! # Cost & cadence
//!
//! One LLM call per invocation. The worker is responsible for deciding
//! when to call (per-entity threshold: regenerate when N new memories
//! arrive OR M hours since last regen). Calling on every memory write
//! would be expensive and unnecessary — entities mostly accrue
//! incremental facts that don't change the page.

use crate::client::Memory;
use crate::concept::templates;
use crate::embeddings::TextGenerator;
use crate::error::MemoryError;
use crate::recall::RecallResult;
use crate::schema::{Body, ConceptBody, MemoryType};
use crate::wikilinks::extract_wikilinks;
use chrono::Utc;
use std::collections::HashSet;
use std::sync::Arc;

/// Maximum atomic memories to feed the synthesis prompt. Keeps the prompt
/// under ~4KB and prevents one viral entity from paying unbounded LLM
/// cost. Memories beyond this cap are dropped — newest-first ordering
/// preserves the freshest facts.
const MAX_MEMORIES_PER_PROMPT: usize = 30;

/// Per-snippet character cap inside the synthesis prompt (matches the
/// recall-time synthesis prompt). 280 chars is a fact-text + structured
/// fields line; the model rarely needs more to write the page.
const MAX_SNIPPET_CHARS: usize = 280;

/// Errors specific to concept-page synthesis. Wraps `MemoryError` with a
/// few synthesis-specific cases that callers may want to handle
/// distinctly (e.g. retry vs skip).
#[derive(Debug, thiserror::Error)]
pub enum ConceptSynthesisError {
    /// The entity's recall returned no atomic memories — nothing to
    /// synthesize from. Caller should skip this entity (no concept page
    /// is better than a hallucinated one).
    #[error("no atomic memories found for entity_id={0}")]
    NoMemories(String),

    /// The model emitted output that didn't start with the required
    /// `# {title}` heading or otherwise looked malformed. Caller may
    /// retry with a different generator or skip.
    #[error("synthesis output malformed: {0}")]
    Malformed(String),

    /// Underlying [`MemoryError`] (recall failure, generator failure, etc.).
    #[error(transparent)]
    Memory(#[from] MemoryError),
}

/// Synthesize a concept page for `entity_id` against the given memory
/// store.
///
/// Pipeline:
/// 1. Recall up to [`MAX_MEMORIES_PER_PROMPT`] atomic memories whose
///    subject / actor mentions `entity_id`. Uses the same multi-channel
///    fan-out as application code (BM25 + KeyMatch — vector skipped to
///    keep the worker self-contained without an embedder).
/// 2. Render each memory as a one-line bullet (timestamp + typed fields).
/// 3. Compose the synthesis prompt and call the generator.
/// 4. Parse `[[entity_id]]` wikilinks from the output → `links_out`.
/// 5. Return a [`ConceptBody`] with `last_synthesized_at = now`,
///    `source_memory_count = recalled_memories.len()`,
///    `source_extractor_versions = unique versions across the inputs`.
///
/// The caller is responsible for writing the result via
/// [`Memory::remember_concept`] — keeping write separate from compute
/// makes this function easier to dry-run, unit-test, and rate-limit.
pub async fn synthesize_concept(
    memory: &Memory,
    entity_id: &str,
    generator: Arc<dyn TextGenerator>,
) -> std::result::Result<ConceptBody, ConceptSynthesisError> {
    if entity_id.trim().is_empty() {
        return Err(ConceptSynthesisError::Memory(MemoryError::InvalidArgument(
            "entity_id must not be empty".into(),
        )));
    }
    // Use the entity_id as the recall query so BM25 + KeyMatch boost
    // memories whose subject contains the entity. We don't filter by
    // `subject == entity_id` strictly because some facts may mention the
    // entity in `object` or `text` and still be relevant for the page.
    let recall_results = memory
        .recall(entity_id)
        .types(&[
            MemoryType::Fact,
            MemoryType::Event,
            MemoryType::Instruction,
            MemoryType::Task,
        ])
        .with_key_match()
        .k(MAX_MEMORIES_PER_PROMPT)
        .send()
        .await?;

    if recall_results.is_empty() {
        return Err(ConceptSynthesisError::NoMemories(entity_id.to_string()));
    }

    // Order memories oldest-first for the prompt — temporal narratives
    // are easier for the model to assemble in chronological order.
    let mut sorted = recall_results;
    sorted.sort_by(|a, b| a.memory.valid_from.cmp(&b.memory.valid_from));

    let bullets: Vec<String> = sorted
        .iter()
        .take(MAX_MEMORIES_PER_PROMPT)
        .map(render_memory_bullet)
        .collect();

    let prompt = format!(
        "{instructions}\n\n{user}",
        instructions = templates::DEFAULT_TEMPLATE_INSTRUCTIONS,
        user = templates::render_user_message(entity_id, &bullets),
    );

    let raw = generator
        .complete(&prompt)
        .await
        .map_err(ConceptSynthesisError::Memory)?;
    let markdown = raw.trim().to_string();
    if markdown.is_empty() {
        return Err(ConceptSynthesisError::Malformed(
            "model returned empty completion".into(),
        ));
    }

    // Parse the title from the first heading. Tolerant: if the model
    // didn't emit a `# Title` line, fall back to the entity_id.
    let title = parse_title(&markdown).unwrap_or_else(|| entity_id.to_string());

    // Extract wikilinks → links_out, with the entity itself filtered out
    // (a concept page shouldn't link to itself).
    let mut links_out: Vec<String> = extract_wikilinks(&markdown)
        .into_iter()
        .filter(|l| l != entity_id)
        .collect();
    links_out.sort();
    links_out.dedup();

    // Distinct extractor versions across the source memories — useful
    // staleness signal: when the extractor schema changes, re-synthesis
    // gets a new fingerprint and downstream consumers can compare.
    let mut versions: HashSet<String> = HashSet::new();
    for r in &sorted {
        versions.insert(r.memory.source.extractor.clone());
    }
    let mut source_extractor_versions: Vec<String> = versions.into_iter().collect();
    source_extractor_versions.sort();

    let entity_type = infer_entity_type(entity_id);

    Ok(ConceptBody {
        entity_id: entity_id.to_string(),
        entity_type,
        title,
        markdown,
        links_out,
        source_memory_count: sorted.len() as u32,
        source_extractor_versions,
        last_synthesized_at: Utc::now(),
    })
}

/// Render one [`RecallResult`] as a `(YYYY-MM-DD) typed-field summary`
/// line for the synthesis prompt. Same style as the on-demand
/// synthesis renderer in `recall.rs` — keeps the model's two prompt
/// surfaces consistent.
fn render_memory_bullet(r: &RecallResult) -> String {
    let typed = match &r.memory.body {
        Body::Fact(f) => format!(
            "fact subject={} predicate={} object={} text={}",
            f.subject,
            f.predicate,
            serde_json::to_string(&f.object).unwrap_or_default(),
            f.text
        ),
        Body::Event(e) => format!(
            "event actor={} verb={} object={} context={}",
            e.actor,
            e.verb,
            e.object.as_deref().unwrap_or(""),
            e.context.as_deref().unwrap_or("")
        ),
        Body::Instruction(i) => format!(
            "instruction scope={} trigger={} rule={}",
            i.scope, i.trigger, i.rule
        ),
        Body::Task(t) => format!(
            "task title={} state={:?} task_id={}",
            t.title, t.state, t.task_id
        ),
        Body::Concept(c) => format!(
            "concept entity_type={} title={} markdown={}",
            c.entity_type, c.title, c.markdown
        ),
    };
    let truncated: String = typed.chars().take(MAX_SNIPPET_CHARS).collect();
    let date = r.memory.valid_from.format("%Y-%m-%d");
    format!("({date}) {truncated}")
}

/// Parse the `# Title` from the first heading line of the markdown. Only
/// matches a single `#` heading (the template instructs the model to
/// produce exactly one). Returns `None` when no `# ` line exists.
fn parse_title(markdown: &str) -> Option<String> {
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

/// Infer an entity type from the namespace-style id. Cheap classifier:
/// `user:luis` → `user`, `place:lapa_lisbon` → `place`, bare ids
/// fall back to `entity`.
fn infer_entity_type(entity_id: &str) -> String {
    match entity_id.split(':').next() {
        Some(prefix) if !prefix.is_empty() && prefix.contains(|c: char| c.is_ascii_lowercase()) => {
            // namespace-style id like `user:luis` → "user"
            if entity_id.contains(':') {
                prefix.to_string()
            } else {
                "entity".to_string()
            }
        }
        _ => "entity".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_title_extracts_first_heading() {
        let md = "# User: Luis\n\n## Summary\nLikes Lapa.";
        assert_eq!(parse_title(md), Some("User: Luis".to_string()));
    }

    #[test]
    fn parse_title_returns_none_without_heading() {
        let md = "Just text, no heading.";
        assert!(parse_title(md).is_none());
    }

    #[test]
    fn parse_title_skips_h2_and_higher() {
        // `## Summary` is h2, not h1, should be skipped.
        let md = "## Summary\n\n# Real Title\nbody";
        assert_eq!(parse_title(md), Some("Real Title".to_string()));
    }

    #[test]
    fn parse_title_ignores_empty_heading() {
        let md = "# \n\n# Real Title";
        assert_eq!(parse_title(md), Some("Real Title".to_string()));
    }

    #[test]
    fn infer_entity_type_namespace_style() {
        assert_eq!(infer_entity_type("user:luis"), "user");
        assert_eq!(infer_entity_type("place:lapa_lisbon"), "place");
        assert_eq!(infer_entity_type("documentary:our_planet"), "documentary");
    }

    #[test]
    fn infer_entity_type_bare_id_falls_back() {
        assert_eq!(infer_entity_type("andy"), "entity");
        assert_eq!(infer_entity_type("roscioli"), "entity");
    }
}
