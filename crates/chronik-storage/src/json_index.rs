//! Field-level search over JSON record values.
//!
//! A Kafka record value that is a JSON object is indexed a second time into a
//! Tantivy JSON field, so a query on `marker` (or `user.name`) matches the
//! records whose `marker` field holds the term, the way an Elasticsearch
//! `match` on a field does. The raw value is still indexed whole as `value`
//! text; this field adds structure, it does not replace anything.
//!
//! The hot in-memory index and the archived segment index both use this
//! module, so a query means the same thing before and after a record is
//! archived. Archives written before the field existed do not have it; the
//! query builder falls back to full-text on `value` for those.

use tantivy::schema::{IndexRecordOption, JsonObjectOptions, TextFieldIndexing};

use crate::text_analysis::ANALYZER_NAME;

/// Name of the JSON field in every Chronik search schema that has one.
/// Underscore-prefixed like the other system fields, so it cannot collide
/// with a field a user's JSON defines: user fields live *inside* it.
pub const JSON_FIELD: &str = "_json";

/// Options for [`JSON_FIELD`]: indexed with the Chronik analyzer (the same
/// one `value` uses), positions kept for phrase queries, numbers and dates
/// indexed as typed terms, not stored (the raw value is stored already),
/// dots in keys expanded so `{"user.name":..}` and `{"user":{"name":..}}`
/// are both reachable as `user.name`.
pub fn json_field_options() -> JsonObjectOptions {
    JsonObjectOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(ANALYZER_NAME)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_expand_dots_enabled()
}

/// The JSON object a record value holds, if it holds one.
///
/// Scalars and arrays at the top level return `None`: they have no field
/// names to query, and `value` already indexes their text.
pub fn json_object(value: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
    // Cheap rejection before a parse attempt: most non-JSON payloads do not
    // start with '{' once leading whitespace is skipped.
    let first = value.iter().find(|b| !b.is_ascii_whitespace())?;
    if *first != b'{' {
        return None;
    }
    match serde_json::from_slice::<serde_json::Value>(value).ok()? {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_objects_are_json_documents() {
        assert!(json_object(br#"{"marker":"x","n":3}"#).is_some());
        assert!(json_object(b"  \n{\"a\":1}").is_some());
        assert!(json_object(b"[1,2]").is_none());
        assert!(json_object(b"\"text\"").is_none());
        assert!(json_object(b"plain text").is_none());
        assert!(json_object(b"{not json").is_none());
        assert!(json_object(b"").is_none());
    }
}
