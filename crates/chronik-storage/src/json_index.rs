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
///
/// Also a fast field (column per JSON path): numbers and dates as typed
/// columns, strings with their exact value (`raw`). Range queries on a JSON
/// path need it (Tantivy refuses them otherwise: up to v2.19.2 a `range` on a
/// JSON field answered nothing), and aggregations read it. Archives written
/// before have no columns; they answer neither.
pub fn json_field_options() -> JsonObjectOptions {
    JsonObjectOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(ANALYZER_NAME)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_expand_dots_enabled()
        .set_fast("raw")
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

    /// What range queries and aggregations need from the JSON field, in the
    /// Tantivy version in use: numeric paths answer ranges, string paths
    /// answer terms aggregations with their exact values, and epoch-millis
    /// numbers answer a histogram.
    #[test]
    fn fast_json_paths_answer_ranges_and_aggregations() {
        use tantivy::aggregation::agg_req::Aggregations;
        use tantivy::aggregation::AggregationCollector;
        use tantivy::collector::Count;
        use tantivy::query::RangeQuery;
        use tantivy::schema::{Schema, Type};
        use tantivy::{Index, TantivyDocument, Term};
        use std::ops::Bound;

        let mut sb = Schema::builder();
        let json = sb.add_json_field(JSON_FIELD, json_field_options());
        let index = Index::create_in_ram(sb.build());
        crate::text_analysis::register_analyzer(&index);
        let mut w = index.writer(15_000_000).unwrap();
        for i in 0..100i64 {
            let v = serde_json::json!({"at": format!("2026-01-01T00:{:02}:{:02}Z", i / 2, (i % 2) * 30), "ts": 1_767_225_600_000i64 + i * 30_000, "service": format!("Svc-{}", i % 3), "latency": i});
            let mut d = TantivyDocument::new();
            d.add_object(json, json_object(v.to_string().as_bytes()).unwrap().into_iter().map(|(k, v)| (k, v.into())).collect());
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();

        let term = |path: &str, v: i64| {
            let mut t = Term::from_field_json_path(json, path, false);
            t.append_type_and_fast_value(v);
            t
        };
        let _ = Type::I64;
        let q = RangeQuery::new(Bound::Included(term("latency", 10)), Bound::Excluded(term("latency", 20)));
        assert_eq!(searcher.search(&q, &Count).unwrap(), 10);
        // Large, evenly spaced values (epoch millis): stored with a GCD codec.
        let ts = |i: i64| 1_767_225_600_000i64 + i * 30_000;
        let q = RangeQuery::new(Bound::Included(term("ts", ts(10))), Bound::Included(term("ts", ts(19))));
        assert_eq!(searcher.search(&q, &Count).unwrap(), 10, "range over evenly spaced epoch millis");

        let aggs: Aggregations = serde_json::from_value(serde_json::json!({
            "s": {"terms": {"field": "_json.service"}},
            "h": {"histogram": {"field": "_json.ts", "interval": 60000}},
            "dh": {"date_histogram": {"field": "_json.ts", "fixed_interval": "1m"}},
            "dha": {"date_histogram": {"field": "_json.at", "fixed_interval": "1m"}},
            "ha": {"histogram": {"field": "_json.at", "interval": 60000}},
            "avg": {"avg": {"field": "_json.latency"}}
        })).unwrap();
        let res = searcher
            .search(&tantivy::query::AllQuery, &AggregationCollector::from_aggs(aggs, Default::default()))
            .unwrap();
        let v = serde_json::to_value(&res).unwrap();
        let keys: Vec<&str> = v["s"]["buckets"].as_array().unwrap().iter().map(|b| b["key"].as_str().unwrap()).collect();
        assert_eq!(keys.len(), 3, "{v}");
        assert!(keys.contains(&"Svc-0"), "exact values, not analyzed: {keys:?}");
        assert_eq!(v["h"]["buckets"].as_array().unwrap().len(), 50, "{}", v["h"]);
        assert_eq!(v["avg"]["value"].as_f64(), Some(49.5));
        // Why the search API runs a date_histogram as a millisecond
        // histogram: Tantivy's date_histogram answers date columns only (an
        // epoch-millis number gets no buckets), while histogram answers both,
        // keyed in milliseconds.
        assert_eq!(v["dh"]["buckets"].as_array().unwrap().len(), 0);
        assert_eq!(v["dha"]["buckets"].as_array().unwrap().len(), 50);
        assert_eq!(v["ha"]["buckets"].as_array().unwrap().len(), 50);
        assert_eq!(v["ha"]["buckets"][0]["key"].as_f64(), Some(1_767_225_600_000.0));
    }

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
