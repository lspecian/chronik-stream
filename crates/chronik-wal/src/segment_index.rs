//! Seeking into WAL segments instead of scanning them.
//!
//! A fetch asks for "records from offset N". The offsets that answer that
//! question live at the *end* of each record on the wire — after the payload —
//! so finding them by reading forwards means walking the whole file. That is
//! what `read_from` used to do, on every fetch, for the entire active segment:
//! measured at 2.5–4.5ms to return two records, growing with the segment until
//! rotation at 250MB (RP-9).
//!
//! This keeps a sparse map of offset → byte position per segment file, built
//! incrementally as the file grows, so every byte is parsed once ever rather
//! than once per fetch. A read binary-searches the marks, opens the file at the
//! nearest one at or below the offset it wants, and reads forward only as far
//! as it needs.
//!
//! The index is a pure accelerator. It is rebuilt from the file whenever the
//! file might have changed underneath it — a truncation, or a shrink — and
//! being wrong costs a rescan, never a wrong answer.

use crate::error::{Result, WalError};
use crate::record::WalRecord;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::debug;

/// Bytes between marks. One mark per 64KiB of segment bounds a 250MB segment to
/// ~4,000 entries (~64KB), and bounds the forward scan after a seek to 64KB.
const MARK_INTERVAL: u64 = 64 * 1024;

/// How much to read at a time when serving from a segment.
const READ_CHUNK: usize = 4 * 1024 * 1024;

const WAL_MAGIC: u16 = 0xCA7E;
const WAL_VERSION_V2: u8 = 2;

/// Fixed header before the topic name: magic, version, flags, length, crc32.
const HEADER_LEN: usize = 12;
/// Fixed trailer after the payload: base_offset, last_offset, record_count.
const TRAILER_LEN: usize = 20;

/// One record's shape within a buffer, located without copying its payload.
pub(crate) struct RecordLayout {
    pub total_len: usize,
    pub topic: std::ops::Range<usize>,
    pub payload: std::ops::Range<usize>,
    pub partition: i32,
    pub base_offset: i64,
    pub last_offset: i64,
    pub record_count: i32,
    pub magic: u16,
    pub version: u8,
    pub flags: u8,
    pub length: u32,
    pub crc32: u32,
}

