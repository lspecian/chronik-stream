//! One order for search hits, everywhere a search assembles them.
//!
//! A search answer is put together from many sources: the hot index (one
//! Tantivy index per partition), every archived segment, and every node of the
//! cluster. Paging (`from`/`size`) is only correct if each source returns its
//! top `from + size` hits in the SAME order the final merge uses, and the
//! final merge skips `from` once. Before v2.17.3 each archived segment skipped
//! its own first `from` hits, the hot index ignored `from`, nobody skipped it on
//! the merged list, and ties (every hit of `match_all`) were broken
//! differently by every source — so page 2 repeated page 1 and dropped records
//! that belonged on it.
//!
//! The order: score (highest first), then timestamp (newest first), then
//! topic, partition, offset (highest first), then `_id` — total, so the same
//! request always yields the same pages.

use crate::api::Hit;
use std::cmp::Ordering;
use tantivy::collector::TopDocs;
use tantivy::query::Query;
use tantivy::{DocAddress, DocId, Score, Searcher, SegmentReader};

/// The per-document rank inside one Tantivy index; compared field by field,
/// highest wins (TopDocs keeps the largest keys). `neg_partition` puts lower
/// partitions first among equal timestamps, as [`hit_order`] does.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
struct RankKey {
    score: Score,
    timestamp_s: i64,
    neg_partition: i64,
    offset: i64,
}

/// The fast fields an index keeps the rank in, found from its schema.
#[derive(Clone, Copy)]
struct RankFields {
    timestamp: &'static str,
    /// `timestamp` is a date field (else i64 milliseconds).
    timestamp_is_date: bool,
    offset: &'static str,
    /// Absent when the index holds a single partition.
    partition: Option<&'static str>,
}

impl RankFields {
    /// The layouts written by Chronik, newest first: an archived segment
    /// (`_ts`, `_offset`, one partition), a hot partition index (`timestamp`,
    /// `offset`, one partition), and a leftover RealtimeIndexer index
    /// (`_timestamp`, `_offset`, `_partition` — all partitions of a topic).
    fn detect(schema: &tantivy::schema::Schema) -> Option<Self> {
        let fast = |name: &str| {
            schema.get_field(name).map(|f| schema.get_field_entry(f).is_fast()).unwrap_or(false)
        };
        let candidates = [
            RankFields { timestamp: "_ts", timestamp_is_date: true, offset: "_offset", partition: None },
            RankFields {
                timestamp: "_timestamp",
                timestamp_is_date: true,
                offset: "_offset",
                partition: Some("_partition"),
            },
            RankFields { timestamp: "timestamp", timestamp_is_date: false, offset: "offset", partition: None },
        ];
        candidates.into_iter().find(|c| {
            fast(c.timestamp) && fast(c.offset) && c.partition.map_or(true, |p| fast(p))
        })
    }
}

/// The top `limit` documents of `query` in the order of [`hit_order`].
///
/// Falls back to plain relevance when the index is of no layout Chronik
/// writes (a REST-created index), which is still deterministic per index.
pub fn top_docs(
    searcher: &Searcher,
    query: &dyn Query,
    limit: usize,
) -> tantivy::Result<Vec<(Score, DocAddress)>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let Some(fields) = RankFields::detect(searcher.schema()) else {
        return searcher.search(query, &TopDocs::with_limit(limit).order_by_score());
    };
    type Getter = Box<dyn Fn(DocId) -> i64 + Send + Sync>;
    let i64_col = |ff: &tantivy::fastfield::FastFieldReaders, name: &str| -> Getter {
        match ff.i64(name) {
            Ok(col) => {
                let col = col.first_or_default_col(0);
                Box::new(move |doc| col.get_val(doc))
            }
            Err(_) => Box::new(|_| 0),
        }
    };
    let collector = TopDocs::with_limit(limit).tweak_score(move |segment: &SegmentReader| {
        let ff = segment.fast_fields();
        let ts: Getter = if fields.timestamp_is_date {
            match ff.date(fields.timestamp) {
                Ok(col) => {
                    let col = col.first_or_default_col(tantivy::DateTime::from_timestamp_millis(0));
                    Box::new(move |doc| col.get_val(doc).into_timestamp_secs())
                }
                Err(_) => Box::new(|_| 0),
            }
        } else {
            let ms = i64_col(ff, fields.timestamp);
            Box::new(move |doc| ms(doc).div_euclid(1000))
        };
        let off = i64_col(ff, fields.offset);
        let part: Getter = match fields.partition {
            Some(p) => i64_col(ff, p),
            None => Box::new(|_| 0),
        };
        move |doc: DocId, score: Score| RankKey {
            score,
            timestamp_s: ts(doc),
            neg_partition: -part(doc),
            offset: off(doc),
        }
    });
    Ok(searcher
        .search(query, &collector)?
        .into_iter()
        .map(|(key, addr)| (key.score, addr))
        .collect())
}

