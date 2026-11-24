//! Which archived segments a query can skip, from their term filters.
//!
//! A query is reduced to the terms a matching document must contain, keeping
//! its boolean structure: a `match` needs any one of its terms, a `bool` needs
//! every `must`/`filter` clause and — when it has none — one `should` clause.
//! Anything a term filter cannot judge (`match_all`, ranges, geo, a stop-word
//! query that matches everything) is `Unknown`, and an `Unknown` requirement
//! never skips a segment. `must_not` only removes documents, so it is ignored.
//! The result is sound: a segment is skipped only when its filter proves no
//! document in it can match.

use crate::api::QueryDsl;
use chronik_storage::term_filter::TermFilter;
use std::sync::OnceLock;

/// What a matching document must contain.
#[derive(Debug, Clone, PartialEq)]
pub enum Requirement {
    /// Cannot be judged from terms: every segment may match.
    Unknown,
    /// At least one of these `(field id, term bytes)`.
    AnyTerm(Vec<(u32, Vec<u8>)>),
    /// Every one of these.
    All(Vec<Requirement>),
    /// At least one of these.
    Any(Vec<Requirement>),
}

/// Empty indexes with each archive layout in use and Chronik's analyzer, to
/// turn a query into the exact terms an archive's term dictionary would hold:
/// the current one, and the one before the JSON field (v2.17.0 and earlier),
/// whose archives answer a field query from the record's full text instead.
/// The layouts share field ids up to the JSON field, which was added last.
fn archive_indexes() -> &'static [tantivy::Index; 2] {
    static INDEXES: OnceLock<[tantivy::Index; 2]> = OnceLock::new();
    INDEXES.get_or_init(|| {
        let (current, _) = chronik_storage::tantivy_segment::SchemaFields::build_schema();
        let mut before_json = tantivy::schema::Schema::builder();
        for (_, entry) in current.fields() {
            if entry.name() != chronik_storage::json_index::JSON_FIELD {
                before_json.add_field(entry.clone());
            }
        }
        [current, before_json.build()].map(|schema| {
            let index = tantivy::Index::create_in_ram(schema);
            chronik_storage::register_analyzer(&index);
            index
        })
    })
}

impl Requirement {
    /// The requirement of `query` (absent = match_all) against archives.
    pub fn of(query: Option<&QueryDsl>) -> Self {
        match query {
            Some(q) => Self::of_dsl(q),
            None => Requirement::Unknown,
        }
    }

    fn of_dsl(dsl: &QueryDsl) -> Self {
        match dsl {
            QueryDsl::Match(_) | QueryDsl::Term(_) => Self::of_leaf(dsl),
            QueryDsl::Bool(b) => {
                let required: Vec<Requirement> =
                    b.must.iter().chain(b.filter.iter()).map(Self::of_dsl).collect();
                if !required.is_empty() {
                    // `should` is optional when there is a `must`/`filter`.
                    return simplify_all(required);
                }
                if !b.should.is_empty() {
                    let options: Vec<Requirement> = b.should.iter().map(Self::of_dsl).collect();
                    if options.iter().any(|r| *r == Requirement::Unknown) {
                        return Requirement::Unknown;
                    }
                    return Requirement::Any(options);
                }
                Requirement::Unknown
            }
            _ => Requirement::Unknown,
        }
    }

    /// A `match` or `term` clause: any one of the terms the query is built
    /// from. (A `term` on analyzed text needs all of its tokens; "any" is
    /// weaker and still sound.)
    ///
    /// The terms of every archive layout are accepted: an archive written
    /// before the JSON field answers a field query from its full text, so
    /// requiring only the JSON-path terms skipped every such archive — a false
    /// negative (v2.18.0). Any layout's terms present: the archive may match.
    fn of_leaf(dsl: &QueryDsl) -> Self {
        let mut terms = Vec::new();
        for index in archive_indexes() {
            let Ok(query) = crate::handlers::build_tantivy_query(dsl, &index.schema(), Some(index)) else {
                // A layout that cannot express the query: the search skips
                // such an archive too, so it adds no requirement.
                continue;
            };
            let mut layout_terms = Vec::new();
            query.query_terms(&mut |term, _| {
                layout_terms.push((term.field().field_id(), term.serialized_value_bytes().to_vec()));
            });
            // No terms: an AllQuery (e.g. a stop-word-only match) — any
            // archive of this layout may match.
            if layout_terms.is_empty() {
                return Requirement::Unknown;
            }
            terms.extend(layout_terms);
        }
        if terms.is_empty() {
            return Requirement::Unknown;
        }
        terms.sort();
        terms.dedup();
        Requirement::AnyTerm(terms)
    }

