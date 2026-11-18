//! Merge `/_query` answers from the nodes of a cluster.
//!
//! Every backend `/_query` uses answers from the node it runs on: the vector
//! and hot text indexes are per node, and the SQL tables serve only the
//! partitions the node leads. A single node's answer is therefore a fraction of
//! the cluster's, returned as if complete. The server fans the request out to
//! every peer and merges here.
//!
//! The same record can come back from several nodes (each replica embeds and
//! indexes it), so hits are de-duplicated by `(topic, partition, offset)`,
//! keeping the best score and the union of the backends that found it. The
//! merged list is re-sorted by score and cut to `k`, per topic for grouped
//! results.

use crate::types::{QueryResponse, RankedResult};
use std::collections::HashMap;

/// Merge `local` with the peers' answers. `incomplete_nodes` on the result is
/// the caller's to fill (it knows which peers failed).
pub fn merge_responses(local: QueryResponse, peers: Vec<QueryResponse>, k: usize) -> QueryResponse {
    if peers.is_empty() {
        return local;
    }
    let mut stats = local.stats.clone();
    let mut all_results = local.results;
    let mut grouped = local.grouped_results;
    let mut incomplete = local.incomplete_nodes;
    for peer in peers {
        stats.candidates += peer.stats.candidates;
        stats.latency_ms = stats.latency_ms.max(peer.stats.latency_ms);
        stats.backends.extend(peer.stats.backends);
        all_results.extend(peer.results);
        incomplete.extend(peer.incomplete_nodes);
        if let Some(peer_groups) = peer.grouped_results {
            let groups = grouped.get_or_insert_with(HashMap::new);
            for (topic, hits) in peer_groups {
                groups.entry(topic).or_default().extend(hits);
            }
        }
    }
    let results = rank(all_results, k);
    let grouped_results = grouped.map(|groups| {
        groups.into_iter().map(|(topic, hits)| (topic, rank(hits, k))).collect::<HashMap<_, _>>()
    });
    stats.ranked = grouped_results
        .as_ref()
        .map(|g| g.values().map(Vec::len).sum())
        .unwrap_or(results.len());
    QueryResponse {
        query_id: local.query_id,
        results,
        grouped_results,
        stats,
        incomplete_nodes: incomplete,
    }
}

/// De-duplicate by record, best score first, at most `k`.
fn rank(hits: Vec<RankedResult>, k: usize) -> Vec<RankedResult> {
    let mut by_record: HashMap<(String, i32, i64), RankedResult> = HashMap::new();
    for hit in hits {
        let key = (hit.topic.clone(), hit.partition, hit.offset);
        match by_record.get_mut(&key) {
            Some(kept) => {
                let mut backends = std::mem::take(&mut kept.backends);
                backends.extend(hit.backends.iter().cloned());
                if hit.final_score > kept.final_score {
                    let preview = kept.text_preview.take();
                    *kept = hit;
                    if kept.text_preview.is_none() {
                        kept.text_preview = preview;
                    }
                } else if kept.text_preview.is_none() {
                    kept.text_preview = hit.text_preview;
                }
                backends.sort();
                backends.dedup();
                kept.backends = backends;
            }
            None => {
                by_record.insert(key, hit);
            }
        }
    }
    let mut ranked: Vec<RankedResult> = by_record.into_values().collect();
    ranked.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (&a.topic, a.partition, a.offset).cmp(&(&b.topic, b.partition, b.offset)))
    });
    ranked.truncate(k);
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BackendStats, QueryStats};

    fn hit(partition: i32, offset: i64, score: f64, backend: &str) -> RankedResult {
        RankedResult {
            topic: "t".into(),
            partition,
            offset,
            final_score: score,
            features: HashMap::new(),
            explanation: None,
            text_preview: None,
            backends: vec![backend.into()],
        }
    }

    fn response(results: Vec<RankedResult>) -> QueryResponse {
        QueryResponse {
            query_id: "q".into(),
            stats: QueryStats {
                candidates: results.len(),
                ranked: results.len(),
                latency_ms: 5,
                backends: vec![BackendStats {
                    backend: "text".into(),
                    topic: "t".into(),
                    candidates: results.len(),
                    latency_ms: 5,
                    timed_out: false,
                }],
            },
            results,
            grouped_results: None,
            incomplete_nodes: Vec::new(),
        }
    }

    #[test]
    fn each_nodes_partitions_reach_the_answer() {
        // Node 1 leads partition 0, node 2 partition 1, node 3 partition 2.
        let merged = merge_responses(
            response(vec![hit(0, 1, 0.5, "sql")]),
            vec![response(vec![hit(1, 7, 0.9, "sql")]), response(vec![hit(2, 3, 0.7, "sql")])],
            10,
        );
        let got: Vec<(i32, i64)> = merged.results.iter().map(|r| (r.partition, r.offset)).collect();
        assert_eq!(got, vec![(1, 7), (2, 3), (0, 1)]);
        assert_eq!(merged.stats.candidates, 3);
        assert_eq!(merged.stats.ranked, 3);
        assert_eq!(merged.stats.backends.len(), 3);
    }

    #[test]
    fn a_record_found_on_several_replicas_is_counted_once() {
        let merged = merge_responses(
            response(vec![hit(0, 1, 0.4, "vector")]),
            vec![response(vec![hit(0, 1, 0.6, "text")]), response(vec![hit(0, 1, 0.5, "vector")])],
            10,
        );
        assert_eq!(merged.results.len(), 1);
        assert_eq!(merged.results[0].final_score, 0.6);
        assert_eq!(merged.results[0].backends, vec!["text".to_string(), "vector".to_string()]);
    }

    #[test]
    fn the_merge_keeps_only_k() {
        let peers = (0..3).map(|n| response((0..5).map(|o| hit(n, o, (n * 10 + o as i32) as f64, "text")).collect())).collect();
        let merged = merge_responses(response(vec![]), peers, 4);
        assert_eq!(merged.results.len(), 4);
        assert_eq!(merged.results[0].final_score, 24.0);
    }

    #[test]
    fn a_preview_from_any_copy_is_kept() {
        let mut with_text = hit(0, 1, 0.1, "text");
        with_text.text_preview = Some("hello".into());
        let merged = merge_responses(response(vec![hit(0, 1, 0.9, "vector")]), vec![response(vec![with_text])], 10);
        assert_eq!(merged.results[0].text_preview.as_deref(), Some("hello"));
    }
}
