//! Concept-page synthesis prompt templates (AMS-3.7 path B).
//!
//! Default template is intentionally markdown — concept pages are stored as
//! markdown text on `ConceptBody::markdown`, and downstream consumers (UI,
//! prompt context, [`crate::recall::RecallBuilder::synthesize`]) can
//! re-format from there. The default template emits:
//!
//! ```text
//! # {title}
//!
//! ## Summary
//! one or two sentences capturing the essence
//!
//! ## Facts
//! - bulleted facts
//!
//! ## Timeline
//! - date / event lines (if any temporal structure exists)
//!
//! ## Related
//! - [[entity_id]] one-line description
//! ```
//!
//! The `[[entity_id]]` syntax is parsed by [`crate::wikilinks::extract_wikilinks`]
//! and stored in `ConceptBody::links_out` so recall can traverse it.

/// Default markdown template instructions for concept synthesis. The
/// synthesizer concatenates this with the entity context + atomic memory
/// list, then passes it to the model. Public so callers can craft custom
/// templates by composition.
pub const DEFAULT_TEMPLATE_INSTRUCTIONS: &str = "\
You are a precise summarizer building a concept page about ONE entity \
in an agent's long-term memory. Read the atomic memories below and \
produce a markdown page that an agent can use as ground-truth context.\n\
\n\
Rules:\n\
- Output VALID markdown. Do not wrap in code fences. No preamble.\n\
- Start with `# {title}` where {title} is a short human-readable name \
for the entity (use the entity_id verbatim if no clear title is in the \
memories).\n\
- Sections (omit any that have no content): `## Summary`, `## Facts`, \
`## Timeline`, `## Related`.\n\
- `## Summary`: 1-2 sentences. The single most important thing to know.\n\
- `## Facts`: bulleted list, each fact one short line. Resolve conflicts \
by preferring the most recent (latest valid_from). Drop redundant \
restatements.\n\
- `## Timeline`: bulleted list of `YYYY-MM-DD — event` lines, ordered \
oldest-first. Only include if 2+ time-anchored events exist.\n\
- `## Related`: bulleted list of `[[entity_id]] — one-line description` \
for OTHER entities mentioned in the memories. Use lowercase \
namespace-style ids: `[[user:luis]]`, `[[andy]]`, `[[place:lapa_lisbon]]`. \
Do NOT include the entity itself; only its links to others.\n\
- When the memories contradict each other (e.g. budget changed from $800K \
to $750K), state both with their dates rather than picking one silently. \
Conflicts are signal, not noise.\n\
- When the memories are sparse (< 3 facts), produce a small page rather \
than padding. A two-line concept page is fine.\n\
- NEVER fabricate. If a section would require information not in the \
memories, omit the section.\n\
";

/// Build the user-message body — context block (entity + tenant) followed
/// by the bulleted memory list. Caller concatenates
/// [`DEFAULT_TEMPLATE_INSTRUCTIONS`] (system or prefix) with the output of
/// this function.
pub fn render_user_message(entity_id: &str, memory_lines: &[String]) -> String {
    let mut s = String::new();
    s.push_str(&format!("Entity: {entity_id}\n"));
    s.push_str("\n");
    if memory_lines.is_empty() {
        s.push_str("(no atomic memories for this entity yet)\n");
    } else {
        s.push_str("Atomic memories about this entity, oldest first:\n");
        for (i, line) in memory_lines.iter().enumerate() {
            s.push_str(&format!("{}. {}\n", i + 1, line));
        }
    }
    s.push_str("\nWrite the concept page now.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_mentions_required_sections() {
        let t = DEFAULT_TEMPLATE_INSTRUCTIONS;
        assert!(t.contains("## Summary"));
        assert!(t.contains("## Facts"));
        assert!(t.contains("## Timeline"));
        assert!(t.contains("## Related"));
        assert!(t.contains("[[entity_id]]"));
    }

    #[test]
    fn user_message_includes_entity_and_memories() {
        let lines = vec![
            "(2026-04-01) user prefers Lapa".to_string(),
            "(2026-04-15) user budget 800k".to_string(),
        ];
        let msg = render_user_message("user:luis", &lines);
        assert!(msg.contains("Entity: user:luis"));
        assert!(msg.contains("user prefers Lapa"));
        assert!(msg.contains("user budget 800k"));
        assert!(msg.contains("Write the concept page"));
    }

    #[test]
    fn user_message_handles_empty_memory_list() {
        let msg = render_user_message("andy", &[]);
        assert!(msg.contains("Entity: andy"));
        assert!(msg.contains("no atomic memories"));
    }
}
