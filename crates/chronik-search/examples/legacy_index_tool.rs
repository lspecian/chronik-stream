//! Tools for the indexes the removed RealtimeIndexer left in `{data_dir}/index`.
//!
//!   report <data_dir> [other nodes' segment_index.json ...]
//!       How many of each old index's records no archive covers (read-only).
//!       Extra catalogues count other nodes' archives as cover too: a cluster
//!       search fans out to them.
//!   make-legacy <index_dir> <topic> <partition> <lo> <hi>
//!       Write an index in the RealtimeIndexer's format (its schema, copied
//!       from 08b1c58^) holding offsets lo..=hi — for tests.
//!   drop-archive <segment_index.json> <topic> <partition> <offset>
//!       Remove the archive holding `offset` from a stopped node's catalogue
//!       and print its object path — for tests that need a gap.
//!
//! cargo run --release -p chronik-search --example legacy_index_tool -- <command> ...

use chronik_search::legacy_index::{coverage, legacy_index_dirs, read_index};
use chronik_storage::segment_index::SegmentIndex;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("report") => report(&args[1..]).await,
        Some("make-legacy") => make_legacy(&args[1..]),
        Some("drop-archive") => drop_archive(&args[1..]).await,
        _ => {
            eprintln!("usage: legacy_index_tool report|make-legacy|drop-archive ... (see the source)");
            std::process::exit(2);
        }
    }
}

async fn report(args: &[String]) {
    let data = PathBuf::from(args.first().expect("report <data_dir> [segment_index.json ...]"));
    let index = SegmentIndex::with_persistence(data.join("segment_index.json"), false);
    index.load().await.expect("segment_index.json");
    let mut others = Vec::new();
    for p in &args[1..] {
        let o = SegmentIndex::with_persistence(PathBuf::from(p), false);
        o.load().await.expect("other segment_index.json");
        others.push(o);
    }
    let (mut docs, mut uncovered, mut bytes) = (0u64, 0u64, 0u64);
    println!("{:<44} {:>10} {:>12} {:>10}  uncovered (partition:lo-hi)", "topic", "MB", "records", "uncovered");
    for (dir_name, dir) in legacy_index_dirs(&data) {
        let contents = match read_index(&dir_name, &dir) {
            Ok(c) => c,
            Err(e) => {
                println!("{dir_name:<44} not analysable: {e}");
                continue;
            }
        };
        let topic = contents.topic.clone();
        let mut archives = index.get_segments_for_topic(&topic).await.unwrap_or_default();
        for o in &others {
            archives.extend(o.get_segments_for_topic(&topic).await.unwrap_or_default());
        }
        let r = coverage(&contents, &archives);
        let ranges: Vec<String> = r
            .partitions
            .iter()
            .flat_map(|p| p.uncovered.iter().take(3).map(move |(lo, hi)| format!("{}:{}-{}", p.partition, lo, hi)))
            .collect();
        println!(
            "{:<44} {:>10.1} {:>12} {:>10}  {}",
            topic,
            r.bytes as f64 / 1_048_576.0,
            r.docs(),
            r.uncovered_docs(),
            ranges.join(" ")
        );
        docs += r.docs();
        uncovered += r.uncovered_docs();
        bytes += r.bytes;
    }
    println!("TOTAL {:.1} MB, {} records, {} uncovered", bytes as f64 / 1_048_576.0, docs, uncovered);
}

fn make_legacy(args: &[String]) {
    use tantivy::schema::{DateOptions, NumericOptions, Schema, STORED, STRING, TEXT};
    let [dir, topic, partition, lo, hi] = args else {
        panic!("make-legacy <index_dir> <topic> <partition> <lo> <hi>");
    };
    let (partition, lo, hi): (i64, i64, i64) = (partition.parse().unwrap(), lo.parse().unwrap(), hi.parse().unwrap());
    std::fs::create_dir_all(dir).unwrap();
    let mut b = Schema::builder();
    let id = b.add_text_field("_id", STRING | STORED);
    let t = b.add_text_field("_topic", STRING | STORED | tantivy::schema::FAST);
    let fast = || NumericOptions::default().set_indexed().set_stored().set_fast();
    let p = b.add_i64_field("_partition", fast());
    let o = b.add_i64_field("_offset", fast());
    let ts = b.add_date_field("_timestamp", DateOptions::default().set_indexed().set_stored().set_fast());
    let json = b.add_json_field("_json_content", TEXT | STORED);
    let key = b.add_text_field("_key", STRING | STORED);
    let value = b.add_text_field("_value", TEXT | STORED);
    let index = tantivy::Index::create_in_dir(dir, b.build()).unwrap();
    let mut w: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    for off in lo..=hi {
        let mut d = tantivy::TantivyDocument::default();
        d.add_text(id, format!("{topic}-{partition}-{off}"));
        d.add_text(t, topic);
        d.add_i64(p, partition);
        d.add_i64(o, off);
        d.add_date(ts, tantivy::DateTime::from_timestamp_millis(1_700_000_000_000 + off));
        let body = format!("{{\"id\":{off},\"note\":\"legacyonlycopy\"}}");
        let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&body).unwrap();
        d.add_object(json, obj.into_iter().map(|(k, v)| (k, v.into())).collect());
        d.add_text(key, format!("k{off}"));
        d.add_text(value, &body);
        w.add_document(d).unwrap();
    }
    w.commit().unwrap();
    println!("wrote {} records to {}", hi - lo + 1, dir);
}

async fn drop_archive(args: &[String]) {
    let [catalogue, topic, partition, offset] = args else {
        panic!("drop-archive <segment_index.json> <topic> <partition> <offset>");
    };
    let (partition, offset): (i32, i64) = (partition.parse().unwrap(), offset.parse().unwrap());
    let index = SegmentIndex::with_persistence(PathBuf::from(catalogue), false);
    index.load().await.expect("catalogue");
    let segs = index.get_segments_for_partition(topic, partition).await.unwrap();
    let Some(seg) = segs.iter().find(|s| s.min_offset <= offset && offset <= s.max_offset) else {
        eprintln!("no archive of {topic}-{partition} holds offset {offset}");
        std::process::exit(1);
    };
    index.remove_segment(topic, partition, &seg.segment_id).await.unwrap();
    index.save().await.unwrap();
    println!("{}", seg.object_store_path);
}
