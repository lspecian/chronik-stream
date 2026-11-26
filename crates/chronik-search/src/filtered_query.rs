//! A bool query's `filter` clauses: they restrict the hits and add nothing to
//! their scores, as in Elasticsearch.
//!
//! Up to v2.20.1 a filter clause was added as one more `must`, so its own
//! BM25 score was added to every hit's: `match: connection` filtered by
//! `term: service` ranked by how rare the service was too. Here the scoring
//! clauses are searched with Tantivy's pruning (Block-WAND skips what cannot
//! beat the page) and each filter is checked only for the documents that get
//! past it, by seeking the filter's postings to them. With archives kept
//! newest first (`ranking`), once the page is full the scoring clause skips
//! whole blocks and the filters are hardly read. A `BooleanQuery` of
//! `must` + `ConstScore(filter, 0)` gives the same answers but steps through
//! every match of the intersection.

use tantivy::query::{EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, DocSet, Score, SegmentReader, TERMINATED};

#[derive(Debug)]
pub struct FilteredQuery {
    scoring: Box<dyn Query>,
    filters: Vec<Box<dyn Query>>,
}

impl Clone for FilteredQuery {
    fn clone(&self) -> Self {
        Self { scoring: self.scoring.box_clone(), filters: self.filters.iter().map(|f| f.box_clone()).collect() }
    }
}

impl FilteredQuery {
    pub fn new(scoring: Box<dyn Query>, filters: Vec<Box<dyn Query>>) -> Self {
        Self { scoring, filters }
    }
}

impl Query for FilteredQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let scoring = self.scoring.weight(enable_scoring)?;
        let unscored = match enable_scoring.searcher() {
            Some(searcher) => EnableScoring::disabled_from_searcher(searcher),
            None => enable_scoring,
        };
        let filters = self.filters.iter().map(|f| f.weight(unscored)).collect::<tantivy::Result<Vec<_>>>()?;
        Ok(Box::new(FilteredWeight { scoring, filters }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a tantivy::Term, bool)) {
        self.scoring.query_terms(visitor);
        for f in &self.filters {
            f.query_terms(visitor);
        }
    }
}

struct FilteredWeight {
    scoring: Box<dyn Weight>,
    filters: Vec<Box<dyn Weight>>,
}

/// A filter's matches, scoring nothing.
struct Unscored(Box<dyn Scorer>);

impl DocSet for Unscored {
    fn advance(&mut self) -> DocId {
        self.0.advance()
    }
    fn seek(&mut self, target: DocId) -> DocId {
        self.0.seek(target)
    }
    fn doc(&self) -> DocId {
        self.0.doc()
    }
    fn size_hint(&self) -> u32 {
        self.0.size_hint()
    }
}

impl Scorer for Unscored {
    fn score(&mut self) -> Score {
        0.0
    }
}

/// Whether every filter matches `doc`; documents are asked in increasing order.
fn all_match(filters: &mut [Box<dyn Scorer>], doc: DocId) -> bool {
    filters.iter_mut().all(|f| {
        let at = f.doc();
        at == doc || (at < doc && at != TERMINATED && f.seek(doc) == doc)
    })
}

impl Weight for FilteredWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let mut scorers = vec![self.scoring.scorer(reader, boost)?];
        for f in &self.filters {
            scorers.push(Box::new(Unscored(f.scorer(reader, 1.0)?)));
        }
        Ok(tantivy::query::intersect_scorers(scorers, reader.num_docs()))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut filters = self.filters.iter().map(|f| f.scorer(reader, 1.0)).collect::<tantivy::Result<Vec<_>>>()?;
        if !all_match(&mut filters, doc) {
            return Err(tantivy::TantivyError::InvalidArgument(format!("document #{doc} does not match a filter")));
        }
        self.scoring.explain(reader, doc)
    }

    fn for_each_pruning(
        &self,
        threshold: Score,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(DocId, Score) -> Score,
    ) -> tantivy::Result<()> {
        let mut filters = self.filters.iter().map(|f| f.scorer(reader, 1.0)).collect::<tantivy::Result<Vec<_>>>()?;
        let mut current = threshold;
        self.scoring.for_each_pruning(threshold, reader, &mut |doc, score| {
            if all_match(&mut filters, doc) {
                current = callback(doc, score);
            }
            current
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::collector::TopDocs;
    use tantivy::query::{BooleanQuery, ConstScoreQuery, Occur, TermQuery};
    use tantivy::schema::{IndexRecordOption, Schema, STRING, TEXT};

    /// Same documents and scores as `must` + `ConstScore(filter, 0)` (the
    /// definition), over several segments with deletions, ranked with
    /// pruning (`ranking::top_docs`) and through the plain scorer.
    #[test]
    fn filters_restrict_without_scoring() {
        let mut sb = Schema::builder();
        let body = sb.add_text_field("body", TEXT);
        let tag = sb.add_text_field("tag", STRING);
        let index = tantivy::Index::create_in_ram(sb.build());
        let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for i in 0..6_000u32 {
            let text = format!("{} {}", "connection ".repeat((i % 3 + 1) as usize), "pad ".repeat((i % 5) as usize));
            let t = ["svc-1", "svc-2", "svc-3"][(i % 7 % 3) as usize];
            w.add_document(tantivy::doc!(body => text, tag => t)).unwrap();
            if i % 1_500 == 1_499 {
                w.commit().unwrap();
            }
        }
        w.delete_term(tantivy::Term::from_field_text(tag, "svc-2"));
        w.add_document(tantivy::doc!(body => "connection", tag => "svc-2")).unwrap();
        w.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();
        let term = |f, s: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(tantivy::Term::from_field_text(f, s), IndexRecordOption::WithFreqs))
        };
        for (filter_tag, extra_filter) in [("svc-3", None), ("svc-2", None), ("svc-1", Some("pad"))] {
            let mut filters = vec![term(tag, filter_tag)];
            let mut reference_clauses: Vec<(Occur, Box<dyn Query>)> = vec![
                (Occur::Must, term(body, "connection")),
                (Occur::Must, Box::new(ConstScoreQuery::new(term(tag, filter_tag), 0.0))),
            ];
            if let Some(word) = extra_filter {
                filters.push(term(body, word));
                reference_clauses.push((Occur::Must, Box::new(ConstScoreQuery::new(term(body, word), 0.0))));
            }
            let filtered = FilteredQuery::new(term(body, "connection"), filters);
            let reference = BooleanQuery::new(reference_clauses);
            for limit in [1, 10, 500, 10_000] {
                let want = searcher.search(&reference, &TopDocs::with_limit(limit).order_by_score()).unwrap();
                let plain = searcher.search(&filtered, &TopDocs::with_limit(limit).order_by_score()).unwrap();
                assert_eq!(plain, want, "{filter_tag} {extra_filter:?} limit={limit}");
            }
            let count = searcher.search(&filtered, &tantivy::collector::Count).unwrap();
            assert_eq!(count, searcher.search(&reference, &tantivy::collector::Count).unwrap());
        }
    }
}
