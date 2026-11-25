//! Exact totals and aggregations over a topic's hot and archived records.
//!
//! Each source (an archived segment, a hot partition) is searched with
//! Tantivy's distributed aggregation collector, which yields mergeable partial
//! results; partials are merged per node, sent between nodes, and finalised
//! once into the Elasticsearch-shaped answer.
//!
//! Field names are the user's: `service`, `user.name`, `ts`. Each source maps
//! them to its own schema ([`TopicAggs::for_schema`]): the record's own fields
//! (`timestamp`, `offset`, `partition`) to that schema's columns, anything else
//! to a path in the JSON field, whose columns hold every JSON value
//! (`json_index::json_field_options`). `x.keyword` means `x`, as with an
//! Elasticsearch dynamic mapping.
//!
//! `date_histogram` runs as a `histogram` in milliseconds: Tantivy's own
//! answers date columns only, so an epoch-millis field (most JSON timestamps)
//! got no buckets. Keys come back as integer milliseconds with
//! `key_as_string`, as Elasticsearch writes them.
//!
//! Every record is counted once: the hot index only counts offsets above its
//! partition's archived watermark (the archive holds the rest), and on a
//! cluster each node counts the partitions it leads.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tantivy::aggregation::agg_req::Aggregations;
use tantivy::aggregation::agg_result::{AggregationResult, AggregationResults, BucketEntries, BucketEntry, BucketResult};
use tantivy::aggregation::intermediate_agg_result::IntermediateAggregationResults;
use tantivy::aggregation::Key;
use tantivy::aggregation::{AggregationLimitsGuard, DistributedAggregationCollector};
use tantivy::collector::Count;
use tantivy::query::Query;
use tantivy::schema::Schema;
use tantivy::Searcher;

use chronik_common::{Error, Result};

/// A node's share of a count and of the aggregations, before finalising.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Partial {
    /// Matching records.
    pub count: u64,
    /// Partial aggregation results, when the request has aggregations. Sent
    /// as bincode in base64: their maps have non-string keys (terms buckets),
    /// which JSON cannot hold.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "binary_aggs")]
    pub aggs: Option<IntermediateAggregationResults>,
}

