//! A summary of the words an archived index segment holds, so a search can
//! skip segments that cannot match without opening them.
//!
//! A search across many topics opens every archived segment that could hold a
//! hit. Thunderbird's nodes hold 6,000–13,000 archives each, most of them a few
//! KB, and a word search across all topics touched every one — unpacked, opened
//! and queried — for a word that was in a handful of them. Each archive now has
//! a Bloom filter of its text and JSON terms beside it (`<archive>.terms`); a
//! search consults the filter and opens the archive only when the filter says a
//! required term may be present. A Bloom filter has false positives (an archive
//! opened for nothing) but never false negatives, so no hit is ever skipped.
//!
//! Terms are keyed by field id and the term's bytes as Tantivy's term
//! dictionary stores them (`Term::serialized_value_bytes`), so a query term can
//! be checked without any knowledge of the analyzer that produced it.
//!
//! The hash and the file format are fixed: a filter written today must give
//! the same answers after a toolchain or dependency upgrade.

use chronik_common::{Error, Result};
use tantivy::schema::FieldType;
use tantivy::Index;

/// Suffix of a filter's object key: `<archive key>.terms`.
pub const TERM_FILTER_SUFFIX: &str = ".terms";

/// The filter's key for an archive.
pub fn term_filter_key(archive_key: &str) -> String {
    format!("{archive_key}{TERM_FILTER_SUFFIX}")
}

const MAGIC: &[u8; 4] = b"CTF1";
/// Target false-positive rate: one archive opened for nothing in a hundred.
const FALSE_POSITIVE_RATE: f64 = 0.01;

/// A Bloom filter over `(field id, term bytes)`.
#[derive(Clone, Debug, PartialEq)]
pub struct TermFilter {
    hashes: u32,
    words: Vec<u64>,
}

impl TermFilter {
    /// An empty filter sized for `terms` entries.
    pub fn with_capacity(terms: usize) -> Self {
        let n = terms.max(1) as f64;
        let ln2 = std::f64::consts::LN_2;
        let bits = (-(n * FALSE_POSITIVE_RATE.ln()) / (ln2 * ln2)).ceil().max(64.0);
        let hashes = ((bits / n) * ln2).round().clamp(1.0, 16.0) as u32;
        let words = ((bits as u64 + 63) / 64) as usize;
        Self { hashes, words: vec![0; words] }
    }