    /// False only when `filter` proves no document can satisfy this.
    pub fn may_match(&self, filter: &TermFilter) -> bool {
        match self {
            Requirement::Unknown => true,
            Requirement::AnyTerm(terms) => terms.iter().any(|(f, v)| filter.may_contain(*f, v)),
            Requirement::All(reqs) => reqs.iter().all(|r| r.may_match(filter)),
            Requirement::Any(reqs) => reqs.iter().any(|r| r.may_match(filter)),
        }
    }

    /// Whether a filter can ever rule a segment out for this requirement.
    pub fn can_skip(&self) -> bool {
        match self {
            Requirement::Unknown => false,
            Requirement::AnyTerm(_) => true,
            Requirement::All(reqs) => reqs.iter().any(Requirement::can_skip),
            Requirement::Any(reqs) => reqs.iter().all(Requirement::can_skip),
        }
    }
}

fn simplify_all(reqs: Vec<Requirement>) -> Requirement {
    let reqs: Vec<Requirement> = reqs.into_iter().filter(|r| *r != Requirement::Unknown).collect();
    match reqs.len() {
        0 => Requirement::Unknown,
        1 => reqs.into_iter().next().expect("one"),
        _ => Requirement::All(reqs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SearchRequest;
    use chronik_storage::canonical_record::{
        CanonicalRecord, CanonicalRecordEntry, CompressionType, TimestampType,
    };
    use chronik_storage::tantivy_segment::TantivySegmentWriter;

    fn query(q: serde_json::Value) -> Option<QueryDsl> {
        let r: SearchRequest = serde_json::from_value(serde_json::json!({ "query": q })).unwrap();
        r.query
    }

    /// An archive holding these JSON values, and its term filter.
    fn archive(values: &[&str]) -> (tantivy::Index, TermFilter) {
        let records = values
            .iter()
            .enumerate()
            .map(|(i, v)| CanonicalRecordEntry {
                offset: i as i64,
                timestamp: 1_700_000_000_000,
                key: None,
                value: Some(v.as_bytes().to_vec()),
                headers: vec![],
                attributes: 0,
            })
            .collect();
        let batch = CanonicalRecord {
            base_offset: 0,
            partition_leader_epoch: 0,
            producer_id: -1,
            producer_epoch: -1,
            base_sequence: -1,
            is_transactional: false,
            is_control: false,
            compression: CompressionType::None,
            timestamp_type: TimestampType::CreateTime,
            base_timestamp: 1_700_000_000_000,
            max_timestamp: 1_700_000_000_000,
            records,
            compressed_records_wire_bytes: None,
            original_v1_wire_format: None,
            original_v2_wire_format: None,
        };
        let mut w = TantivySegmentWriter::new_in_ram("t".into(), 0, 0).unwrap();
        w.write_batch(&batch).unwrap();
        let index = w.commit().unwrap().0;
        let filter = TermFilter::build(&index).unwrap();
        (index, filter)
    }

    /// Does the query really match anything in the archive? (ground truth)
    fn matches(index: &tantivy::Index, q: &Option<QueryDsl>) -> bool {
        let built = match q {
            Some(q) => crate::handlers::build_tantivy_query(q, &index.schema(), Some(index)).unwrap(),
            None => Box::new(tantivy::query::AllQuery),
        };
        let searcher = index.reader().unwrap().searcher();
        searcher.search(&*built, &tantivy::collector::Count).unwrap() > 0
    }

    #[test]
    fn skips_only_archives_that_cannot_match() {
        let (index, filter) = archive(&[
            r#"{"marker":"zephyrquandary","note":"plain text here"}"#,
            r#"{"marker":"ordinary","count":7,"ok":true}"#,
        ]);
        let cases = [
            (serde_json::json!({"match":{"marker":"zephyrquandary"}}), true),
            (serde_json::json!({"match":{"marker":"absentword"}}), false),
            (serde_json::json!({"match":{"value":"zephyrquandary"}}), true),
            (serde_json::json!({"match":{"value":"absentword"}}), false),
            (serde_json::json!({"match":{"value":"absentword zephyrquandary"}}), true),
            (serde_json::json!({"term":{"count":7}}), true),
            (serde_json::json!({"term":{"count":8}}), false),
            (serde_json::json!({"bool":{"must":[{"match":{"marker":"ordinary"}},{"match":{"marker":"absentword"}}]}}), false),
            (serde_json::json!({"bool":{"should":[{"match":{"marker":"absentword"}},{"match":{"note":"plain"}}]}}), true),
            (serde_json::json!({"bool":{"must_not":[{"match":{"marker":"ordinary"}}]}}), true),
            (serde_json::json!({"match_all":{}}), true),
            (serde_json::json!({"match":{"value":"the"}}), true),
        ];
        for (q, expect_possible) in cases {
            let dsl = query(q.clone());
            let req = Requirement::of(dsl.as_ref());
            let truth = matches(&index, &dsl);
            assert_eq!(req.may_match(&filter), expect_possible, "{q}: {req:?}");
            // Never a false negative: whatever really matches is never skipped.
            assert!(!truth || req.may_match(&filter), "{q} matches but would be skipped");
        }
    }

    /// An archive written before the JSON field (v2.17.0 and earlier) answers
    /// a field query from its full text. v2.18.0 required the JSON-path terms
    /// only and skipped every such archive.
    #[test]
    fn archives_of_the_layout_before_the_json_field_are_not_skipped() {
        let schema = archive_indexes()[1].schema();
        assert!(schema.get_field(chronik_storage::json_index::JSON_FIELD).is_err());
        let index = tantivy::Index::create_in_ram(schema.clone());
        chronik_storage::register_analyzer(&index);
        let value = schema.get_field("value").unwrap();
        let offset = schema.get_field("_offset").unwrap();
        let mut w: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        for (o, v) in [(0i64, r#"{"marker":"zephyrquandary","n":1}"#), (1, r#"{"marker":"ordinary","n":2}"#)] {
            let mut d = tantivy::TantivyDocument::default();
            d.add_i64(offset, o);
            d.add_text(value, v);
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
        let filter = TermFilter::build(&index).unwrap();
        for (q, present) in [
            (serde_json::json!({"match":{"marker":"zephyrquandary"}}), true),
            (serde_json::json!({"match":{"value":"zephyrquandary"}}), true),
            (serde_json::json!({"match":{"marker":"absentword"}}), false),
        ] {
            let dsl = query(q.clone());
            let truth = matches(&index, &dsl);
            assert_eq!(truth, present, "{q}: ground truth");
            let req = Requirement::of(dsl.as_ref());
            assert!(!truth || req.may_match(&filter), "{q} matches this archive but would be skipped");
            if !present {
                assert!(!req.may_match(&filter), "{q}: absent everywhere, skipped");
            }
        }
    }

    #[test]
    fn unjudgeable_queries_never_skip() {
        for q in [
            serde_json::json!({"match_all":{}}),
            serde_json::json!({"range":{"partition":{"gte":0}}}),
            serde_json::json!({"bool":{"must_not":[{"match":{"value":"x"}}]}}),
            serde_json::json!({"bool":{"should":[{"match":{"value":"x"}},{"match_all":{}}]}}),
        ] {
            assert!(!Requirement::of(query(q.clone()).as_ref()).can_skip(), "{q}");
        }
        assert!(!Requirement::of(None).can_skip());
        assert!(Requirement::of(query(serde_json::json!({"match":{"value":"coffee"}})).as_ref()).can_skip());
    }
}
