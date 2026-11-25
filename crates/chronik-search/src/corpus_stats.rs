//! BM25 statistics of a topic as a whole, for scoring every index of one
//! search alike.
//!
//! A topic's records are spread over many Tantivy indexes: one archive per
//! sealed WAL segment and the in-memory hot index per partition. Tantivy
//! scores a document with its own index's statistics (documents, tokens per
//! field, documents holding each term), so the same message scored
//! differently in every archive: searching "timeout" over 10M records ranked
//! the newest 20,136 matches (6.43448) below a million-offset-older archive's
//! (6.44069), although they were the same text, and "newest first among equal
//! scores" never applied across indexes. Elasticsearch's
//! `dfs_query_then_fetch` exists for the same reason.
//!
//! [`CorpusStats`] sums the statistics of every index a search reads, and
//! each index scores with the sums ([`CorpusStats::provider`]). Equal texts
//! then score equally wherever they are, and a page filled by newer indexes
//! lets older ones skip every block that cannot beat it (Block-WAND).
//!
//! Terms are looked up by field *name*: indexes written by different versions
//! number their fields differently. A field an index lacks contributes
//! nothing. Each total is computed once per search, on first use.

use std::collections::HashMap;
use std::sync::Mutex;

use tantivy::query::Bm25StatisticsProvider;
use tantivy::schema::{Field, Schema};
use tantivy::{Searcher, Term};

pub struct CorpusStats {
    sources: Vec<Searcher>,
    num_docs: u64,
    tokens: Mutex<HashMap<String, u64>>,
    /// (field name, value bytes) -> documents holding the term.
    doc_freqs: Mutex<HashMap<(String, Vec<u8>), u64>>,
}

impl CorpusStats {
    pub fn new(sources: Vec<Searcher>) -> Self {
        let num_docs = sources
            .iter()
            .flat_map(|s| s.segment_readers())
            .map(|r| u64::from(r.max_doc()))
            .sum();
        Self { sources, num_docs, tokens: Mutex::default(), doc_freqs: Mutex::default() }
    }

    /// The statistics as seen by an index of `schema` (whose field ids the
    /// terms it asks about carry).
    pub fn provider<'a>(&'a self, schema: &'a Schema) -> impl Bm25StatisticsProvider + 'a {
        Provider { corpus: self, schema }
    }

    fn total_tokens(&self, name: &str) -> tantivy::Result<u64> {
        if let Some(&n) = self.tokens.lock().unwrap().get(name) {
            return Ok(n);
        }
        let total = self.sum_over_segments(name, &[], |segment, field| {
            Ok(segment.inverted_index(field)?.total_num_tokens())
        })?;
        self.tokens.lock().unwrap().insert(name.to_string(), total);
        Ok(total)
    }

    fn doc_freq(&self, name: &str, value: &[u8]) -> tantivy::Result<u64> {
        let key = (name.to_string(), value.to_vec());
        if let Some(&n) = self.doc_freqs.lock().unwrap().get(&key) {
            return Ok(n);
        }
        let total = self.sum_over_segments(name, value, |segment, field| {
            Ok(u64::from(segment.inverted_index(field)?.doc_freq(&term_in(field, value))?))
        })?;
        self.doc_freqs.lock().unwrap().insert(key, total);
        Ok(total)
    }

    /// `per_segment` summed over every segment of every source holding field
    /// `name`, each segment's value taken from the process-wide cache when
    /// there (a segment never changes, so neither do its statistics). Without
    /// it every search looked each term up twice in each archive, once here
    /// and once to read its postings: 18% of the CPU of a text query over 48.
    fn sum_over_segments(
        &self,
        name: &str,
        value: &[u8],
        per_segment: impl Fn(&tantivy::SegmentReader, Field) -> tantivy::Result<u64>,
    ) -> tantivy::Result<u64> {
        let segments: Vec<(&tantivy::SegmentReader, Field)> = self
            .sources
            .iter()
            .filter_map(|s| s.schema().get_field(name).ok().map(|f| (s, f)))
            .flat_map(|(s, f)| s.segment_readers().iter().map(move |r| (r, f)))
            .collect();
        let key = |r: &tantivy::SegmentReader| (r.segment_id(), name.to_string(), value.to_vec());
        let mut total = 0u64;
        let mut missing = Vec::new();
        {
            let cache = segment_stats().lock().unwrap();
            for (i, (r, _)) in segments.iter().enumerate() {
                match cache.get(&key(r)) {
                    Some(&n) => total += n,
                    None => missing.push(i),
                }
            }
        }
        let mut found = Vec::with_capacity(missing.len());
        for i in missing {
            let (r, f) = segments[i];
            let n = per_segment(r, f)?;
            total += n;
            found.push((key(r), n));
        }
        if !found.is_empty() {
            let mut cache = segment_stats().lock().unwrap();
            if cache.len() + found.len() > SEGMENT_STATS_ENTRIES {
                cache.clear(); // simple bound: rebuilt from the next searches
            }
            cache.extend(found);
        }
        Ok(total)
    }
}

