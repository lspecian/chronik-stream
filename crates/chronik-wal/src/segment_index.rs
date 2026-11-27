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
use std::collections::{BTreeMap, HashSet};
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
    /// Highest `last_offset` of any record in the file — the maximum, not the
    /// last record's, because batches are not always stored in offset order.
    max_last_offset: Option<i64>,
    /// Lowest `base_offset` of any record in the file, so a search for batches
    /// below some offset can skip files that cannot hold one.
    min_base_offset: Option<i64>,
    /// (highest `last_offset` of every record BEFORE this position, byte
    /// position of a record's first byte), roughly one per `MARK_INTERVAL`.
    ///
    /// The running maximum, not the record's own base offset. Concurrent
    /// produce requests appended batches out of offset order until v2.16.1 —
    /// measured on a live partition as 857821, 857768, 858245, 858086, … in
    /// file order — and that data is still on disk. Keyed by base offset, a
    /// seek could start past a batch that holds the wanted offset; keyed by
    /// the running maximum, everything before the chosen mark provably ends
    /// below the offset. Non-decreasing, so it still binary-searches.
    marks: Vec<(i64, u64)>,
}

impl SegmentMarks {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            indexed_len: 0,
            max_last_offset: None,
            min_base_offset: None,
            marks: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.indexed_len = 0;
        self.max_last_offset = None;
        self.min_base_offset = None;
        self.marks.clear();
    }

    /// Read whatever has been appended since the last refresh and extend the
    /// marks over it. A file that shrank is rescanned from the start, which is
    /// what makes a truncation safe without any signal from the writer.
    ///
    /// `len` is the file's current length, `None` if it no longer exists.
    async fn refresh(&mut self, len: Option<u64>) -> Result<()> {
        let len = match len {
            Some(len) => len,
            // Rotation and reclamation both delete files; a segment that has
            // gone simply has nothing to contribute.
            None => {
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
                        self.marks.push((self.max_last_offset.unwrap_or(i64::MIN), position));
                    }
                    self.max_last_offset = Some(
                        self.max_last_offset.map_or(layout.last_offset, |m| m.max(layout.last_offset)),
                    );
                    self.min_base_offset = Some(
                        self.min_base_offset.map_or(layout.base_offset, |m| m.min(layout.base_offset)),
                    );
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
        if self.max_last_offset? < offset {
            return None;
        }
        // The last mark before which every record ends below the offset.
        let after = self.marks.partition_point(|(max_before, _)| *max_before < offset);
        match after {
            0 => self.marks.first().map(|(_, p)| *p),
            i => Some(self.marks[i - 1].1),
        }
    }
}