/// A hit's timestamp in whole seconds, the granularity the order uses.
///
/// Archived and RealtimeIndexer indexes keep their timestamps in Tantivy date
/// fast fields, which store seconds; ranking inside them by the second while
/// the merge compared milliseconds made the two orders disagree whenever
/// records shared a second (every producer batch), and pages overlapped. So
/// every source and the merge rank by the second, and partition and offset
/// break the tie. `_source.timestamp` itself stays in milliseconds.
pub fn rank_second(hit: &Hit) -> Option<i64> {
    hit._source.get("timestamp").and_then(|v| v.as_i64()).map(|ms| ms.div_euclid(1000))
}

/// The order of hits across sources: see the module docs. Hits whose
/// `_source` lacks `timestamp`/`partition`/`offset` (documents of REST-created
/// indices) fall through to `_index` and `_id`.
pub fn hit_order(a: &Hit, b: &Hit) -> Ordering {
    fn num(h: &Hit, k: &str) -> Option<i64> {
        h._source.get(k).and_then(|v| v.as_i64())
    }
    let score = |h: &Hit| h._score.unwrap_or(0.0);
    score(b)
        .total_cmp(&score(a))
        .then_with(|| rank_second(b).cmp(&rank_second(a)))
        .then_with(|| a._index.cmp(&b._index))
        .then_with(|| num(a, "partition").cmp(&num(b, "partition")))
        .then_with(|| num(b, "offset").cmp(&num(a, "offset")))
        .then_with(|| a._id.cmp(&b._id))
}

/// Largest `from + size` a search accepts (`CHRONIK_SEARCH_MAX_RESULT_WINDOW`,
/// default 10,000 — Elasticsearch's `index.max_result_window`). Every source
/// collects that many hits, on every node, so a deep page is costly for the
/// whole cluster; beyond it a client should page with a filter instead.
pub fn max_result_window() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("CHRONIK_SEARCH_MAX_RESULT_WINDOW")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(10_000)
    })
}

/// `Err(reason)` when `from + size` is over [`max_result_window`], worded as
/// Elasticsearch words it.
pub fn check_result_window(from: usize, size: usize) -> Result<(), String> {
    let window = from.saturating_add(size);
    let max = max_result_window();
    if window > max {
        return Err(format!(
            "Result window is too large, from + size must be less than or equal to: [{max}] but was [{window}]. \
             See CHRONIK_SEARCH_MAX_RESULT_WINDOW."
        ));
    }
    Ok(())
}

/// One page of `hits`: sorted by [`hit_order`], each record once (by `_index`
/// and `_id`, keeping its best-ranked copy — a record can come from both a
/// leftover RealtimeIndexer index and an archive), `from` skipped, `size`
/// kept. Returns the page and how many distinct candidates there were.
pub fn page(mut hits: Vec<Hit>, from: usize, size: usize) -> (Vec<Hit>, usize) {
    hits.sort_by(hit_order);
    let mut seen = std::collections::HashSet::with_capacity(hits.len());
    hits.retain(|h| seen.insert((h._index.clone(), h._id.clone())));
    let found = hits.len();
    (hits.into_iter().skip(from).take(size).collect(), found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(index: &str, score: f32, ts: i64, p: i64, off: i64) -> Hit {
        Hit {
            _index: index.to_string(),
            _id: format!("{p}-{off}"),
            _score: Some(score),
            _source: serde_json::json!({"timestamp": ts * 1000, "partition": p, "offset": off}),
            highlight: None,
        }
    }

    #[test]
    fn score_then_newest_then_position() {
        let mut v = vec![
            hit("t", 1.0, 100, 0, 1),
            hit("t", 2.0, 50, 0, 2),
            hit("t", 1.0, 200, 1, 9),
            hit("t", 1.0, 100, 0, 3),
            hit("a", 1.0, 100, 2, 0),
        ];
        v.sort_by(hit_order);
        let ids: Vec<_> = v.iter().map(|h| format!("{}/{}", h._index, h._id)).collect();
        assert_eq!(ids, ["t/0-2", "t/1-9", "a/2-0", "t/0-3", "t/0-1"]);
    }

    #[test]
    fn pages_tile_the_list() {
        let all: Vec<Hit> = (0..25).map(|i| hit("t", 1.0, 1000 + (i % 7), i % 3, i)).collect();
        let (whole, n) = page(all.clone(), 0, 100);
        assert_eq!(n, 25);
        let mut tiled = Vec::new();
        for from in (0..25).step_by(10) {
            tiled.extend(page(all.clone(), from, 10).0);
        }
        let ids = |v: &[Hit]| v.iter().map(|h| h._id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&tiled), ids(&whole));
    }

    #[test]
    fn window_cap() {
        assert!(check_result_window(9_990, 10).is_ok());
        assert!(check_result_window(9_991, 10).unwrap_err().contains("[10001]"));
        assert!(check_result_window(usize::MAX, 10).is_err());
    }
}
