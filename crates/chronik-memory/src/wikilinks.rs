//! Wikilink parser for [`crate::schema::ConceptBody`] markdown content
//! (AMS-3.7 path B).
//!
//! Concept pages use `[[entity_id]]` references to link to other concept
//! pages — same convention as Roam Research, Obsidian, Karpathy's LLM Wiki
//! gist (April 2026). The synthesis prompt instructs the model to emit
//! wikilinks when it mentions another entity that has its own concept page;
//! this module extracts those references at write-time so they can be
//! stored in `ConceptBody::links_out` and traversed at recall-time via
//! `RecallBuilder::expand_concepts`.
//!
//! # Format
//!
//! - `[[user:luis]]` — namespace-style entity id (preferred)
//! - `[[andy]]` — bare entity id
//! - `[[place:lapa_lisbon]]` — typed entity id
//! - `[[Andy|the protagonist]]` — display-text alias (the parser keeps
//!   only the entity id before the `|`)
//!
//! Code spans (`` `[[not a link]]` ``) and code fences (\`\`\`...\`\`\`)
//! are not parsed — wikilinks inside them are ignored, matching the
//! markdown convention.
//!
//! # Constraints
//!
//! - Entity ids must be lowercase ascii alphanum + `:` `_` `-`. Other
//!   characters are rejected so we don't accept arbitrary text wrapped
//!   in `[[ ]]` (e.g. `[[a real sentence with spaces]]`).
//! - Empty `[[]]` is ignored.
//! - Whitespace inside the brackets is trimmed; whitespace internal to
//!   the id rejects the link.
//! - Duplicates within a single document are deduplicated; order is
//!   preserved (first occurrence wins).