pub(crate) enum Parsed {
    /// A whole record, starting at byte 0 of the buffer.
    Record(RecordLayout),
    /// The buffer ends mid-record; read more and try again.
    Incomplete,
    /// Not a V2 WAL record. The rest of the file is unreadable, which is how a
    /// torn tail from a crash presents.
    Invalid,
}

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn le_i32(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn le_i64(b: &[u8], at: usize) -> i64 {
    i64::from_le_bytes([
        b[at],
        b[at + 1],
        b[at + 2],
        b[at + 3],
        b[at + 4],
        b[at + 5],
        b[at + 6],
        b[at + 7],
    ])
}

/// Locate one V2 record at the start of `buf`, without copying anything.
///
/// The wire layout is: magic, version, flags, length, crc32, topic_len, topic,
/// partition, canonical_data_len, canonical_data, base_offset, last_offset,
/// record_count. The offsets trail the payload, which is exactly why a reader
/// that wants to filter on them cannot avoid walking every record — and why the
/// index above exists.
pub(crate) fn parse_record(buf: &[u8]) -> Parsed {
    if buf.len() < HEADER_LEN + 2 {
        return Parsed::Incomplete;
    }

    let magic = le_u16(buf, 0);
    let version = buf[2];
    let flags = buf[3];
    let length = le_u32(buf, 4);
    let crc32 = le_u32(buf, 8);

    if magic != WAL_MAGIC || version != WAL_VERSION_V2 || length == 0 {
        return Parsed::Invalid;
    }

    let topic_len = le_u16(buf, HEADER_LEN) as usize;
    let topic_at = HEADER_LEN + 2;
    let partition_at = topic_at + topic_len;
    let cdl_at = partition_at + 4;
    let payload_at = cdl_at + 4;
    if buf.len() < payload_at {
        return Parsed::Incomplete;
    }

    let partition = le_i32(buf, partition_at);
    let payload_len = le_u32(buf, cdl_at) as usize;
    let meta_at = match payload_at.checked_add(payload_len) {
        Some(p) => p,
        None => return Parsed::Invalid,
    };
    let total_len = match meta_at.checked_add(TRAILER_LEN) {
        Some(t) => t,
        None => return Parsed::Invalid,
    };
    if buf.len() < total_len {
        return Parsed::Incomplete;
    }

    Parsed::Record(RecordLayout {
        total_len,
        topic: topic_at..partition_at,
        payload: payload_at..meta_at,
        partition,
        base_offset: le_i64(buf, meta_at),
        last_offset: le_i64(buf, meta_at + 8),
        record_count: le_i32(buf, meta_at + 16),
        magic,
        version,
        flags,
        length,
        crc32,
    })
}

impl RecordLayout {
    /// Materialise the record, copying the payload. Only called for records a
    /// read is actually going to return.
    fn to_record(&self, buf: &[u8]) -> Option<WalRecord> {
        let topic = std::str::from_utf8(&buf[self.topic.clone()]).ok()?.to_string();
        Some(WalRecord::V2 {
            magic: self.magic,
            version: self.version,
            flags: self.flags,
            length: self.length,
            crc32: self.crc32,
            topic,
            partition: self.partition,
            canonical_data: buf[self.payload.clone()].to_vec(),
            base_offset: self.base_offset,
            last_offset: self.last_offset,
            record_count: self.record_count,
        })
    }
}

/// What is known about one segment file.
struct SegmentMarks {
    path: PathBuf,
    /// Bytes already scanned. Everything below this is described by the fields
    /// here; everything above has yet to be read.
    indexed_len: u64,
    first_offset: Option<i64>,
    last_offset: Option<i64>,
    /// (base offset of a record, byte position of that record's first byte),
    /// ascending, roughly one per `MARK_INTERVAL`.
    marks: Vec<(i64, u64)>,
}

impl SegmentMarks {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            indexed_len: 0,
            first_offset: None,
            last_offset: None,
            marks: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.indexed_len = 0;
        self.first_offset = None;
        self.last_offset = None;
        self.marks.clear();
    }

    /// Read whatever has been appended since the last refresh and extend the
    /// marks over it. A file that shrank is rescanned from the start, which is
    /// what makes a truncation safe without any signal from the writer.
    async fn refresh(&mut self) -> Result<()> {
        let len = match tokio::fs::metadata(&self.path).await {
            Ok(m) => m.len(),
            // Rotation and reclamation both delete files; a segment that has
            // gone simply has nothing to contribute.
            Err(_) => {
                self.reset();
                return Ok(());
            }
        };

        if len < self.indexed_len {
            debug!(
                "WAL segment {:?} shrank from {} to {} — reindexing",
                self.path, self.indexed_len, len
            );
            self.reset();
        }
        if len == self.indexed_len {
            return Ok(());
        }

        let mut file = tokio::fs::File::open(&self.path).await?;
        file.seek(std::io::SeekFrom::Start(self.indexed_len)).await?;
        let mut tail = Vec::with_capacity((len - self.indexed_len) as usize);
        file.take(len - self.indexed_len).read_to_end(&mut tail).await?;

        let mut cursor = 0usize;
        loop {
            match parse_record(&tail[cursor..]) {
                Parsed::Record(layout) => {
                    let position = self.indexed_len + cursor as u64;
                    let want_mark = match self.marks.last() {
                        None => true,
                        Some((_, last_pos)) => position - last_pos >= MARK_INTERVAL,
                    };
                    if want_mark {
                        self.marks.push((layout.base_offset, position));
                    }
                    if self.first_offset.is_none() {
                        self.first_offset = Some(layout.base_offset);
                    }
                    self.last_offset = Some(layout.last_offset);
                    cursor += layout.total_len;
                }
                // Stop at the first incomplete or torn record and leave
                // `indexed_len` below it, so the next refresh retries from
                // there once the writer has finished.
                Parsed::Incomplete | Parsed::Invalid => break,
            }
        }

        self.indexed_len += cursor as u64;
        Ok(())
    }

    /// Byte position to start reading from to reach `offset`, or `None` if this
    /// segment cannot contain it.
    fn seek_to(&self, offset: i64) -> Option<u64> {
        if self.last_offset? < offset {
            return None;
        }
        // The last mark at or below the offset. Records between it and the
        // offset are skipped by the reader, which costs at most MARK_INTERVAL.
        match self.marks.binary_search_by(|(base, _)| base.cmp(&offset)) {
            Ok(i) => Some(self.marks[i].1),
            Err(0) => self.marks.first().map(|(_, p)| *p),
            Err(i) => Some(self.marks[i - 1].1),
        }
    }
}

/// The sparse index for one partition's segments.
pub struct PartitionIndex {
    dir: PathBuf,
    partition: i32,
    /// Keyed by segment id from the filename, so iteration is in log order.
    segments: BTreeMap<u64, SegmentMarks>,
}