mod binary_aggs {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};
    use tantivy::aggregation::intermediate_agg_result::IntermediateAggregationResults;

    pub fn serialize<S: Serializer>(v: &Option<IntermediateAggregationResults>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(aggs) => {
                let bytes = bincode::serialize(aggs).map_err(serde::ser::Error::custom)?;
                s.serialize_some(&base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<IntermediateAggregationResults>, D::Error> {
        let Some(text) = Option::<String>::deserialize(d)? else { return Ok(None) };
        let bytes = base64::engine::general_purpose::STANDARD.decode(text).map_err(serde::de::Error::custom)?;
        bincode::deserialize(&bytes).map(Some).map_err(serde::de::Error::custom)
    }
}

impl Partial {
    pub fn merge(&mut self, other: Partial) -> Result<()> {
        self.count += other.count;
        match (&mut self.aggs, other.aggs) {
            (Some(mine), Some(theirs)) => mine
                .merge_fruits(theirs)
                .map_err(|e| Error::Internal(format!("merging aggregations: {e}")))?,
            (mine @ None, theirs) => *mine = theirs,
            (Some(_), None) => {}
        }
        Ok(())
    }
}

/// Merges many partials pairwise by size, as merge sort does, so each is
/// merged about log2(n) times rather than once for every partial after it.
/// Tantivy rebuilds a histogram's whole bucket list on each merge: folding 49
/// sources one by one into a growing total moved ~200K buckets for one
/// 8K-bucket date_histogram over 10M records, most of the query's CPU.
#[derive(Default)]
pub struct Merger {
    /// Partials by level (how many merges each holds, as a power of two),
    /// strictly decreasing from the bottom.
    stack: Vec<(u32, Partial)>,
}

impl Merger {
    pub fn push(&mut self, mut part: Partial) -> Result<()> {
        let mut level = 0;
        while self.stack.last().is_some_and(|(l, _)| *l == level) {
            let (_, mut below) = self.stack.pop().expect("checked");
            below.merge(part)?;
            part = below;
            level += 1;
        }
        self.stack.push((level, part));
        Ok(())
    }

    /// Everything pushed, merged; smallest first.
    pub fn finish(self) -> Result<Partial> {
        let mut parts = self.stack.into_iter().rev().map(|(_, p)| p);
        let Some(mut total) = parts.next() else {
            return Ok(Partial::default());
        };
        for mut larger in parts {
            larger.merge(total)?;
            total = larger;
        }
        Ok(total)
    }
}

/// A request's aggregations, translated, ready to run against any source.
#[derive(Debug, Clone)]
pub struct TopicAggs {
    /// The aggregations with user field names, date_histograms translated.
    request: Value,
    /// Paths (aggregation names, nested ones joined with '>') of the
    /// date_histograms, whose keys are formatted on the way out.
    date_histograms: HashSet<String>,
}

/// Whether a request needs an exact count (and therefore every source).
pub fn wants_exact_count(size: usize, track_total_hits: Option<&Value>, has_aggs: bool) -> bool {
    has_aggs || size == 0 || matches!(track_total_hits, Some(Value::Bool(true)))
}

impl TopicAggs {
    /// Translate a request's `aggs` (or `aggregations`).
    pub fn prepare(aggs: &Value) -> Result<Self> {
        let mut date_histograms = HashSet::new();
        let request = translate(aggs, "", &mut date_histograms)?;
        Ok(Self { request, date_histograms })
    }

    /// The aggregations with field names mapped to `schema`.
    pub fn for_schema(&self, schema: &Schema) -> Result<Aggregations> {
        let mut req = self.request.clone();
        rewrite_fields(&mut req, schema);
        serde_json::from_value(req).map_err(|e| Error::InvalidInput(format!("aggregations: {e}")))
    }

    /// Final, Elasticsearch-shaped results from merged partials, as JSON.
    ///
    /// Written straight from Tantivy's result: building a `serde_json::Value`
    /// tree first (a map per bucket), editing it and dropping it was ~40% of
    /// the CPU of an 8K-bucket date_histogram.
    pub fn finalize(&self, partial: Option<IntermediateAggregationResults>) -> Result<Box<serde_json::value::RawValue>> {
        // The request only shapes the result here (names, sizes, intervals);
        // its fields were resolved per source when the partials were made.
        let mut req = self.request.clone();
        rewrite_fields(&mut req, &Schema::builder().build());
        let req: Aggregations =
            serde_json::from_value(req).map_err(|e| Error::InvalidInput(format!("aggregations: {e}")))?;
        let partial = partial.unwrap_or_default();
        let result = partial
            .into_final_result(req, AggregationLimitsGuard::default())
            .map_err(|e| Error::InvalidInput(format!("aggregations: {e}")))?;
        let mut result = result;
        format_date_histograms(&mut result, "", &self.date_histograms);
        serde_json::value::to_raw_value(&result).map_err(|e| Error::Internal(format!("aggregations: {e}")))
    }
}

/// Count, and aggregate when `aggs` is set, `query` over one searcher.
pub fn collect(searcher: &Searcher, query: &dyn Query, aggs: Option<Aggregations>) -> Result<Partial> {
    match aggs {
        None => {
            let count = searcher.search(query, &Count).map_err(|e| Error::Internal(format!("count: {e}")))?;
            Ok(Partial { count: count as u64, aggs: None })
        }
        Some(aggs) => {
            let collector = DistributedAggregationCollector::from_aggs(aggs, Default::default());
            let (count, aggs) = searcher
                .search(query, &(Count, collector))
                .map_err(|e| Error::InvalidInput(format!("aggregations: {e}")))?;
            Ok(Partial { count: count as u64, aggs: Some(aggs) })
        }
    }
}

/// Elasticsearch interval ("30s", "1m", "1h", "1d", "1w", "500ms", or a
/// calendar unit name) in milliseconds. Months, quarters and years have no
/// fixed length and are refused.
fn interval_ms(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    let named = match raw {
        "second" => Some(1_000),
        "minute" => Some(60_000),
        "hour" => Some(3_600_000),
        "day" => Some(86_400_000),
        "week" => Some(604_800_000),
        _ => None,
    };
    if let Some(ms) = named {
        return Ok(ms);
    }
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (n, unit) = raw.split_at(split);
    let n: u64 = n.parse().map_err(|_| Error::InvalidInput(format!("interval: {raw}")))?;
    let unit_ms = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => {
            return Err(Error::InvalidInput(format!(
                "interval {raw}: only fixed intervals (ms, s, m, h, d, w) are supported"
            )))
        }
    };
    Ok(n * unit_ms)
}

/// Copy `aggs`, turning each date_histogram into a millisecond histogram and
/// recording its path.
fn translate(aggs: &Value, prefix: &str, dates: &mut HashSet<String>) -> Result<Value> {
    let Value::Object(map) = aggs else {
        return Err(Error::InvalidInput("aggregations must be an object".into()));
    };
    let mut out = Map::new();
    for (name, node) in map {
        let Value::Object(node) = node else {
            return Err(Error::InvalidInput(format!("aggregation {name} must be an object")));
        };
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}>{name}") };
        let mut new_node = Map::new();
        for (key, body) in node {
            match key.as_str() {
                "aggs" | "aggregations" => {
                    new_node.insert("aggs".into(), translate(body, &path, dates)?);
                }
                "date_histogram" => {
                    let params = body.as_object().ok_or_else(|| Error::InvalidInput(format!("{name}: date_histogram")))?;
                    let interval = ["fixed_interval", "calendar_interval", "interval"]
                        .iter()
                        .find_map(|k| params.get(*k).and_then(Value::as_str))
                        .ok_or_else(|| Error::InvalidInput(format!("{name}: date_histogram needs fixed_interval")))?;
                    let mut h = Map::new();
                    if let Some(field) = params.get("field") {
                        h.insert("field".into(), field.clone());
                    }
                    h.insert("interval".into(), Value::from(interval_ms(interval)? as f64));
                    for k in ["min_doc_count", "extended_bounds", "hard_bounds", "keyed"] {
                        if let Some(v) = params.get(k) {
                            h.insert(k.into(), v.clone());
                        }
                    }
                    new_node.insert("histogram".into(), Value::Object(h));
                    dates.insert(path.clone());
                }
                _ => {
                    new_node.insert(key.clone(), body.clone());
                }
            }
        }
        out.insert(name.clone(), Value::Object(new_node));
    }
    Ok(Value::Object(out))
}