/// (segment, field name, term value bytes, or empty for the field's token
/// total) -> that statistic of the segment.
type SegmentStats = HashMap<(tantivy::index::SegmentId, String, Vec<u8>), u64>;
const SEGMENT_STATS_ENTRIES: usize = 200_000;

fn segment_stats() -> &'static Mutex<SegmentStats> {
    static CACHE: std::sync::OnceLock<Mutex<SegmentStats>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// The term of `field` holding `value` (its type byte and value bytes, as
/// `Term::value().as_serialized()` gives them).
fn term_in(field: Field, value: &[u8]) -> Term {
    let mut serialized = Vec::with_capacity(4 + value.len());
    serialized.extend_from_slice(&field.field_id().to_be_bytes());
    serialized.extend_from_slice(value);
    #[allow(deprecated)] // the typed API has no "same value, other field"
    Term::wrap(&serialized)
}

struct Provider<'a> {
    corpus: &'a CorpusStats,
    schema: &'a Schema,
}

impl Bm25StatisticsProvider for Provider<'_> {
    fn total_num_tokens(&self, field: Field) -> tantivy::Result<u64> {
        self.corpus.total_tokens(self.schema.get_field_name(field))
    }

    fn total_num_docs(&self) -> tantivy::Result<u64> {
        Ok(self.corpus.num_docs)
    }

    fn doc_freq(&self, term: &Term) -> tantivy::Result<u64> {
        self.corpus.doc_freq(self.schema.get_field_name(term.field()), term.value().as_serialized())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::collector::TopDocs;
    use tantivy::query::QueryParser;
    use tantivy::schema::{TEXT, STRING};

    /// Two indexes with different field numbering and different contents:
    /// the same document scores alike in both once they share statistics,
    /// and the shared numbers are those of one index holding everything.
    #[test]
    fn the_same_text_scores_alike_in_every_index() {
        let make = |extra_first: bool, docs: &[&str]| {
            let mut sb = Schema::builder();
            if extra_first {
                sb.add_text_field("other", STRING);
            }
            let body = sb.add_text_field("body", TEXT);
            let index = tantivy::Index::create_in_ram(sb.build());
            let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
            for d in docs {
                w.add_document(tantivy::doc!(body => *d)).unwrap();
            }
            w.commit().unwrap();
            index
        };
        let a_docs = ["timeout on read", "connection refused", "timeout on write", "ok"];
        let b_docs = ["timeout on read", "all good", "fine", "fine again", "still fine"];
        let (a, b) = (make(false, &a_docs), make(true, &b_docs));
        let all = make(false, &[&a_docs[..], &b_docs[..]].concat());
        let (sa, sb, sall) = (a.reader().unwrap().searcher(), b.reader().unwrap().searcher(), all.reader().unwrap().searcher());

        let top = |index: &tantivy::Index, searcher: &Searcher, stats: Option<&CorpusStats>| -> f32 {
            let field = index.schema().get_field("body").unwrap();
            let q = QueryParser::for_index(index, vec![field]).parse_query("timeout read").unwrap();
            let c = TopDocs::with_limit(1).order_by_score();
            let hits = match stats {
                Some(s) => searcher.search_with_statistics_provider(&q, &c, &s.provider(searcher.schema())).unwrap(),
                None => searcher.search(&q, &c).unwrap(),
            };
            hits[0].0
        };
        assert_ne!(top(&a, &sa, None), top(&b, &sb, None), "own statistics differ");
        let corpus = CorpusStats::new(vec![sa.clone(), sb.clone()]);
        let (in_a, in_b) = (top(&a, &sa, Some(&corpus)), top(&b, &sb, Some(&corpus)));
        assert_eq!(in_a, in_b);
        assert_eq!(in_a, top(&all, &sall, None), "as if one index held both");
    }

    #[test]
    fn a_field_an_index_lacks_counts_nothing() {
        let mut sb = Schema::builder();
        let body = sb.add_text_field("body", TEXT);
        let a = tantivy::Index::create_in_ram(sb.build());
        let mut w: tantivy::IndexWriter = a.writer_with_num_threads(1, 15_000_000).unwrap();
        w.add_document(tantivy::doc!(body => "timeout")).unwrap();
        w.commit().unwrap();
        let mut sb = Schema::builder();
        let other = sb.add_text_field("other", TEXT);
        let b = tantivy::Index::create_in_ram(sb.build());
        let mut w: tantivy::IndexWriter = b.writer_with_num_threads(1, 15_000_000).unwrap();
        w.add_document(tantivy::doc!(other => "timeout")).unwrap();
        w.commit().unwrap();
        let sa = a.reader().unwrap().searcher();
        let corpus = CorpusStats::new(vec![sa.clone(), b.reader().unwrap().searcher()]);
        let p = corpus.provider(sa.schema());
        assert_eq!(p.total_num_docs().unwrap(), 2);
        assert_eq!(p.doc_freq(&Term::from_field_text(body, "timeout")).unwrap(), 1);
        assert_eq!(p.total_num_tokens(body).unwrap(), 1);
    }
}