impl PartitionIndex {
    pub fn new(dir: PathBuf, partition: i32) -> Self {
        Self {
            dir,
            partition,
            segments: BTreeMap::new(),
        }
    }

    fn segment_id(path: &Path, partition: i32) -> Option<u64> {
        let name = path.file_name()?.to_str()?;
        name.strip_prefix(&format!("wal_{}_", partition))?
            .strip_suffix(".log")?
            .parse()
            .ok()
    }

    /// Bring the index up to date with what is on disk.
    async fn refresh(&mut self) -> Result<()> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(e) => e,
            Err(_) => {
                return Err(WalError::SegmentNotFound(format!(
                    "{:?}/{}",
                    self.dir, self.partition
                )))
            }
        };

        let mut seen = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(id) = Self::segment_id(&path, self.partition) {
                seen.push(id);
                self.segments
                    .entry(id)
                    .or_insert_with(|| SegmentMarks::new(path));
            }
        }
        self.segments.retain(|id, _| seen.contains(id));

        for marks in self.segments.values_mut() {
            marks.refresh().await?;
        }
        Ok(())
    }

    /// Records from `offset` onward, reading only the parts of the segments
    /// that can contain them.
    ///
    /// `max_records` counts Kafka messages, not WAL batches, matching the
    /// contract `read_from` has always had.
    pub async fn read_from(&mut self, offset: i64, max_records: usize) -> Result<Vec<WalRecord>> {
        self.refresh().await?;

        let mut out = Vec::new();
        let mut messages = 0usize;

        // BTreeMap iterates by segment id, which is log order.
        let plan: Vec<(PathBuf, u64)> = self
            .segments
            .values()
            .filter_map(|marks| marks.seek_to(offset).map(|pos| (marks.path.clone(), pos)))
            .collect();

        for (path, start) in plan {
            if messages >= max_records {
                break;
            }
            self.read_segment_from(&path, start, offset, max_records, &mut messages, &mut out)
                .await?;
        }

        Ok(out)
    }

    async fn read_segment_from(
        &self,
        path: &Path,
        start: u64,
        offset: i64,
        max_records: usize,
        messages: &mut usize,
        out: &mut Vec<WalRecord>,
    ) -> Result<()> {
        let mut file = match tokio::fs::File::open(path).await {
            Ok(f) => f,
            Err(_) => return Ok(()), // rotated away between refresh and read
        };
        file.seek(std::io::SeekFrom::Start(start)).await?;

        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK];

        loop {
            let n = file.read(&mut chunk).await?;
            if n > 0 {
                buf.extend_from_slice(&chunk[..n]);
            }

            let mut cursor = 0usize;
            let mut stop = false;
            loop {
                match parse_record(&buf[cursor..]) {
                    Parsed::Record(layout) => {
                        if layout.last_offset >= offset && *messages < max_records {
                            if let Some(record) = layout.to_record(&buf[cursor..]) {
                                *messages += layout.record_count.max(1) as usize;
                                out.push(record);
                            }
                        }
                        cursor += layout.total_len;
                        if *messages >= max_records {
                            stop = true;
                            break;
                        }
                    }
                    Parsed::Incomplete => break,
                    Parsed::Invalid => {
                        stop = true;
                        break;
                    }
                }
            }
            buf.drain(..cursor);

            if stop || n == 0 {
                break;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_bytes(topic: &str, partition: i32, base: i64, last: i64, payload: usize) -> Vec<u8> {
        WalRecord::new_v2(
            topic.to_string(),
            partition,
            vec![7u8; payload],
            base,
            last,
            (last - base + 1) as i32,
        )
        .to_bytes()
        .unwrap()
    }

    #[test]
    fn a_record_is_located_without_copying_its_payload() {
        let bytes = record_bytes("t", 3, 10, 19, 128);
        match parse_record(&bytes) {
            Parsed::Record(l) => {
                assert_eq!(l.total_len, bytes.len());
                assert_eq!(l.base_offset, 10);
                assert_eq!(l.last_offset, 19);
                assert_eq!(l.record_count, 10);
                assert_eq!(l.partition, 3);
                assert_eq!(&bytes[l.topic.clone()], b"t");
                assert_eq!(l.payload.len(), 128);
            }
            _ => panic!("expected a record"),
        }
    }

    #[test]
    fn a_short_buffer_is_incomplete_not_invalid() {
        let bytes = record_bytes("t", 0, 0, 0, 64);
        for cut in [1, HEADER_LEN, HEADER_LEN + 2, bytes.len() - 1] {
            assert!(
                matches!(parse_record(&bytes[..cut]), Parsed::Incomplete),
                "a record cut at {cut} must be retried, not discarded"
            );
        }
    }

    #[test]
    fn garbage_is_invalid() {
        assert!(matches!(parse_record(&[0xff; 64]), Parsed::Invalid));
    }

    async fn write_segment(dir: &Path, partition: i32, id: u64, batches: &[(i64, i64)]) {
        let path = dir.join(format!("wal_{}_{}.log", partition, id));
        let mut bytes = Vec::new();
        for (base, last) in batches {
            bytes.extend_from_slice(&record_bytes("t", partition, *base, *last, 256));
        }
        tokio::fs::write(path, bytes).await.unwrap();
    }

    #[tokio::test]
    async fn reads_start_at_the_offset_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9), (10, 19), (20, 29)]).await;

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let got = index.read_from(10, 1000).await.unwrap();
        assert_eq!(
            got.iter().map(|r| r.get_base_offset()).collect::<Vec<_>>(),
            vec![10, 20],
            "the batch ending at 9 is entirely below the requested offset"
        );
    }

    #[tokio::test]
    async fn segments_that_cannot_contain_the_offset_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9)]).await;
        write_segment(dir.path(), 0, 1, &[(10, 19)]).await;
        write_segment(dir.path(), 0, 2, &[(20, 29)]).await;

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let got = index.read_from(20, 1000).await.unwrap();
        assert_eq!(
            got.iter().map(|r| r.get_base_offset()).collect::<Vec<_>>(),
            vec![20]
        );
    }

    #[tokio::test]
    async fn max_records_counts_messages_not_batches() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9), (10, 19), (20, 29)]).await;

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let got = index.read_from(0, 15).await.unwrap();
        assert_eq!(
            got.len(),
            2,
            "the limit is in messages, so it stops after the batch that crosses it"
        );
    }

    /// The point of the index: bytes are parsed once as the file grows, not
    /// once per read.
    #[tokio::test]
    async fn appended_bytes_are_indexed_once_and_then_only_extended() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9)]).await;

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        index.read_from(0, 1000).await.unwrap();
        let after_first = index.segments.get(&0).unwrap().indexed_len;
        assert!(after_first > 0);

        // Grow the file and read again.
        let path = dir.path().join("wal_0_0.log");
        let mut bytes = tokio::fs::read(&path).await.unwrap();
        bytes.extend_from_slice(&record_bytes("t", 0, 10, 19, 256));
        tokio::fs::write(&path, &bytes).await.unwrap();

        let got = index.read_from(10, 1000).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            index.segments.get(&0).unwrap().indexed_len,
            bytes.len() as u64,
            "the index covers the whole file after extending over the new bytes"
        );
    }

    /// A truncation rewrites the file shorter. The index must notice and
    /// rebuild rather than answer from marks pointing past the new end.
    #[tokio::test]
    async fn a_shrunken_file_is_reindexed() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9), (10, 19), (20, 29)]).await;

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        assert_eq!(index.read_from(0, 1000).await.unwrap().len(), 3);

        // Cut back to the first batch, as a suffix truncation would.
        let one = record_bytes("t", 0, 0, 9, 256);
        tokio::fs::write(dir.path().join("wal_0_0.log"), &one).await.unwrap();

        let got = index.read_from(0, 1000).await.unwrap();
        assert_eq!(
            got.iter().map(|r| r.get_last_offset()).collect::<Vec<_>>(),
            vec![9],
            "the cut records must not be served from a stale index"
        );
    }

    /// A crash can leave a half-written record at the end. It must be ignored
    /// without discarding the whole segment.
    #[tokio::test]
    async fn a_torn_tail_does_not_lose_the_records_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal_0_0.log");
        let mut bytes = record_bytes("t", 0, 0, 9, 256);
        let partial = record_bytes("t", 0, 10, 19, 256);
        bytes.extend_from_slice(&partial[..partial.len() / 2]);
        tokio::fs::write(&path, &bytes).await.unwrap();

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let got = index.read_from(0, 1000).await.unwrap();
        assert_eq!(got.len(), 1, "the complete record before the tear survives");
    }

    #[tokio::test]
    async fn a_missing_partition_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = PartitionIndex::new(dir.path().join("nope"), 0);
        assert!(index.read_from(0, 10).await.is_err());
    }
}