/// The schema field (or JSON path) a user field name means in `schema`.
pub(crate) fn resolve_agg_field(schema: &Schema, name: &str) -> String {
    let name = name.strip_suffix(".keyword").unwrap_or(name);
    if name.starts_with(chronik_storage::json_index::JSON_FIELD) {
        return name.to_string();
    }
    let candidates: &[&str] = match name {
        "timestamp" | "@timestamp" | "_ts" => &["_ts", "timestamp"],
        "offset" | "_offset" => &["_offset", "offset"],
        "partition" | "_partition" => &["partition"],
        _ => &[],
    };
    for c in candidates {
        if let Ok(f) = schema.get_field(c) {
            if schema.get_field_entry(f).is_fast() {
                return (*c).to_string();
            }
        }
    }
    format!("{}.{}", chronik_storage::json_index::JSON_FIELD, name)
}

/// Replace every aggregation's `field` with what it means in `schema`.
fn rewrite_fields(aggs: &mut Value, schema: &Schema) {
    let Value::Object(map) = aggs else { return };
    for node in map.values_mut() {
        let Value::Object(node) = node else { continue };
        for (key, body) in node.iter_mut() {
            if key == "aggs" {
                rewrite_fields(body, schema);
            } else if let Some(field) = body.get_mut("field") {
                if let Some(name) = field.as_str() {
                    *field = Value::String(resolve_agg_field(schema, name));
                }
            }
        }
    }
}