/// Every segment file of `partition` in `dir`: (segment id, path, current
/// length, or `None` if it vanished between listing and stat). Blocking.
fn list_segments(dir: &Path, partition: i32) -> std::io::Result<Vec<(u64, PathBuf, Option<u64>)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if let Some(id) = PartitionIndex::segment_id(&path, partition) {
            let len = std::fs::metadata(&path).ok().map(|m| m.len());
            found.push((id, path, len));
        }
    }
    Ok(found)
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
    ///
    /// The listing and every segment's length are taken in one blocking task.
    /// Asking tokio for each length separately is a thread-pool round trip
    /// per file on every read: a signal-stream partition of 14,179 segments
    /// spent ~170ms per fetch here before reading a single record.
    async fn refresh(&mut self) -> Result<()> {
        let dir = self.dir.clone();
        let partition = self.partition;
        let listing = tokio::task::spawn_blocking(move || list_segments(&dir, partition))
            .await
            .map_err(|e| WalError::Io(std::io::Error::other(e)))?
            .map_err(|_| {
                WalError::SegmentNotFound(format!("{:?}/{}", self.dir, self.partition))
            })?;

        let seen: HashSet<u64> = listing.iter().map(|(id, _, _)| *id).collect();
        self.segments.retain(|id, _| seen.contains(id));

        for (id, path, len) in listing {
            self.segments
                .entry(id)
                .or_insert_with(|| SegmentMarks::new(path))
                .refresh(len)
                .await?;
        }
        Ok(())
    }

    /// Records from `offset` onward, reading only the parts of the segments
    /// that can contain them.
    ///
    /// `max_records` counts Kafka messages, not WAL batches, matching the
    /// contract `read_from` has always had.
    pub async fn read_from(&mut self, offset: i64, max_records: usize) -> Result<Vec<WalRecord>> {
        self.read_from_bounded(offset, max_records, usize::MAX).await
    }

    /// [`read_from`](Self::read_from), stopping once `max_bytes` of records
    /// have been gathered — Kafka's fetch contract: the first batch is always
    /// returned whatever its size, so an oversized batch cannot stall a
    /// reader, and nothing after it that would cross the budget.
    ///
    /// A message count alone is no bound on work. A replica fetch asks for
    /// 10MB, which the fetch path turned into a million messages; on a
    /// partition of ~4.5KB trace records that is 967MB per read, read,
    /// decoded and concatenated before the response was cut. On a busy leader
    /// that ran past the 30-second fetch deadline every time, so a follower
    /// rebuilding from offset 0 never got past its first response — measured
    /// on signal-stream, v2.16.8.
    pub async fn read_from_bounded(
        &mut self,
        offset: i64,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<Vec<WalRecord>> {
        self.refresh().await?;

        // Batches are not always stored in offset order (see `SegmentMarks::
        // marks`). Returning them in FILE order let a reader serve 857768..
        // 857873 and then 858245, so the consumer's next fetch began at 858268
        // and never saw the batches stored after it: measured on a live
        // cluster as ~1.2% of every partition — 10,583 of 842,237 records on
        // one — on disk, replicated, counted by SQL, unreachable through Fetch.
        //
        // So: collect a little past the limit, order by offset, and return
        // only a contiguous run. A batch stored further away than the slack is
        // found by the next fetch, which asks for exactly the offset it starts
        // at — nothing is skipped, at worst a fetch returns less.
        let window = Budget {
            messages: max_records
                .saturating_mul(2)
                .max(max_records.saturating_add(READ_SLACK_MESSAGES)),
            bytes: max_bytes
                .saturating_mul(2)
                .max(max_bytes.saturating_add(READ_SLACK_BYTES)),
        };
        let mut out = Vec::new();
        let truncated = self.collect(offset, window, |_| true, &mut out).await?;
        out.sort_by_key(|r| r.get_base_offset());

        // The run must start with the batch holding `offset` if one exists.
        // When the window filled before reaching it, look for anything below
        // what was found — once, unbounded, but keeping only those records.
        let first_base = out.first().map(|r| r.get_base_offset()).unwrap_or(i64::MAX);
        if truncated && first_base > offset {
            let mut earlier = Vec::new();
            // Bounded by segment: only files holding a batch that starts below
            // `first_base` are read. Unbounded, this scanned the whole rest of
            // the log on every fetch that landed in a gap — 4.2s per fetch on a
            // 1.4GB partition once reclaimed WAL left gaps the segment tier fills.
            self.collect_below(offset, first_base, &mut earlier).await?;
            out.extend(earlier);
            out.sort_by_key(|r| r.get_base_offset());
        }

        Ok(contiguous_run(out, max_records, max_bytes))
    }

    /// Records with `last_offset >= offset` and `base_offset < below`, reading
    /// only segments that hold such a batch.
    async fn collect_below(&self, offset: i64, below: i64, out: &mut Vec<WalRecord>) -> Result<()> {
        let mut spent = Spent::default();
        let plan: Vec<(PathBuf, u64)> = self
            .segments
            .values()
            .filter(|marks| marks.min_base_offset.is_some_and(|m| m < below))
            .filter_map(|marks| marks.seek_to(offset).map(|pos| (marks.path.clone(), pos)))
            .collect();
        for (path, start) in plan {
            self.read_segment_from(&path, start, offset, &Budget::UNBOUNDED, |l| l.base_offset < below, &mut spent, out)
                .await?;
        }
        Ok(())
    }

    /// Records with `last_offset >= offset` that `accept` keeps, from every
    /// segment that can hold them, until `limit` is spent. Returns whether the
    /// limit cut the scan short.
    async fn collect(
        &self,
        offset: i64,
        limit: Budget,
        accept: impl Fn(&RecordLayout) -> bool + Copy,
        out: &mut Vec<WalRecord>,
    ) -> Result<bool> {
        let mut spent = Spent::default();
        // BTreeMap iterates by segment id, which is log order.
        let plan: Vec<(PathBuf, u64)> = self
            .segments
            .values()
            .filter_map(|marks| marks.seek_to(offset).map(|pos| (marks.path.clone(), pos)))
            .collect();

        for (path, start) in plan {
            if spent.reached(&limit) {
                return Ok(true);
            }
            self.read_segment_from(&path, start, offset, &limit, accept, &mut spent, out)
                .await?;
        }
        Ok(spent.reached(&limit))
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_segment_from(
        &self,
        path: &Path,
        start: u64,
        offset: i64,
        limit: &Budget,
        accept: impl Fn(&RecordLayout) -> bool,
        spent: &mut Spent,
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
                        if layout.last_offset >= offset && !spent.reached(limit) && accept(&layout) {
                            if let Some(record) = layout.to_record(&buf[cursor..]) {
                                spent.messages += layout.record_count.max(1) as usize;
                                spent.bytes += layout.total_len;
                                out.push(record);
                            }
                        }
                        cursor += layout.total_len;
                        if spent.reached(limit) {
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

/// Messages read past `max_records` so that batches stored slightly out of
/// order can still be returned in order.
const READ_SLACK_MESSAGES: usize = 1000;

/// The same slack, for the byte budget.
const READ_SLACK_BYTES: usize = 1024 * 1024;

/// How much a scan may gather before it stops.
#[derive(Clone, Copy)]
struct Budget {
    messages: usize,
    bytes: usize,
}

impl Budget {
    const UNBOUNDED: Budget = Budget {
        messages: usize::MAX,
        bytes: usize::MAX,
    };
}

/// What a scan has gathered so far.
#[derive(Default)]
struct Spent {
    messages: usize,
    bytes: usize,
}

impl Spent {
    fn reached(&self, limit: &Budget) -> bool {
        self.messages >= limit.messages || self.bytes >= limit.bytes
    }
}

/// From batches sorted by base offset, the longest run without a gap, up to
/// `max_records` messages and `max_bytes` bytes — the first batch always, so a
/// batch larger than the budget is still delivered.
///
/// A batch wholly covered by what is already in the run is a second copy of
/// those offsets (they exist on disk from before v2.16.1) and is dropped; the
/// first copy in log order is kept. A gap ends the run — the next fetch asks
/// for the gap's first offset, and finds the batch if it exists. The run may
/// begin above the requested offset: that is a genuine hole, and consumers
/// step over it as Kafka's do.
fn contiguous_run(sorted: Vec<WalRecord>, max_records: usize, max_bytes: usize) -> Vec<WalRecord> {
    let mut run = Vec::new();
    let mut next: Option<i64> = None;
    let mut messages = 0usize;
    let mut bytes = 0usize;
    for record in sorted {
        let (base, last) = (record.get_base_offset(), record.get_last_offset());
        if let Some(n) = next {
            if last < n {
                continue;
            }
            if base > n {
                break;
            }
        }
        let size = record.heap_size();
        if !run.is_empty() && bytes.saturating_add(size) > max_bytes {
            break;
        }
        bytes += size;
        next = Some(last + 1);
        messages += record.get_record_count().max(1) as usize;
        run.push(record);
        if messages >= max_records {
            break;
        }
    }
    run
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

    fn bases(got: &[WalRecord]) -> Vec<i64> {
        got.iter().map(|r| r.get_base_offset()).collect()
    }

    /// Batches stored out of offset order — as concurrent produce requests
    /// left them until v2.16.1 — come back in offset order.
    #[tokio::test]
    async fn batches_stored_out_of_order_are_returned_in_order() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(10, 19), (0, 9), (30, 39), (20, 29)]).await;
        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        assert_eq!(bases(&index.read_from(0, 1000).await.unwrap()), vec![0, 10, 20, 30]);
    }

    /// The live-cluster failure: with a limit, file order served 0..9 and then
    /// 20..29, the consumer's next fetch began at 30, and 10..19 was never read.
    #[tokio::test]
    async fn a_batch_stored_late_is_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9), (20, 29), (10, 19)]).await;
        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let got = index.read_from(0, 15).await.unwrap();
        assert_eq!(bases(&got), vec![0, 10], "the run must be contiguous");
    }

    #[tokio::test]
    async fn a_second_copy_of_the_same_offsets_is_served_once() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9), (10, 10), (10, 10), (11, 11)]).await;
        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        assert_eq!(bases(&index.read_from(0, 1000).await.unwrap()), vec![0, 10, 11]);
    }

    /// A genuine hole is stepped over, as Kafka consumers expect; it must not
    /// stall the reader.
    #[tokio::test]
    async fn a_genuine_hole_is_stepped_over() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9), (20, 29)]).await;
        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        assert_eq!(bases(&index.read_from(10, 1000).await.unwrap()), vec![20]);
    }

    /// A seek must not start past a misplaced batch. Marks keyed by base
    /// offset sent a read for 500 to a mark near 990's position, beyond where
    /// batch 500 was actually stored.
    #[tokio::test]
    async fn a_seek_does_not_jump_past_a_misplaced_batch() {
        let dir = tempfile::tempdir().unwrap();
        // 1,000 single-record batches (~300 bytes each, several marks), with
        // batch 500 stored near the end.
        let mut order: Vec<(i64, i64)> = (0..1000).filter(|o| *o != 500).map(|o| (o, o)).collect();
        order.insert(990, (500, 500));
        write_segment(dir.path(), 0, 0, &order).await;
        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let got = index.read_from(500, 5).await.unwrap();
        assert_eq!(bases(&got), vec![500, 501, 502, 503, 504]);
    }

    /// Several reads in a row see every offset exactly once — what a consumer
    /// does — even across scattered disorder.
    #[tokio::test]
    async fn successive_reads_cover_every_offset_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut order: Vec<(i64, i64)> = (0..200).map(|b| (b * 5, b * 5 + 4)).collect();
        for i in (0..order.len() - 3).step_by(7) {
            order.swap(i, i + 3);
        }
        write_segment(dir.path(), 0, 0, &order).await;
        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let mut next = 0i64;
        let mut seen = Vec::new();
        while next < 1000 {
            let got = index.read_from(next, 12).await.unwrap();
            assert!(!got.is_empty(), "stalled at {next}");
            for r in &got {
                seen.push(r.get_base_offset());
                next = r.get_last_offset() + 1;
            }
        }
        assert_eq!(seen, (0..200).map(|b| b * 5).collect::<Vec<_>>());
    }

    /// What `contiguous_run` charges for one batch written by `write_segment`.
    fn batch_cost() -> usize {
        WalRecord::from_bytes(&record_bytes("t", 0, 0, 0, 256))
            .unwrap()
            .heap_size()
    }

    #[tokio::test]
    async fn a_byte_budget_stops_the_read_however_many_messages_are_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let batches: Vec<(i64, i64)> = (0..100).map(|b| (b * 10, b * 10 + 9)).collect();
        write_segment(dir.path(), 0, 0, &batches).await;

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let got = index
            .read_from_bounded(0, usize::MAX, batch_cost() * 3)
            .await
            .unwrap();
        assert_eq!(
            got.iter().map(|r| r.get_base_offset()).collect::<Vec<_>>(),
            vec![0, 10, 20],
            "three batches fit the budget; a message limit that allows everything must not override it"
        );
    }

    #[tokio::test]
    async fn a_batch_larger_than_the_budget_is_still_returned() {
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), 0, 0, &[(0, 9), (10, 19)]).await;

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let got = index.read_from_bounded(0, usize::MAX, 1).await.unwrap();
        assert_eq!(
            got.iter().map(|r| r.get_base_offset()).collect::<Vec<_>>(),
            vec![0],
            "the first batch always, or a reader with a small budget never moves"
        );
    }

    #[tokio::test]
    async fn successive_bounded_reads_cover_every_offset_once_across_many_segments() {
        let dir = tempfile::tempdir().unwrap();
        for id in 0..50u64 {
            let first = id as i64 * 20;
            let mut batches = vec![(first, first + 4), (first + 5, first + 9), (first + 10, first + 14), (first + 15, first + 19)];
            batches.swap(1, 2); // stored out of order, as pre-v2.16.1 logs are
            write_segment(dir.path(), 0, id, &batches).await;
        }

        let mut index = PartitionIndex::new(dir.path().to_path_buf(), 0);
        let mut next = 0i64;
        let mut seen = Vec::new();
        while next < 1000 {
            let got = index
                .read_from_bounded(next, usize::MAX, batch_cost() * 7)
                .await
                .unwrap();
            assert!(!got.is_empty(), "stalled at {next}");
            assert!(got.len() <= 7, "read {} batches against a budget of 7", got.len());
            for r in &got {
                seen.push(r.get_base_offset());
                next = r.get_last_offset() + 1;
            }
        }
        assert_eq!(seen, (0..200).map(|b| b * 5).collect::<Vec<_>>());
    }
}