/// Extract `[[entity_id]]` references from markdown text.
///
/// Returns deduplicated entity ids in the order they first appear.
/// Skips wikilinks inside `` `inline code` `` and ```` ```fenced``` ````
/// code blocks — same precedence rule as standard markdown renderers.
///
/// See module-level docs for accepted entity-id syntax.
pub fn extract_wikilinks(markdown: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let stripped = strip_code_regions(markdown);
    let bytes = stripped.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Find the closing `]]` — but stop if we hit another `[[` before
            // the close, which means the original `[[` was unclosed and the
            // second `[[` is the real opener. Cap the search at 256 bytes so
            // a stray `[[` near the start of a long doc doesn't run away.
            let start = i + 2;
            let max = (start + 256).min(bytes.len());
            match find_close_or_reopen(&stripped[start..max]) {
                Some(CloseScan::Closed(rel)) => {
                    let inner = &stripped[start..start + rel];
                    if let Some(entity) = parse_link_inner(inner) {
                        if seen.insert(entity.clone()) {
                            out.push(entity);
                        }
                    }
                    i = start + rel + 2;
                    continue;
                }
                Some(CloseScan::ReopenedAt(rel)) => {
                    // Original `[[` was unclosed; jump to the new `[[` and
                    // try again from there.
                    i = start + rel;
                    continue;
                }
                None => {
                    // Neither close nor another `[[` within the cap — abandon
                    // this opener, advance one byte.
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

enum CloseScan {
    /// Found `]]` at this relative byte offset.
    Closed(usize),
    /// Found another `[[` before any `]]` — the original opener is unclosed.
    ReopenedAt(usize),
}

/// Scan `s` for the first of either `]]` (close) or `[[` (re-opener), or
/// neither if absent.
fn find_close_or_reopen(s: &str) -> Option<CloseScan> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    for i in 0..bytes.len() - 1 {
        if bytes[i] == b']' && bytes[i + 1] == b']' {
            return Some(CloseScan::Closed(i));
        }
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            return Some(CloseScan::ReopenedAt(i));
        }
    }
    None
}

/// Strip out content inside `` ` `` (single backtick spans) and triple
/// backtick fenced code blocks. We keep the surrounding text but replace
/// each code region with the same length of spaces so byte offsets in the
/// output align with the input — important when the caller wants to map
/// extracted links back to source positions.
fn strip_code_regions(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"```" {
            // Skip ahead to the next ``` (or to end of string if unclosed).
            out.push_str("   ");
            let mut j = i + 3;
            while j + 2 < bytes.len() {
                if &bytes[j..j + 3] == b"```" {
                    break;
                }
                out.push(' ');
                j += 1;
            }
            // Push the closing fence (or whatever's left if unclosed).
            let end = (j + 3).min(bytes.len());
            for _ in j..end {
                out.push(' ');
            }
            i = end;
            continue;
        }
        if bytes[i] == b'`' {
            out.push(' ');
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'`' {
                out.push(' ');
                j += 1;
            }
            if j < bytes.len() {
                out.push(' ');
                j += 1;
            }
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Parse the inner of a `[[...]]` link — drop a `|alias` suffix, validate
/// the remaining entity id, return `None` if rejected.
fn parse_link_inner(inner: &str) -> Option<String> {
    let raw = inner.split('|').next().unwrap_or("").trim();
    if raw.is_empty() {
        return None;
    }
    if !is_valid_entity_id(raw) {
        return None;
    }
    Some(raw.to_string())
}

/// Validate an entity id: lowercase ascii alphanum + `:` `_` `-` only,
/// non-empty, no leading/trailing separator.
fn is_valid_entity_id(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.starts_with(':') || s.starts_with('_') || s.starts_with('-') {
        return false;
    }
    if s.ends_with(':') || s.ends_with('_') || s.ends_with('-') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == ':' || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_wikilinks() {
        let md = "Luis prefers [[user:luis]] and met with [[andy]] last week.";
        let links = extract_wikilinks(md);
        assert_eq!(links, vec!["user:luis", "andy"]);
    }

    #[test]
    fn deduplicates_repeated_links() {
        let md = "[[user:luis]] saw [[andy]]. Then [[user:luis]] left, [[andy]] stayed.";
        let links = extract_wikilinks(md);
        assert_eq!(links, vec!["user:luis", "andy"]);
    }

    #[test]
    fn handles_alias_pipe_syntax() {
        let md = "Met [[andy|Andy Smith]] at the [[place:lapa_lisbon|Lapa neighborhood]].";
        let links = extract_wikilinks(md);
        assert_eq!(links, vec!["andy", "place:lapa_lisbon"]);
    }

    #[test]
    fn rejects_invalid_entity_ids() {
        // Spaces, uppercase, special chars all rejected.
        let md = "[[A real sentence]] [[user-with-uppercase]] [[u$er]] [[]]";
        let links = extract_wikilinks(md);
        // "user-with-uppercase" is actually all lowercase, so it's valid.
        assert_eq!(links, vec!["user-with-uppercase"]);
    }

    #[test]
    fn rejects_leading_or_trailing_separator() {
        let md = "[[:colonstart]] [[underscoreend_]] [[-dashstart]]";
        let links = extract_wikilinks(md);
        assert!(links.is_empty(), "got: {links:?}");
    }

    #[test]
    fn ignores_wikilinks_in_inline_code() {
        let md = "Real link [[user:luis]] but `[[fake_in_code]]` is ignored.";
        let links = extract_wikilinks(md);
        assert_eq!(links, vec!["user:luis"]);
    }

    #[test]
    fn ignores_wikilinks_in_fenced_code() {
        let md = "Real [[user:luis]] before.\n\
\n\
```\n\
This [[fake_in_fence]] is in a code block.\n\
```\n\
\n\
After [[andy]].";
        let links = extract_wikilinks(md);
        assert_eq!(links, vec!["user:luis", "andy"]);
    }

    #[test]
    fn handles_unclosed_link() {
        let md = "Unclosed [[link is dropped, but [[andy]] is fine.";
        let links = extract_wikilinks(md);
        assert_eq!(links, vec!["andy"]);
    }

    #[test]
    fn handles_empty_or_whitespace_inner() {
        let md = "[[]] and [[ ]] both ignored, [[ andy ]] trims.";
        let links = extract_wikilinks(md);
        assert_eq!(links, vec!["andy"]);
    }

    #[test]
    fn accepts_namespace_style_ids() {
        let md = "[[user:luis]], [[place:lapa_lisbon]], [[documentary:our_planet]]";
        let links = extract_wikilinks(md);
        assert_eq!(
            links,
            vec!["user:luis", "place:lapa_lisbon", "documentary:our_planet"]
        );
    }

    #[test]
    fn accepts_digits_in_entity_id() {
        let md = "[[user:luis2]] and [[apt404]]";
        let links = extract_wikilinks(md);
        assert_eq!(links, vec!["user:luis2", "apt404"]);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(extract_wikilinks("").is_empty());
        assert!(extract_wikilinks("plain text no links").is_empty());
    }
}
