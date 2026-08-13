//! Suffix truncation — discarding the *tail* of a partition's WAL.
//!
//! Every other reclamation path in this crate removes records from the front:
//! `delete_records_before` drops whole segments below a low watermark, and
//! rotation retires segments once they have been archived. Truncation is the
//! opposite operation, and until RP-3.3 nothing could perform it.
//!
//! A follower needs it exactly once: when it learns that the records it holds
//! above some offset were never committed by the leader that is now in charge.
//! Those records are not "old", they are *wrong*, and they have to go before
//! the follower appends anything else.
//!
//! # Why this module only plans
//!
//! Deciding what to cut is pure arithmetic over bytes and is where every
//! interesting mistake lives — an off-by-one here silently keeps divergent
//! records or throws away good ones. Doing the deciding in a function that
//! takes a `&[u8]` and returns a verdict means the whole decision surface is
//! reachable from unit tests, with no files, no locks, and no writer. The
//! `unlink`/`set_len` half lives in [`crate::group_commit`], which owns the
//! writer and can stop it first.
//!
//! # The rule
//!
//! **No record with any offset at or above the target may survive.**
//!
//! WAL records are batches, so the target does not always fall on a record
//! boundary. When it lands *inside* a batch, that whole batch is discarded —
//! including the offsets below the target that share it. Cutting the other way
//! (keeping the straddling batch) would retain records at and above the target,
//! which is the one outcome truncation exists to prevent. Erring low costs a
//! re-fetch; erring high keeps a divergent log and calls it converged.
//!
//! So the caller must treat the returned offset, not its requested target, as
//! where the log now ends.

/// A single WAL record's extent, read from its framing without decoding the
/// batch body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecordSpan {
    /// Bytes this record occupies, header included.
    total_size: usize,
    /// Lowest offset the record contains.
    base_offset: i64,
    /// Highest offset the record contains.
    last_offset: i64,
}

/// Bytes of a segment's fixed record header — enough to learn how long the
/// first record is without reading the file.
pub const RECORD_HEADER_LEN: usize = 12;

/// How many bytes the record at the head of `header` occupies, read from its
/// length field alone.
///
/// This is what lets a caller read a segment's first record without reading the
/// segment: stat the header, then read exactly this much. Returns `None` if
/// `header` is short or does not begin a V2 record.
pub fn declared_record_size(header: &[u8]) -> Option<usize> {
    if header.len() < RECORD_HEADER_LEN {
        return None;
    }
    if le_u16(header, 0)? != MAGIC || header[2] != VERSION_V2 {
        return None;
    }
    let length = le_u32(header, 4)? as usize;
    if length == 0 {
        return None;
    }
    RECORD_HEADER_LEN.checked_add(length)
}

/// The offset range `[base, last]` of the record at the head of `data`.
///
/// Used to place a segment relative to a truncation target without scanning it:
/// segments whose first record already sits at or above the target are deleted
/// whole, and only the one segment that straddles the target is scanned.
pub fn first_record_range(data: &[u8]) -> Option<(i64, i64)> {
    parse_record_span(data).map(|s| (s.base_offset, s.last_offset))
}

/// What must happen to one segment file for a truncation to `target_offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentVerdict {
    /// Nothing parseable in the file — an empty or freshly-created segment.
    /// Distinct from `KeepWhole` because there is no surviving offset to report.
    Empty,
    /// Every record ends below the target. The file is untouched.
    KeepWhole { last_offset: i64 },
    /// The first record already reaches the target. The whole file goes.
    DeleteWhole,
    /// Keep the first `keep_bytes`; what survives ends at `last_offset`.
    Cut { keep_bytes: u64, last_offset: i64 },
}

/// Decide what a truncation to `target_offset` does to one segment file.
///
/// `target_offset` is exclusive: offsets strictly below it may survive.
///
/// Scanning stops at the first byte that does not parse as a WAL record, which
/// is the same place [`crate::WalManager::read_from`] stops. Records beyond a
/// corrupt point are already invisible to every reader, so this refuses to
/// judge them rather than deleting bytes it could not identify — truncation
/// removes records it has positively read as being at or above the target, and
/// nothing else.
pub fn plan_segment_truncation(data: &[u8], target_offset: i64) -> SegmentVerdict {
    let mut cursor = 0usize;
    let mut last_kept: Option<i64> = None;

    while cursor < data.len() {
        let span = match parse_record_span(&data[cursor..]) {
            Some(span) => span,
            // Unparseable: treat as end of readable data, keep what we read.
            None => break,
        };

        if span.last_offset >= target_offset {
            return match last_kept {
                None => SegmentVerdict::DeleteWhole,
                Some(last_offset) => SegmentVerdict::Cut {
                    keep_bytes: cursor as u64,
                    last_offset,
                },
            };
        }

        last_kept = Some(span.last_offset);
        cursor += span.total_size;
    }

    match last_kept {
        None => SegmentVerdict::Empty,
        Some(last_offset) => SegmentVerdict::KeepWhole { last_offset },
    }
}