/// Integer keys and `key_as_string` (RFC 3339, milliseconds) on the buckets of
/// date_histograms, at any depth.
fn format_date_histograms(results: &mut AggregationResults, prefix: &str, dates: &HashSet<String>) {
    for (name, result) in results.0.iter_mut() {
        let AggregationResult::BucketResult(buckets) = result else { continue };
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}>{name}") };
        let is_date = dates.contains(&path);
        match buckets {
            BucketResult::Histogram { buckets } => {
                let mut format = |b: &mut BucketEntry| {
                    if is_date {
                        let ms = match b.key {
                            Key::F64(v) => Some(v as i64),
                            Key::I64(v) => Some(v),
                            Key::U64(v) => Some(v as i64),
                            Key::Str(_) => None,
                        };
                        if let Some(ms) = ms {
                            b.key = Key::I64(ms);
                            b.key_as_string = chrono::DateTime::from_timestamp_millis(ms)
                                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
                        }
                    }
                    format_date_histograms(&mut b.sub_aggregation, &path, dates);
                };
                match buckets {
                    BucketEntries::Vec(v) => v.iter_mut().for_each(&mut format),
                    BucketEntries::HashMap(m) => m.values_mut().for_each(&mut format),
                }
            }
            BucketResult::Range { buckets } => {
                let mut sub = |b: &mut tantivy::aggregation::agg_result::RangeBucketEntry| {
                    format_date_histograms(&mut b.sub_aggregation, &path, dates)
                };
                match buckets {
                    BucketEntries::Vec(v) => v.iter_mut().for_each(&mut sub),
                    BucketEntries::HashMap(m) => m.values_mut().for_each(&mut sub),
                }
            }
            BucketResult::Terms { buckets, .. } => {
                buckets.iter_mut().for_each(|b| format_date_histograms(&mut b.sub_aggregation, &path, dates))
            }
            BucketResult::MultiTerms { buckets, .. } => {
                buckets.iter_mut().for_each(|b| format_date_histograms(&mut b.sub_aggregation, &path, dates))
            }
            BucketResult::Composite { buckets, .. } => {
                buckets.iter_mut().for_each(|b| format_date_histograms(&mut b.sub_aggregation, &path, dates))
            }
            BucketResult::Filter(f) => format_date_histograms(&mut f.sub_aggregations, &path, dates),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn intervals_in_milliseconds() {
        assert_eq!(interval_ms("1m").unwrap(), 60_000);
        assert_eq!(interval_ms("30s").unwrap(), 30_000);
        assert_eq!(interval_ms("500ms").unwrap(), 500);
        assert_eq!(interval_ms("hour").unwrap(), 3_600_000);
        assert_eq!(interval_ms("2d").unwrap(), 172_800_000);
        assert!(interval_ms("1M").is_err());
        assert!(interval_ms("month").is_err());
    }

    #[test]
    fn date_histograms_become_millisecond_histograms_at_any_depth() {
        let aggs = json!({
            "per_min": {"date_histogram": {"field": "ts", "fixed_interval": "1m", "min_doc_count": 1},
                        "aggs": {"per_hour": {"date_histogram": {"field": "ts", "calendar_interval": "hour"}}}},
            "by_service": {"terms": {"field": "service.keyword", "size": 3}}
        });
        let t = TopicAggs::prepare(&aggs).unwrap();
        assert_eq!(t.request["per_min"]["histogram"], json!({"field": "ts", "interval": 60000.0, "min_doc_count": 1}));
        assert_eq!(t.request["per_min"]["aggs"]["per_hour"]["histogram"]["interval"], json!(3600000.0));
        assert_eq!(t.date_histograms, ["per_min", "per_min>per_hour"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn fields_map_to_each_schema() {
        let mut archive = Schema::builder();
        archive.add_date_field("_ts", tantivy::schema::INDEXED | tantivy::schema::FAST);
        archive.add_i64_field("_offset", tantivy::schema::FAST);
        let archive = archive.build();
        let mut hot = Schema::builder();
        hot.add_i64_field("timestamp", tantivy::schema::FAST);
        hot.add_i64_field("offset", tantivy::schema::FAST);
        let hot = hot.build();
        assert_eq!(resolve_agg_field(&archive, "timestamp"), "_ts");
        assert_eq!(resolve_agg_field(&hot, "timestamp"), "timestamp");
        assert_eq!(resolve_agg_field(&hot, "offset"), "offset");
        assert_eq!(resolve_agg_field(&archive, "service"), "_json.service");
        assert_eq!(resolve_agg_field(&archive, "service.keyword"), "_json.service");
        assert_eq!(resolve_agg_field(&archive, "user.name"), "_json.user.name");
        assert_eq!(resolve_agg_field(&archive, "_json.x"), "_json.x");
    }

    /// Partials travel between nodes as JSON (the search response); every
    /// aggregation kind must survive that and merge.
    #[test]
    fn partials_survive_json_between_nodes_and_merge() {
        use tantivy::schema::INDEXED;
        let mut sb = Schema::builder();
        let json = sb.add_json_field(chronik_storage::json_index::JSON_FIELD, chronik_storage::json_index::json_field_options());
        let offset = sb.add_i64_field("offset", INDEXED | tantivy::schema::FAST);
        let index = tantivy::Index::create_in_ram(sb.build());
        chronik_storage::register_analyzer(&index);
        let mut w = index.writer(15_000_000).unwrap();
        for i in 0..90i64 {
            let level = ["INFO", "WARN", "ERROR"][(i % 3) as usize];
            let v = json!({"level": level, "ts": 1_767_225_600_000i64 + i * 1000, "n": i});
            let mut d = tantivy::TantivyDocument::new();
            d.add_i64(offset, i);
            d.add_object(json, chronik_storage::json_index::json_object(v.to_string().as_bytes()).unwrap().into_iter().map(|(k, v)| (k, v.into())).collect());
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();
        let aggs = TopicAggs::prepare(&json!({
            "l": {"terms": {"field": "level"}, "aggs": {"avg_n": {"avg": {"field": "n"}}}},
            "m": {"date_histogram": {"field": "ts", "fixed_interval": "30s"}},
            "s": {"stats": {"field": "n"}},
            "c": {"cardinality": {"field": "level"}},
            "p": {"percentiles": {"field": "n", "percents": [50]}}
        }))
        .unwrap();
        let schema = index.schema();
        let partial = |lo: i64, hi: i64| {
            let q = tantivy::query::RangeQuery::new(
                std::ops::Bound::Included(tantivy::Term::from_field_i64(offset, lo)),
                std::ops::Bound::Excluded(tantivy::Term::from_field_i64(offset, hi)),
            );
            collect(&searcher, &q, Some(aggs.for_schema(&schema).unwrap())).unwrap()
        };
        // Two "nodes", each with half; one goes through JSON as over HTTP.
        let mut a = partial(0, 45);
        let b: Partial = serde_json::from_str(&serde_json::to_string(&partial(45, 90)).unwrap()).unwrap();
        a.merge(b).unwrap();
        assert_eq!(a.count, 90);
        let out: Value = serde_json::from_str(aggs.finalize(a.aggs).unwrap().get()).unwrap();
        let levels: Vec<(String, u64)> = out["l"]["buckets"].as_array().unwrap().iter()
            .map(|b| (b["key"].as_str().unwrap().to_string(), b["doc_count"].as_u64().unwrap())).collect();
        assert_eq!(levels.len(), 3, "{out}");
        assert!(levels.iter().all(|(_, n)| *n == 30), "{levels:?}");
        assert_eq!(out["m"]["buckets"].as_array().unwrap().len(), 3);
        assert_eq!(out["m"]["buckets"][0]["key_as_string"], "2026-01-01T00:00:00.000Z");
        assert_eq!(out["s"]["count"], 90);
        assert_eq!(out["c"]["value"].as_f64().map(|v| v.round()), Some(3.0));

        // Many sources merged by size give what folding them in order gives.
        for n in [1usize, 2, 3, 7, 8, 9, 17] {
            let ranges: Vec<(i64, i64)> = (0..n as i64).map(|i| (i * 90 / n as i64, (i + 1) * 90 / n as i64)).collect();
            let mut folded = Partial::default();
            let mut merger = Merger::default();
            for &(lo, hi) in &ranges {
                folded.merge(partial(lo, hi)).unwrap();
                merger.push(partial(lo, hi)).unwrap();
            }
            let merged = merger.finish().unwrap();
            assert_eq!(merged.count, 90, "n={n}");
            let json = |p: Option<IntermediateAggregationResults>| -> Value { serde_json::from_str(aggs.finalize(p).unwrap().get()).unwrap() };
            assert_eq!(json(merged.aggs), json(folded.aggs), "n={n}");
        }
        assert_eq!(Merger::default().finish().unwrap().count, 0);
    }

    #[test]
    fn exact_counts_only_when_asked() {
        assert!(!wants_exact_count(10, None, false));
        assert!(wants_exact_count(0, None, false));
        assert!(wants_exact_count(10, Some(&json!(true)), false));
        assert!(!wants_exact_count(10, Some(&json!(false)), false));
        assert!(wants_exact_count(10, None, true));
    }
}