    fn positions(&self, field: u32, value: &[u8]) -> impl Iterator<Item = (usize, u64)> + '_ {
        let (h1, h2) = hash_pair(field, value);
        let bits = (self.words.len() as u64) * 64;
        (0..self.hashes as u64).map(move |i| {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % bits;
            ((bit / 64) as usize, 1u64 << (bit % 64))
        })
    }

    pub fn insert(&mut self, field: u32, value: &[u8]) {
        let positions: Vec<_> = self.positions(field, value).collect();
        for (word, mask) in positions {
            self.words[word] |= mask;
        }
    }

    /// False only if `(field, value)` was certainly never inserted.
    pub fn may_contain(&self, field: u32, value: &[u8]) -> bool {
        self.positions(field, value).all(|(word, mask)| self.words[word] & mask != 0)
    }

    /// The filter of every term of every indexed text and JSON field of `index`.
    pub fn build(index: &Index) -> Result<Self> {
        let schema = index.schema();
        let fields: Vec<_> = schema
            .fields()
            .filter(|(_, entry)| match entry.field_type() {
                FieldType::Str(opts) => opts.get_indexing_options().is_some(),
                FieldType::JsonObject(opts) => opts.get_text_indexing_options().is_some(),
                _ => false,
            })
            .map(|(field, _)| field)
            .collect();
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()
            .map_err(|e: tantivy::TantivyError| Error::Internal(format!("term filter reader: {e}")))?;
        let searcher = reader.searcher();
        let mut terms: Vec<(u32, Vec<u8>)> = Vec::new();
        for segment in searcher.segment_readers() {
            for &field in &fields {
                let inverted = segment
                    .inverted_index(field)
                    .map_err(|e| Error::Internal(format!("term filter: {e}")))?;
                let mut stream = inverted
                    .terms()
                    .stream()
                    .map_err(|e| Error::Internal(format!("term filter: {e}")))?;
                while stream.advance() {
                    terms.push((field.field_id(), stream.key().to_vec()));
                }
            }
        }
        terms.sort_unstable();
        terms.dedup();
        let mut filter = Self::with_capacity(terms.len());
        for (field, value) in &terms {
            filter.insert(*field, value);
        }
        Ok(filter)
    }

    /// `CTF1`, hashes (u32 LE), word count (u32 LE), words (u64 LE), CRC-32 of
    /// everything before it (u32 LE).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.words.len() * 8);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.hashes.to_le_bytes());
        out.extend_from_slice(&(self.words.len() as u32).to_le_bytes());
        for w in &self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// `None` for anything that is not an intact filter: a damaged or foreign
    /// file must never be read as "no terms here".
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 16 || &bytes[..4] != MAGIC {
            return None;
        }
        let (body, crc) = bytes.split_at(bytes.len() - 4);
        if crc32fast::hash(body) != u32::from_le_bytes(crc.try_into().ok()?) {
            return None;
        }
        let hashes = u32::from_le_bytes(body[4..8].try_into().ok()?);
        let words = u32::from_le_bytes(body[8..12].try_into().ok()?) as usize;
        if !(1..=16).contains(&hashes) || words == 0 || body.len() != 12 + words * 8 {
            return None;
        }
        let words = body[12..]
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
            .collect();
        Some(Self { hashes, words })
    }

    /// Bytes held in memory.
    pub fn memory_bytes(&self) -> usize {
        self.words.len() * 8
    }
}

/// Two independent 64-bit hashes of `(field, value)`: FNV-1a, finished with
/// SplitMix64. Fixed forever — see the module docs.
fn hash_pair(field: u32, value: &[u8]) -> (u64, u64) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in field.to_le_bytes().iter().chain(value) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let h1 = splitmix64(h);
    let h2 = splitmix64(h ^ 0x9e37_79b9_7f4a_7c15) | 1;
    (h1, h2)
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_a_false_negative_and_few_false_positives() {
        let mut f = TermFilter::with_capacity(10_000);
        for i in 0..10_000u32 {
            f.insert(7, format!("word{i}").as_bytes());
        }
        for i in 0..10_000u32 {
            assert!(f.may_contain(7, format!("word{i}").as_bytes()));
        }
        let fp = (0..10_000u32).filter(|i| f.may_contain(7, format!("other{i}").as_bytes())).count();
        assert!(fp < 300, "{fp} false positives in 10,000 (target 1%)");
        // The field is part of the key.
        let fp_field = (0..10_000u32).filter(|i| f.may_contain(8, format!("word{i}").as_bytes())).count();
        assert!(fp_field < 300);
    }

    #[test]
    fn round_trips_and_rejects_damage() {
        let mut f = TermFilter::with_capacity(100);
        f.insert(1, b"coffee");
        let bytes = f.to_bytes();
        assert_eq!(TermFilter::from_bytes(&bytes), Some(f));
        let mut bad = bytes.clone();
        bad[20] ^= 1;
        assert_eq!(TermFilter::from_bytes(&bad), None);
        assert_eq!(TermFilter::from_bytes(b"CTF1short"), None);
        assert_eq!(TermFilter::from_bytes(&bytes[..bytes.len() - 1]), None);
    }

    /// The hash may never change: filters on disk depend on it.
    #[test]
    fn the_hash_is_fixed() {
        assert_eq!(hash_pair(3, b"coffee"), hash_pair(3, b"coffee"));
        let (a, b) = hash_pair(3, b"coffee");
        assert_eq!((a, b), (FIXED_A, FIXED_B), "hash_pair changed: every stored filter is now wrong");
    }
    const FIXED_A: u64 = 6_925_793_023_419_894_962;
    const FIXED_B: u64 = 10_868_684_035_689_932_531;
}