/// Read one V2 record's framing from the head of `data`.
///
/// Returns `None` for anything that is not a complete, self-consistent V2
/// record — wrong magic, a version this cannot read, a length that overruns the
/// buffer, or interior fields that disagree with the declared length. V1
/// records are deliberately not handled: the partition WAL read path is
/// V2-only, so a V1 record here is already unreadable data.
///
/// V2 layout, little-endian throughout:
/// ```text
///   0  magic u16 = 0xCA7E     12          topic_len u16
///   2  version u8 = 2         14          topic[topic_len]
///   3  flags u8               14+tl       partition i32
///   4  length u32             18+tl       data_len u32
///   8  crc32 u32              22+tl       canonical_data[data_len]
///                             22+tl+dl    base_offset i64
///                             30+tl+dl    last_offset i64
///                             38+tl+dl    record_count i32
/// ```
/// `length` counts everything after the 12-byte header, so the record occupies
/// `12 + length` bytes and `12 + length == 42 + topic_len + data_len`. Checking
/// that identity is what makes a garbage `length` detectable instead of a
/// cursor that walks off into the next record.
fn parse_record_span(data: &[u8]) -> Option<RecordSpan> {
    let total_size = declared_record_size(data)?;
    if total_size > data.len() {
        return None;
    }

    let topic_len = le_u16(data, 12)? as usize;
    let data_len_pos = 18usize.checked_add(topic_len)?;
    let canonical_len = le_u32(data, data_len_pos)? as usize;

    // The trailer must land exactly where `length` says the record ends.
    let trailer = 22usize.checked_add(topic_len)?.checked_add(canonical_len)?;
    if trailer.checked_add(20)? != total_size {
        return None;
    }

    Some(RecordSpan {
        total_size,
        base_offset: le_i64(data, trailer)?,
        last_offset: le_i64(data, trailer + 8)?,
    })
}

const MAGIC: u16 = 0xCA7E;
const VERSION_V2: u8 = 2;

