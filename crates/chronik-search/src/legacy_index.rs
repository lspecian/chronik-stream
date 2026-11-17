//! Retiring the indexes the removed RealtimeIndexer left behind.
//!
//! The RealtimeIndexer (removed in 08b1c58) wrote one Tantivy index per topic
//! into `{data_dir}/index/{topic}`. Nothing writes those directories any more;
//! they are frozen at the day the indexer stopped, and searches read them as
//! one more source. The WalIndexer's archives hold the same records, so an old
//! index can be deleted once every record in it is also in an archive.
//!
//! This module works out, per old index, which of its records the archives do
//! not cover. Whatever is covered is redundant; an index wholly covered (or
//! whose remaining records are below the partition's log start — data that
//! retention already deleted everywhere else) can go.

use chronik_common::{Error, Result};
use chronik_storage::segment_index::SegmentMetadata;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One partition of an old index, measured against the archives.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartitionCoverage {
    pub partition: i32,
    /// Documents the old index holds for the partition.
    pub docs: u64,
    pub min_offset: i64,
    pub max_offset: i64,
    /// Of those, documents no archive covers.
    pub uncovered_docs: u64,
    /// The uncovered offsets, as inclusive ranges.
    pub uncovered: Vec<(i64, i64)>,
}

/// An old index, measured against the archives.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyIndexReport {
    pub topic: String,
    pub dir: PathBuf,
    pub bytes: u64,
    pub partitions: Vec<PartitionCoverage>,
}

impl LegacyIndexReport {
    pub fn docs(&self) -> u64 {
        self.partitions.iter().map(|p| p.docs).sum()
    }
    pub fn uncovered_docs(&self) -> u64 {
        self.partitions.iter().map(|p| p.uncovered_docs).sum()
    }
    /// Uncovered documents at or above each partition's log start: records
    /// that exist nowhere else and that retention has not deleted.
    pub fn live_uncovered_docs(&self, log_start: impl Fn(i32) -> i64) -> u64 {
        self.partitions
            .iter()
            .map(|p| {
                let start = log_start(p.partition);
                p.uncovered
                    .iter()
                    .map(|&(lo, hi)| if hi < start { 0 } else { (hi - lo.max(start) + 1) as u64 })
                    .sum::<u64>()
                    .min(p.uncovered_docs)
            })
            .sum()
    }
}

/// The old index directories under `data_dir/index`, as (topic, path).
pub fn legacy_index_dirs(data_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(data_dir.join("index")) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("meta.json").is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                out.push((name.to_string(), path));
            }
        }
    }
    out.sort();
    out
}

/// Measure the old index at `dir` against `archives` (the topic's archived
/// segments). Fails if `dir` is not an index the RealtimeIndexer wrote.
pub fn analyze(topic: &str, dir: &Path, archives: &[SegmentMetadata]) -> Result<LegacyIndexReport> {
    let index = tantivy::Index::open_in_dir(dir)
        .map_err(|e| Error::Internal(format!("open {}: {e}", dir.display())))?;
    let schema = index.schema();
    for name in ["_partition", "_offset", "_timestamp"] {
        let fast = schema.get_field(name).map(|f| schema.get_field_entry(f).is_fast()).unwrap_or(false);
        if !fast {
            return Err(Error::Internal(format!(
                "{} is not a RealtimeIndexer index (no fast field {name})",
                dir.display()
            )));
        }
    }
    let reader = index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::Manual)
        .try_into()
        .map_err(|e: tantivy::TantivyError| Error::Internal(format!("reader {}: {e}", dir.display())))?;
    let searcher = reader.searcher();

    let mut offsets: BTreeMap<i32, Vec<i64>> = BTreeMap::new();
    for segment in searcher.segment_readers() {
        let ff = segment.fast_fields();
        let part = ff
            .i64("_partition")
            .map_err(|e| Error::Internal(format!("{e}")))?
            .first_or_default_col(0);
        let off = ff
            .i64("_offset")
            .map_err(|e| Error::Internal(format!("{e}")))?
            .first_or_default_col(-1);
        for doc in segment.doc_ids_alive() {
            let o = off.get_val(doc);
            if o >= 0 {
                offsets.entry(part.get_val(doc) as i32).or_default().push(o);
            }
        }
    }

    let mut partitions = Vec::new();
    for (partition, mut offs) in offsets {
        offs.sort_unstable();
        offs.dedup();
        let mut covered = covered_ranges(archives, partition);
        covered.sort_unstable();
        let mut uncovered: Vec<(i64, i64)> = Vec::new();
        let mut uncovered_docs = 0u64;
        let mut ci = 0;
        for &o in &offs {
            while ci < covered.len() && covered[ci].1 < o {
                ci += 1;
            }
            let is_covered = ci < covered.len() && covered[ci].0 <= o;
            if !is_covered {
                uncovered_docs += 1;
                match uncovered.last_mut() {
                    Some(last) if last.1 + 1 == o => last.1 = o,
                    _ => uncovered.push((o, o)),
                }
            }
        }
        partitions.push(PartitionCoverage {
            partition,
            docs: offs.len() as u64,
            min_offset: *offs.first().unwrap_or(&0),
            max_offset: *offs.last().unwrap_or(&0),
            uncovered_docs,
            uncovered,
        });
    }
    Ok(LegacyIndexReport {
        topic: topic.to_string(),
        dir: dir.to_path_buf(),
        bytes: dir_bytes(dir),
        partitions,
    })
}

