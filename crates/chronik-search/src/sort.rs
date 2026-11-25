//! Elasticsearch `sort`: ordering hits by record fields and JSON paths.
//!
//! Up to v2.20.1 a request's `sort` was parsed and then ignored: every search
//! answered in relevance order, so `"sort": [{"offset": "asc"}]` returned the
//! newest records instead of the first ones. Now each source (an archive, a
//! hot partition, a REST index) collects its top `from + size` by the sort
//! keys from the fields' columns, every hit carries its `sort` values (as in
//! Elasticsearch), and the node and cluster merges order hits by them.
//!
//! - A key is `_score`, `_doc` (record order: offset, ascending), a record
//!   field (`offset`, `timestamp`, `partition`) or a path in the record's JSON
//!   value (`status`, `user.name`; `x.keyword` means `x`). Order defaults to
//!   ascending, descending for `_score`; a hit without a value sorts last in
//!   either direction.
//! - Timestamps compare by the second: archives store them in seconds, so a
//!   finer order would disagree between the hot index and the archives. The
//!   reported value is that second in milliseconds.
//! - A JSON path holding numbers sorts numerically; one holding only strings
//!   lexicographically; numbers before strings where a path holds both.
//! - Equal keys fall back to the usual order (`ranking::hit_order`).

use std::cmp::Ordering;

use serde_json::Value;
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::{Column, DynamicColumn, StrColumn};
use tantivy::query::Query;
use tantivy::{DocAddress, DocId, Score, Searcher, SegmentReader};

use crate::api::{SortClause, SortOrder, SortOrderSpec};
use crate::corpus_stats::CorpusStats;