fn le_u16(d: &[u8], p: usize) -> Option<u16> {
    d.get(p..p + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn le_u32(d: &[u8], p: usize) -> Option<u32> {
    d.get(p..p + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le_i64(d: &[u8], p: usize) -> Option<i64> {
    d.get(p..p + 8).map(|b| {
        i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    })
}

/// What a truncation actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncateOutcome {
    /// Where the log now ends, exclusive — the offset a follower should fetch
    /// next. `None` when nothing survived and the partition's log is empty.
    ///
    /// This can be **below** the requested target when the target fell inside a
    /// batch; see the module docs. Callers must resume from this, not from what
    /// they asked for.
    pub new_log_end_offset: Option<i64>,
    /// Segment files removed outright.
    pub segments_deleted: usize,
    /// Bytes removed, whether by deleting files or shortening one.
    pub bytes_discarded: u64,
    /// Buffered writes dropped before they reached disk.
    pub buffered_writes_dropped: usize,
}

impl TruncateOutcome {
    /// Nothing to do: the log already ended at or below the target.
    pub(crate) fn untouched(new_log_end_offset: Option<i64>) -> Self {
        Self {
            new_log_end_offset,
            segments_deleted: 0,
            bytes_discarded: 0,
            buffered_writes_dropped: 0,
        }
    }

    /// Whether any bytes were removed. A truncation that changed nothing must
    /// not disturb the writer — and callers need to distinguish that from one
    /// that emptied the log, because the two report the same `None` log end and
    /// only one of them means "start again from 0".
    pub fn touched_disk(&self) -> bool {
        self.segments_deleted > 0 || self.bytes_discarded > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::WalRecord;

    /// A record covering `[base, last]`, sized by `payload` so tests can tell
    /// records apart by their byte extent.
    fn record(base: i64, last: i64, payload: usize) -> Vec<u8> {
        WalRecord::new_v2(
            "orders".to_string(),
            3,
            vec![0xAB; payload],
            base,
            last,
            (last - base + 1) as i32,
        )
        .to_bytes()
        .unwrap()
    }

    fn segment(records: &[Vec<u8>]) -> Vec<u8> {
        records.iter().flatten().copied().collect()
    }

    #[test]
    fn an_empty_file_has_nothing_to_cut() {
        assert_eq!(plan_segment_truncation(&[], 10), SegmentVerdict::Empty);
    }

    #[test]
    fn a_target_above_the_log_keeps_everything() {
        let data = segment(&[record(0, 4, 8), record(5, 9, 8)]);
        assert_eq!(
            plan_segment_truncation(&data, 100),
            SegmentVerdict::KeepWhole { last_offset: 9 }
        );
    }

    /// The boundary case: a target equal to the next offset is a no-op, because
    /// no existing record reaches it.
    #[test]
    fn a_target_at_the_log_end_keeps_everything() {
        let data = segment(&[record(0, 4, 8), record(5, 9, 8)]);
        assert_eq!(
            plan_segment_truncation(&data, 10),
            SegmentVerdict::KeepWhole { last_offset: 9 }
        );
    }

    /// ...and one offset lower must drop that last batch, since it contains the
    /// target itself.
    #[test]
    fn a_target_one_below_the_end_drops_the_last_record() {
        let first = record(0, 4, 8);
        let keep_bytes = first.len() as u64;
        let data = segment(&[first, record(5, 9, 8)]);

        assert_eq!(
            plan_segment_truncation(&data, 9),
            SegmentVerdict::Cut {
                keep_bytes,
                last_offset: 4
            }
        );
    }

    #[test]
    fn a_target_below_the_first_record_deletes_the_file() {
        let data = segment(&[record(50, 54, 8), record(55, 59, 8)]);
        assert_eq!(plan_segment_truncation(&data, 50), SegmentVerdict::DeleteWhole);
        assert_eq!(plan_segment_truncation(&data, 10), SegmentVerdict::DeleteWhole);
    }

    #[test]
    fn a_target_of_zero_deletes_everything() {
        let data = segment(&[record(0, 4, 8)]);
        assert_eq!(plan_segment_truncation(&data, 0), SegmentVerdict::DeleteWhole);
    }

    /// The rule the module exists to enforce: a target *inside* a batch takes
    /// the whole batch, so the surviving log ends below what was asked for.
    #[test]
    fn a_target_inside_a_batch_discards_that_whole_batch() {
        let first = record(0, 9, 8);
        let keep_bytes = first.len() as u64;
        let data = segment(&[first, record(10, 19, 8)]);

        // Ask to keep offsets < 15, which sits in the middle of [10, 19].
        assert_eq!(
            plan_segment_truncation(&data, 15),
            SegmentVerdict::Cut {
                keep_bytes,
                last_offset: 9
            }
        );
    }

    /// The property, stated directly: whatever the target, replaying the bytes
    /// that survive must never yield an offset at or above it.
    #[test]
    fn nothing_at_or_above_the_target_ever_survives() {
        let records = vec![
            record(0, 3, 4),
            record(4, 4, 16),
            record(5, 11, 1),
            record(12, 20, 32),
            record(21, 21, 8),
        ];
        let data = segment(&records);

        for target in 0..=25 {
            let kept = match plan_segment_truncation(&data, target) {
                SegmentVerdict::Empty | SegmentVerdict::DeleteWhole => &data[..0],
                SegmentVerdict::KeepWhole { .. } => &data[..],
                SegmentVerdict::Cut { keep_bytes, .. } => &data[..keep_bytes as usize],
            };

            // Walk what survived and check every record.
            let mut cursor = 0usize;
            while let Some(span) = parse_record_span(&kept[cursor..]) {
                assert!(
                    span.last_offset < target,
                    "target {target} left a record ending at {} alive",
                    span.last_offset
                );
                cursor += span.total_size;
            }
            assert_eq!(cursor, kept.len(), "survivors must end on a record boundary");
        }
    }

    /// Reported and actual must agree, or a follower resumes at the wrong place.
    #[test]
    fn the_reported_end_matches_the_surviving_bytes() {
        let data = segment(&[record(0, 3, 4), record(4, 9, 16), record(10, 14, 8)]);

        for target in 0..=20 {
            let (kept_len, reported) = match plan_segment_truncation(&data, target) {
                SegmentVerdict::Empty | SegmentVerdict::DeleteWhole => (0, None),
                SegmentVerdict::KeepWhole { last_offset } => (data.len(), Some(last_offset)),
                SegmentVerdict::Cut {
                    keep_bytes,
                    last_offset,
                } => (keep_bytes as usize, Some(last_offset)),
            };

            let mut cursor = 0usize;
            let mut actual_last = None;
            while let Some(span) = parse_record_span(&data[cursor..kept_len]) {
                actual_last = Some(span.last_offset);
                cursor += span.total_size;
            }
            assert_eq!(reported, actual_last, "target {target}");
        }
    }

    /// A partial record at the tail — a crash mid-write — is not a reason to
    /// discard the complete records before it.
    #[test]
    fn a_half_written_trailing_record_stops_the_scan() {
        let mut data = segment(&[record(0, 4, 8), record(5, 9, 8)]);
        let torn = record(10, 14, 8);
        data.extend_from_slice(&torn[..torn.len() / 2]);

        assert_eq!(
            plan_segment_truncation(&data, 100),
            SegmentVerdict::KeepWhole { last_offset: 9 }
        );
    }

    #[test]
    fn garbage_is_not_mistaken_for_a_record() {
        assert!(parse_record_span(&[0xFF; 64]).is_none());
        assert!(parse_record_span(&[]).is_none());
        assert!(parse_record_span(&[0x7E, 0xCA, 0x02]).is_none());
    }

    /// A length field that disagrees with the interior fields must be rejected,
    /// not trusted as a stride — trusting it walks the cursor into the middle
    /// of the next record, where anything can parse as anything.
    #[test]
    fn a_length_that_contradicts_the_body_is_rejected() {
        let good = record(0, 4, 8);

        let mut lying = good.clone();
        let inflated = (u32::from_le_bytes([lying[4], lying[5], lying[6], lying[7]]) + 8).to_le_bytes();
        lying[4..8].copy_from_slice(&inflated);
        assert!(parse_record_span(&lying).is_none());

        let mut shrunk = good.clone();
        let deflated = (u32::from_le_bytes([shrunk[4], shrunk[5], shrunk[6], shrunk[7]]) - 8).to_le_bytes();
        shrunk[4..8].copy_from_slice(&deflated);
        assert!(parse_record_span(&shrunk).is_none());
    }

    /// A V1 record is not readable by the partition WAL read path, so the
    /// scanner must stop at it rather than guess at its extent.
    #[test]
    fn a_v1_record_stops_the_scan() {
        let v1 = WalRecord::new(0, None, b"legacy".to_vec(), 0).to_bytes().unwrap();
        assert!(parse_record_span(&v1).is_none());
        assert_eq!(plan_segment_truncation(&v1, 100), SegmentVerdict::Empty);
    }

    /// The header alone must reveal the record's length, or a caller cannot
    /// read one record without reading the whole segment.
    #[test]
    fn a_records_length_is_readable_from_its_header_alone() {
        for payload in [0usize, 1, 300, 65_536] {
            let bytes = record(0, 0, payload);
            assert_eq!(
                declared_record_size(&bytes[..RECORD_HEADER_LEN]),
                Some(bytes.len())
            );
        }
        assert_eq!(declared_record_size(&[0xFF; RECORD_HEADER_LEN]), None);
        assert_eq!(declared_record_size(&[0u8; RECORD_HEADER_LEN - 1]), None);
    }

    #[test]
    fn the_first_records_range_places_a_segment_without_scanning_it() {
        let data = segment(&[record(400, 449, 32), record(450, 461, 8)]);
        assert_eq!(first_record_range(&data), Some((400, 449)));
        assert_eq!(first_record_range(&[]), None);
    }

    #[test]
    fn spans_measure_records_exactly() {
        for payload in [0usize, 1, 7, 64, 1024] {
            let bytes = record(11, 22, payload);
            let span = parse_record_span(&bytes).expect("record must parse");
            assert_eq!(span.total_size, bytes.len());
            assert_eq!(span.last_offset, 22);
        }
    }

    /// Records are found at their true positions even when sizes vary, which is
    /// what makes `keep_bytes` a valid file length rather than an estimate.
    #[test]
    fn a_cut_lands_on_a_real_record_boundary() {
        let sizes = [3usize, 100, 7, 4096, 1];
        let records: Vec<Vec<u8>> = sizes
            .iter()
            .enumerate()
            .map(|(i, &p)| record(i as i64 * 10, i as i64 * 10 + 9, p))
            .collect();
        let data = segment(&records);

        // Cutting before record i must keep exactly the first i records' bytes.
        let mut expected = 0u64;
        for (i, rec) in records.iter().enumerate() {
            let target = i as i64 * 10; // first offset of record i
            let verdict = plan_segment_truncation(&data, target);
            if i == 0 {
                assert_eq!(verdict, SegmentVerdict::DeleteWhole);
            } else {
                assert_eq!(
                    verdict,
                    SegmentVerdict::Cut {
                        keep_bytes: expected,
                        last_offset: (i as i64 - 1) * 10 + 9,
                    }
                );
            }
            expected += rec.len() as u64;
        }
        assert_eq!(expected as usize, data.len());
    }
}