/// The partition's archived offset ranges, merged.
fn covered_ranges(archives: &[SegmentMetadata], partition: i32) -> Vec<(i64, i64)> {
    let mut ranges: Vec<(i64, i64)> = archives
        .iter()
        .filter(|s| s.partition == partition)
        .map(|s| (s.min_offset, s.max_offset))
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|it| it.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::{DateOptions, NumericOptions, Schema, STORED, STRING, TEXT};

    fn legacy(dir: &Path, docs: &[(i64, i64)]) {
        let mut b = Schema::builder();
        b.add_text_field("_topic", STRING | STORED);
        let fast = || NumericOptions::default().set_indexed().set_stored().set_fast();
        let p = b.add_i64_field("_partition", fast());
        let o = b.add_i64_field("_offset", fast());
        let t = b.add_date_field("_timestamp", DateOptions::default().set_indexed().set_stored().set_fast());
        b.add_text_field("_value", TEXT | STORED);
        let index = tantivy::Index::create_in_dir(dir, b.build()).unwrap();
        let mut w: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        for &(part, off) in docs {
            let mut d = tantivy::TantivyDocument::default();
            d.add_i64(p, part);
            d.add_i64(o, off);
            d.add_date(t, tantivy::DateTime::from_timestamp_millis(1_700_000_000_000));
            w.add_document(d).unwrap();
        }
        w.commit().unwrap();
    }

    fn archive(partition: i32, lo: i64, hi: i64) -> SegmentMetadata {
        SegmentMetadata {
            segment_id: format!("t-{partition}-{lo}-{hi}"),
            topic: "t".into(),
            partition,
            min_offset: lo,
            max_offset: hi,
            record_count: (hi - lo + 1) as usize,
            min_timestamp: 0,
            max_timestamp: 0,
            object_store_path: String::new(),
            size_bytes: 0,
            created_at: 0,
            compression: String::new(),
        }
    }

    #[test]
    fn finds_exactly_the_offsets_no_archive_holds() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("index").join("t");
        std::fs::create_dir_all(&dir).unwrap();
        let mut docs: Vec<(i64, i64)> = (0..100).map(|o| (0, o)).collect();
        docs.extend((0..50).map(|o| (1, o)));
        legacy(&dir, &docs);

        // Partition 0: archives hold 0-39 and 60-99 (40-59 missing).
        // Partition 1: archive holds 10-49 (0-9 missing).
        let archives = [archive(0, 0, 19), archive(0, 20, 39), archive(0, 60, 99), archive(1, 10, 49)];
        let r = analyze("t", &dir, &archives).unwrap();
        assert_eq!(r.docs(), 150);
        assert_eq!(r.uncovered_docs(), 30);
        assert_eq!(r.partitions[0].uncovered, vec![(40, 59)]);
        assert_eq!(r.partitions[1].uncovered, vec![(0, 9)]);
        // Retention has deleted partition 1 below 5 and partition 0 below 50.
        assert_eq!(r.live_uncovered_docs(|p| if p == 0 { 50 } else { 5 }), 10 + 5);

        assert_eq!(legacy_index_dirs(root.path()), vec![("t".to_string(), dir.clone())]);
        // Fully covered once the gaps are archived.
        let full = [archive(0, 0, 99), archive(1, 0, 49)];
        assert_eq!(analyze("t", &dir, &full).unwrap().uncovered_docs(), 0);
    }
}