#[derive(Clone, Debug, PartialEq)]
pub enum SortField {
    Score,
    Field(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    pub field: SortField,
    pub desc: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SortSpec {
    pub keys: Vec<SortKey>,
}

impl SortSpec {
    /// The request's sort; `None` when it has none, or sorts by relevance
    /// alone (the order every search already answers in).
    pub fn of(sort: Option<&[SortClause]>) -> Option<Self> {
        let mut keys = Vec::new();
        for clause in sort.unwrap_or_default() {
            match clause {
                SortClause::Field(name) => keys.push(key(name, None)),
                SortClause::Object(map) => {
                    let mut entries: Vec<_> = map.iter().collect();
                    entries.sort_by(|a, b| a.0.cmp(b.0));
                    for (name, spec) in entries {
                        let order = match spec {
                            SortOrderSpec::Order(o) => Some(o.clone()),
                            SortOrderSpec::Options { order } => order.clone(),
                        };
                        keys.push(key(name, order));
                    }
                }
            }
        }
        let relevance = keys.is_empty() || (keys.len() == 1 && keys[0] == SortKey { field: SortField::Score, desc: true });
        (!relevance).then_some(SortSpec { keys })
    }

    /// Whether any key needs relevance scores.
    pub fn scores(&self) -> bool {
        self.keys.iter().any(|k| k.field == SortField::Score)
    }

    /// The order of two hits' `sort` values.
    pub fn compare(&self, a: &[Value], b: &[Value]) -> Ordering {
        for (i, k) in self.keys.iter().enumerate() {
            let o = compare_values(a.get(i).unwrap_or(&Value::Null), b.get(i).unwrap_or(&Value::Null), k.desc);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
}

fn key(name: &str, order: Option<SortOrder>) -> SortKey {
    let desc = |default: bool| order.map_or(default, |o| matches!(o, SortOrder::Desc));
    match name {
        "_score" => SortKey { field: SortField::Score, desc: desc(true) },
        "_doc" => SortKey { field: SortField::Field("offset".into()), desc: desc(false) },
        _ => SortKey { field: SortField::Field(name.strip_suffix(".keyword").unwrap_or(name).to_string()), desc: desc(false) },
    }
}

/// Missing (`null`) last in both directions; numbers before strings.
fn compare_values(a: &Value, b: &Value, desc: bool) -> Ordering {
    let rank = |v: &Value| match v {
        Value::Null => 2,
        Value::String(_) => 1,
        _ => 0,
    };
    match (rank(a), rank(b)) {
        (2, 2) => return Ordering::Equal,
        (2, _) => return Ordering::Greater,
        (_, 2) => return Ordering::Less,
        _ => {}
    }
    let base = match (a, b) {
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::String(_), _) => Ordering::Greater,
        (_, Value::String(_)) => Ordering::Less,
        _ => number(a).total_cmp(&number(b)),
    };
    if desc {
        base.reverse()
    } else {
        base
    }
}

fn number(v: &Value) -> f64 {
    match v {
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => v.as_f64().unwrap_or(f64::NAN),
    }
}

/// A sort key's column in one segment.
enum Accessor {
    Score,
    I64(Column<i64>),
    U64(Column<u64>),
    F64(Column<f64>),
    Date(Column<tantivy::DateTime>),
    Bool(Column<bool>),
    Str(StrColumn),
    Missing,
}

/// A value inside one segment: strings as their ordinal (ordinals of one
/// column are in lexicographic order), turned into text for the hits kept.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SegValue {
    Num(f64),
    Ord(u64),
    Missing,
}

fn compare_seg(a: SegValue, b: SegValue, desc: bool) -> Ordering {
    let base = match (a, b) {
        (SegValue::Missing, SegValue::Missing) => return Ordering::Equal,
        (SegValue::Missing, _) => return Ordering::Greater,
        (_, SegValue::Missing) => return Ordering::Less,
        (SegValue::Num(x), SegValue::Num(y)) => x.total_cmp(&y),
        (SegValue::Ord(x), SegValue::Ord(y)) => x.cmp(&y),
        (SegValue::Num(_), SegValue::Ord(_)) => Ordering::Less,
        (SegValue::Ord(_), SegValue::Num(_)) => Ordering::Greater,
    };
    if desc {
        base.reverse()
    } else {
        base
    }
}

/// The column `name` means in `schema`: a field of that name with a column
/// (a REST index's own fields), else what aggregations resolve it to (record
/// fields of archives and hot partitions, JSON paths).
fn column_name(schema: &tantivy::schema::Schema, name: &str) -> String {
    if let Ok(f) = schema.get_field(name) {
        if schema.get_field_entry(f).is_fast() {
            return name.to_string();
        }
    }
    crate::topic_aggs::resolve_agg_field(schema, name)
}

const TIMESTAMP_COLUMNS: [&str; 3] = ["_ts", "timestamp", "_timestamp"];

fn accessor(segment: &SegmentReader, schema: &tantivy::schema::Schema, key: &SortKey) -> (Accessor, bool) {
    let SortField::Field(name) = &key.field else {
        return (Accessor::Score, false);
    };
    let column = column_name(schema, name);
    let is_timestamp = TIMESTAMP_COLUMNS.contains(&column.as_str());
    let handles = segment.fast_fields().dynamic_column_handles(&column).unwrap_or_default();
    // Numbers first, then strings, then booleans.
    let preference = |t: tantivy::columnar::ColumnType| {
        use tantivy::columnar::ColumnType::*;
        match t {
            I64 | U64 | F64 | DateTime => 0,
            Str => 1,
            Bool => 2,
            _ => 3,
        }
    };
    let Some(handle) = handles.iter().filter(|h| preference(h.column_type()) < 3).min_by_key(|h| preference(h.column_type())) else {
        return (Accessor::Missing, is_timestamp);
    };
    let acc = match handle.open() {
        Ok(DynamicColumn::I64(c)) => Accessor::I64(c),
        Ok(DynamicColumn::U64(c)) => Accessor::U64(c),
        Ok(DynamicColumn::F64(c)) => Accessor::F64(c),
        Ok(DynamicColumn::DateTime(c)) => Accessor::Date(c),
        Ok(DynamicColumn::Bool(c)) => Accessor::Bool(c),
        Ok(DynamicColumn::Str(c)) => Accessor::Str(c),
        _ => Accessor::Missing,
    };
    (acc, is_timestamp)
}

struct SortCollector {
    spec: std::sync::Arc<SortSpec>,
    limit: usize,
}

struct SortSegment {
    spec: std::sync::Arc<SortSpec>,
    limit: usize,
    ord: tantivy::SegmentOrdinal,
    /// Per key: its column, and whether it is the record timestamp.
    accessors: Vec<(Accessor, bool)>,
    held: Vec<(Vec<SegValue>, Score, DocId)>,
}

impl SortSegment {
    fn value(&self, i: usize, doc: DocId, score: Score) -> SegValue {
        let (acc, is_timestamp) = &self.accessors[i];
        let num = |x: f64| SegValue::Num(x);
        match acc {
            Accessor::Score => num(f64::from(score)),
            Accessor::I64(c) => c.first(doc).map_or(SegValue::Missing, |v| {
                // Hot partitions keep timestamps in milliseconds: by the second.
                num(if *is_timestamp { (v.div_euclid(1000) * 1000) as f64 } else { v as f64 })
            }),
            Accessor::U64(c) => c.first(doc).map_or(SegValue::Missing, |v| num(v as f64)),
            Accessor::F64(c) => c.first(doc).map_or(SegValue::Missing, num),
            Accessor::Date(c) => c.first(doc).map_or(SegValue::Missing, |v| num((v.into_timestamp_secs() * 1000) as f64)),
            Accessor::Bool(c) => c.first(doc).map_or(SegValue::Missing, |v| num(f64::from(u8::from(v)))),
            Accessor::Str(c) => c.ords().first(doc).map_or(SegValue::Missing, SegValue::Ord),
            Accessor::Missing => SegValue::Missing,
        }
    }

    fn compare(&self, a: &(Vec<SegValue>, Score, DocId), b: &(Vec<SegValue>, Score, DocId)) -> Ordering {
        for (i, k) in self.spec.keys.iter().enumerate() {
            let o = compare_seg(a.0[i], b.0[i], k.desc);
            if o != Ordering::Equal {
                return o;
            }
        }
        a.2.cmp(&b.2)
    }

    fn trim(&mut self) {
        let mut held = std::mem::take(&mut self.held);
        held.sort_by(|a, b| self.compare(a, b));
        held.truncate(self.limit);
        self.held = held;
    }
}

impl Collector for SortCollector {
    type Fruit = Vec<(Vec<Value>, Score, DocAddress)>;
    type Child = SortSegment;

    fn for_segment(&self, ord: tantivy::SegmentOrdinal, segment: &SegmentReader) -> tantivy::Result<Self::Child> {
        // Every segment of an index shares its schema; columns are per segment.
        let schema = segment.schema();
        let accessors = self.spec.keys.iter().map(|k| accessor(segment, schema, k)).collect();
        Ok(SortSegment { spec: self.spec.clone(), limit: self.limit, ord, accessors, held: Vec::new() })
    }

    fn requires_scoring(&self) -> bool {
        self.spec.scores()
    }

    fn merge_fruits(&self, fruits: Vec<Self::Fruit>) -> tantivy::Result<Self::Fruit> {
        let mut all: Self::Fruit = fruits.into_iter().flatten().collect();
        all.sort_by(|a, b| self.spec.compare(&a.0, &b.0).then(a.2.cmp(&b.2)));
        all.truncate(self.limit);
        Ok(all)
    }
}

impl SegmentCollector for SortSegment {
    type Fruit = Vec<(Vec<Value>, Score, DocAddress)>;

    fn collect(&mut self, doc: DocId, score: Score) {
        let values = (0..self.spec.keys.len()).map(|i| self.value(i, doc, score)).collect();
        self.held.push((values, score, doc));
        if self.held.len() >= self.limit.saturating_mul(2).max(64) {
            self.trim();
        }
    }

    fn harvest(mut self) -> Self::Fruit {
        self.trim();
        let mut text = String::new();
        let held = std::mem::take(&mut self.held);
        held.into_iter()
            .map(|(values, score, doc)| {
                let values = values
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| match (v, &self.accessors[i].0) {
                        (SegValue::Num(x), _) => json_number(x),
                        (SegValue::Ord(o), Accessor::Str(c)) => {
                            text.clear();
                            match c.ord_to_str(o, &mut text) {
                                Ok(true) => Value::String(text.clone()),
                                _ => Value::Null,
                            }
                        }
                        _ => Value::Null,
                    })
                    .collect();
                (values, score, DocAddress::new(self.ord, doc))
            })
            .collect()
    }
}

fn json_number(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9.0e15 {
        Value::from(x as i64)
    } else {
        serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
    }
}

/// The top `limit` documents of `query` in `spec` order, with each one's sort
/// values (and its score, when a key needs scores), the best first.
pub fn top_sorted(
    searcher: &Searcher,
    query: &dyn Query,
    spec: &SortSpec,
    limit: usize,
    stats: Option<&CorpusStats>,
) -> tantivy::Result<Vec<(Vec<Value>, Score, DocAddress)>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    crate::ranking::search_scored(searcher, query, &SortCollector { spec: std::sync::Arc::new(spec.clone()), limit }, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(v: Value) -> Option<SortSpec> {
        let clauses: Vec<SortClause> = serde_json::from_value(v).unwrap();
        SortSpec::of(Some(&clauses))
    }

    #[test]
    fn clauses_parse_with_elasticsearch_defaults() {
        assert_eq!(spec(json!([])), None);
        assert_eq!(spec(json!(["_score"])), None, "relevance alone is the default order");
        assert_eq!(spec(json!([{"_score": "desc"}])), None);
        let s = spec(json!(["offset", {"timestamp": "desc"}, {"level.keyword": {"order": "asc"}}, "_doc", {"_score": {"order": "asc"}}])).unwrap();
        let keys: Vec<(SortField, bool)> = s.keys.into_iter().map(|k| (k.field, k.desc)).collect();
        assert_eq!(
            keys,
            vec![
                (SortField::Field("offset".into()), false),
                (SortField::Field("timestamp".into()), true),
                (SortField::Field("level".into()), false),
                (SortField::Field("offset".into()), false),
                (SortField::Score, false),
            ]
        );
    }

    #[test]
    fn missing_values_sort_last_in_both_directions() {
        for desc in [false, true] {
            let s = SortSpec { keys: vec![SortKey { field: SortField::Field("x".into()), desc }] };
            let mut v = vec![vec![json!(null)], vec![json!(3)], vec![json!("b")], vec![json!(1)], vec![json!("a")]];
            v.sort_by(|a, b| s.compare(a, b));
            let want = if desc {
                vec![json!("b"), json!("a"), json!(3), json!(1), json!(null)]
            } else {
                vec![json!(1), json!(3), json!("a"), json!("b"), json!(null)]
            };
            assert_eq!(v.into_iter().map(|x| x[0].clone()).collect::<Vec<_>>(), want, "desc={desc}");
        }
    }

    /// The collector returns exactly the documents and values of sorting
    /// every matching document by hand, over several segments, for record
    /// fields, JSON numbers, JSON strings and a path some documents lack.
    #[test]
    fn sorted_collection_matches_sorting_every_document() {
        use tantivy::schema::{Schema, FAST, INDEXED, STORED};
        let mut sb = Schema::builder();
        let ts = sb.add_date_field("_ts", INDEXED | STORED | FAST);
        let off = sb.add_i64_field("_offset", INDEXED | STORED | FAST);
        let json = sb.add_json_field(chronik_storage::json_index::JSON_FIELD, chronik_storage::json_index::json_field_options());
        let index = tantivy::Index::create_in_ram(sb.build());
        let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
        let mut rows: Vec<(i64, i64, Option<i64>, String)> = Vec::new(); // offset, second, status, level
        for o in 0..1_200i64 {
            let second = 1_000 + (o * 7919) % 97;
            let status = (o % 5 != 0).then_some((o * 31) % 600);
            let level = ["error", "info", "warn", "debug"][(o % 4) as usize].to_string();
            let mut d = tantivy::TantivyDocument::new();
            d.add_date(ts, tantivy::DateTime::from_timestamp_secs(second));
            d.add_i64(off, o);
            let mut obj = std::collections::BTreeMap::new();
            if let Some(s) = status {
                obj.insert("status".to_string(), tantivy::schema::OwnedValue::I64(s));
            }
            obj.insert("level".to_string(), tantivy::schema::OwnedValue::Str(level.clone()));
            d.add_object(json, obj);
            w.add_document(d).unwrap();
            rows.push((o, second, status, level));
            if o % 300 == 299 {
                w.commit().unwrap();
            }
        }
        w.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();
        assert!(searcher.segment_readers().len() > 1);
        let offset_of = |a: DocAddress| {
            let col = searcher.segment_reader(a.segment_ord).fast_fields().i64("_offset").unwrap();
            col.first(a.doc_id).unwrap()
        };
        for (sort, limit) in [
            (json!([{"status": "desc"}, "offset"]), 50),
            (json!([{"status": "asc"}, {"offset": "desc"}]), 1_000),
            (json!([{"level.keyword": "asc"}, {"timestamp": "desc"}, "offset"]), 37),
            (json!([{"timestamp": "asc"}, {"offset": "desc"}]), 200),
            (json!(["_doc"]), 10),
        ] {
            let s = spec(sort.clone()).unwrap();
            let got = top_sorted(&searcher, &tantivy::query::AllQuery, &s, limit, None).unwrap();
            let value = |r: &(i64, i64, Option<i64>, String), field: &SortField| -> Value {
                match field {
                    SortField::Field(f) if f == "offset" => json!(r.0),
                    SortField::Field(f) if f == "timestamp" => json!(r.1 * 1000),
                    SortField::Field(f) if f == "status" => r.2.map_or(Value::Null, |x| json!(x)),
                    SortField::Field(f) if f == "level" => json!(r.3),
                    _ => unreachable!(),
                }
            };
            let mut want: Vec<Vec<Value>> = rows.iter().map(|r| s.keys.iter().map(|k| value(r, &k.field)).collect()).collect();
            want.sort_by(|a, b| s.compare(a, b));
            want.truncate(limit);
            let got_values: Vec<Vec<Value>> = got.iter().map(|g| g.0.clone()).collect();
            assert_eq!(got_values, want, "{sort}");
            // And the values belong to the documents returned.
            for (vals, _, addr) in &got {
                let row = &rows[offset_of(*addr) as usize];
                let expected: Vec<Value> = s.keys.iter().map(|k| value(row, &k.field)).collect();
                assert_eq!(vals, &expected, "{sort}");
            }
        }
    }
}
