//! Plain Tantivy on the OpenSearch comparison corpus (tests/os_adaptor/bench):
//! a typed timeseries schema, multi-threaded indexing, the same queries.
//! Answers "is a Tantivy index slower than OpenSearch?" apart from how
//! Chronik uses it. In-process: no HTTP or JSON request parsing.
//!
//!   cargo run --release -p chronik-search --example plain_tantivy_bench -- \
//!     <corpus-dir> <index-dir> [threads=8] [seconds=60]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tantivy::aggregation::agg_req::Aggregations;
use tantivy::aggregation::AggregationCollector;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Schema, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::{DateTime, Index, IndexWriter, ReloadPolicy, TantivyDocument, Term};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let corpus = Path::new(&args[1]);
    let dir = Path::new(&args[2]);
    let threads: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    let seconds: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;

    let mut sb = Schema::builder();
    let ts = sb.add_date_field("ts", INDEXED | FAST);
    let level = sb.add_text_field("level", STRING | FAST);
    let service = sb.add_text_field("service", STRING | FAST);
    let host = sb.add_text_field("host", STRING | FAST);
    let status = sb.add_i64_field("status", INDEXED | FAST);
    let latency = sb.add_i64_field("latency_ms", FAST);
    let message = sb.add_text_field("message", TEXT);
    let source = sb.add_text_field("_source", STORED);
    let schema = sb.build();
    let index = Index::create_in_dir(dir, schema)?;

    // ---- indexing ----
    let mut files: Vec<_> = std::fs::read_dir(corpus)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "ndjson"))
        .collect();
    files.sort();
    let mut writer: IndexWriter = index.writer(1_000_000_000)?;
    let t0 = Instant::now();
    let mut docs = 0u64;
    for f in &files {
        for line in std::fs::read_to_string(f)?.lines() {
            if line.starts_with("{\"index\"") {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(line)?;
            let mut d = TantivyDocument::new();
            d.add_date(ts, DateTime::from_timestamp_millis(v["ts"].as_i64().unwrap_or(0)));
            d.add_text(level, v["level"].as_str().unwrap_or(""));
            d.add_text(service, v["service"].as_str().unwrap_or(""));
            d.add_text(host, v["host"].as_str().unwrap_or(""));
            d.add_i64(status, v["status"].as_i64().unwrap_or(0));
            d.add_i64(latency, v["latency_ms"].as_i64().unwrap_or(0));
            d.add_text(message, v["message"].as_str().unwrap_or(""));
            d.add_text(source, line);
            writer.add_document(d)?;
            docs += 1;
        }
    }
    writer.commit()?;
    let index_s = t0.elapsed().as_secs_f64();
    let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
    // Freshness: one more document, commit, reload, until a search finds it.
    let t1 = Instant::now();
    let mut d = TantivyDocument::new();
    d.add_date(ts, DateTime::from_timestamp_millis(1767225600000));
    d.add_text(message, "zzfinalmarkerzz");
    writer.add_document(d)?;
    writer.commit()?;
    reader.reload()?;
    let marker = TermQuery::new(Term::from_field_text(message, "zzfinalmarkerzz"), IndexRecordOption::Basic);
    let found = reader.searcher().search(&marker, &Count)?;
    let fresh_ms = t1.elapsed().as_secs_f64() * 1000.0;
    let segments_before = reader.searcher().segment_readers().len();
    writer.wait_merging_threads()?;
    reader.reload()?;
    let searcher = reader.searcher();
    println!(
        "indexed {docs} docs in {index_s:.1}s ({:.0} docs/s); marker searchable in {fresh_ms:.0} ms (found {found}); \
         segments {segments_before} -> {} after merges; index {} MB",
        docs as f64 / index_s,
        searcher.segment_readers().len(),
        dir_mb(dir)
    );

    // ---- correctness: same selective totals as the OpenSearch comparison ----
    let qp = QueryParser::for_index(&index, vec![message]);
    let term = |f, t: &str| -> Box<dyn Query> { Box::new(TermQuery::new(Term::from_field_text(f, t), IndexRecordOption::Basic)) };
    let text = |t: &str| -> Box<dyn Query> { qp.parse_query(t).unwrap() };
    let and = |qs: Vec<Box<dyn Query>>| -> Box<dyn Query> { Box::new(BooleanQuery::new(qs.into_iter().map(|q| (Occur::Must, q)).collect())) };
    for (name, q) in [
        ("refused + svc-3", and(vec![text("refused"), term(service, "svc-3")])),
        ("host-07 + ERROR", and(vec![term(host, "host-07"), term(level, "ERROR")])),
        ("timeout + svc-11 + WARN", and(vec![text("timeout"), term(service, "svc-11"), term(level, "WARN")])),
    ] {
        println!("  answer {name}: {}", searcher.search(&*q, &Count)?);
    }
    let t0ms = 1767225600000i64;
    let range = RangeQuery::new(
        std::ops::Bound::Included(Term::from_field_date(ts, DateTime::from_timestamp_millis(t0ms + 600_000))),
        std::ops::Bound::Included(Term::from_field_date(ts, DateTime::from_timestamp_millis(t0ms + 1_200_000))),
    );
    let terms_agg: Aggregations = serde_json::from_value(serde_json::json!({"s": {"terms": {"field": "service", "size": 3}}}))?;
    let hist_agg: Aggregations = serde_json::from_value(serde_json::json!({"m": {"date_histogram": {"field": "ts", "fixed_interval": "1m"}}}))?;
    let r = searcher.search(&range, &AggregationCollector::from_aggs(terms_agg.clone(), Default::default()))?;
    println!("  range + terms(service) top 3: {}", serde_json::to_string(&r)?.chars().take(200).collect::<String>());
    let r = searcher.search(&AllQuery, &AggregationCollector::from_aggs(hist_agg.clone(), Default::default()))?;
    let v = serde_json::to_value(&r)?;
    let sum: u64 = v["m"]["buckets"].as_array().map(|b| b.iter().filter_map(|x| x["doc_count"].as_u64()).sum()).unwrap_or(0);
    println!("  date_histogram 1m: {} buckets, sum {sum}", v["m"]["buckets"].as_array().map(|b| b.len()).unwrap_or(0));

    // ---- load: the benchmark's query mix, `threads` threads for `seconds` ----
    let queries: Vec<(&str, Box<dyn Query>, bool)> = vec![
        ("match_text", text("timeout"), false),
        ("term_level", term(level, "ERROR"), false),
        ("bool_text_filter", and(vec![text("connection"), term(service, "svc-3")]), false),
        ("match_all", Box::new(AllQuery), false),
        ("range_terms_agg", Box::new(range.clone()), true),
        ("date_histogram", Box::new(AllQuery), true),
    ];
    let queries = Arc::new(queries);
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..threads {
        let (queries, stop, reader) = (queries.clone(), stop.clone(), reader.clone());
        let (terms_agg, hist_agg) = (terms_agg.clone(), hist_agg.clone());
        handles.push(std::thread::spawn(move || {
            let mut lat: Vec<Vec<u64>> = vec![Vec::new(); queries.len()];
            while !stop.load(Ordering::Relaxed) {
                let searcher = reader.searcher();
                for (i, (name, q, agg)) in queries.iter().enumerate() {
                    let t = Instant::now();
                    if *agg {
                        let aggs = if *name == "date_histogram" { hist_agg.clone() } else { terms_agg.clone() };
                        let _ = searcher.search(&**q, &AggregationCollector::from_aggs(aggs, Default::default())).unwrap();
                    } else {
                        // Top 10 with their stored _source, as a search API returns them.
                        let top = searcher.search(&**q, &(TopDocs::with_limit(10).order_by_score(), Count)).unwrap();
                        for (_, addr) in top.0 {
                            let _: TantivyDocument = searcher.doc(addr).unwrap();
                        }
                    }
                    lat[i].push(t.elapsed().as_micros() as u64);
                }
            }
            lat
        }));
    }
    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut all: Vec<Vec<u64>> = vec![Vec::new(); queries.len()];
    for h in handles {
        for (i, v) in h.join().unwrap().into_iter().enumerate() {
            all[i].extend(v);
        }
    }
    println!("load: {threads} threads, {seconds}s");
    for (i, (name, _, _)) in queries.iter().enumerate() {
        let v = &mut all[i];
        v.sort_unstable();
        let p = |q: f64| v[((v.len() as f64 - 1.0) * q) as usize] as f64 / 1000.0;
        println!("  {name:18} n={:7} p50={:.2}ms p95={:.2}ms p99={:.2}ms", v.len(), p(0.50), p(0.95), p(0.99));
    }
    Ok(())
}

fn dir_mb(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|it| it.filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum::<u64>() / 1_048_576)
        .unwrap_or(0)
}
