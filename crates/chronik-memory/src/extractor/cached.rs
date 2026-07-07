//! Disk-backed extraction cache — wraps any [`Extractor`] and replays
//! previously-computed extractions instead of calling the LLM.
//!
//! # Why
//!
//! LLM extraction dominates eval cost: a 500-item LongMemEval fleet spends
//! ~$12-13 on extraction (5,500 calls × ~16K tokens) and ~$1-2 on everything
//! else. Extraction output only changes when the extractor's code or prompt
//! changes — yet every fleet run before this cache re-extracted the same 500
//! conversations. Four runs ≈ €50 of identical extractions.
//!
//! # Cache key
//!
//! `sha256(inner.id() ‖ turn₀.role ‖ turn₀.content ‖ turn₀.ts? ‖ turn₁… )`
//!
//! - `inner.id()` embeds the provider + prompt version (e.g.
//!   `chain[rules@v1+openai-v3]`), so a prompt bump automatically misses and
//!   re-extracts — no manual invalidation.
//! - Turn timestamps participate because they flow into extraction output
//!   (dated excerpts, WS-3.2).
//! - The key ignores namespace/tenant: the same conversation chunk extracted
//!   under a different eval tenant reuses the cache. That is the point.
//!
//! # Format
//!
//! One JSON file per (inner-id, chunk) under the cache dir:
//! `{dir}/{key[0..2]}/{key}.json` (two-level fan-out keeps directories
//! listable at 100K+ entries). Contents: `CacheEntry { extractor_id,
//! extracted }`. Corrupt or unreadable files are treated as misses and
//! overwritten — the cache is always safe to delete wholesale.

use super::{Extracted, Extractor, Turn};
use crate::error::Result;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Wire format of one cache file.
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    /// `inner.id()` at write time — belt-and-braces guard on top of the key
    /// (the id is already hashed into the filename).
    extractor_id: String,
    /// The cached extraction result, verbatim.
    extracted: Vec<Extracted>,
}

/// Cache hit/miss counters, exposed for eval-harness logging.
#[derive(Debug, Default)]
pub struct CacheStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub write_errors: AtomicU64,
}

/// Disk-backed extraction cache. Wraps any inner [`Extractor`].
///
/// Cheap to clone via `Arc` in the usual extractor position. All I/O is
/// blocking `std::fs` moved onto the blocking pool — cache files are a few
/// KB and this path is dwarfed by the LLM latency it replaces.
#[derive(Debug)]
pub struct CachedExtractor {
    inner: Arc<dyn Extractor>,
    dir: PathBuf,
    stats: Arc<CacheStats>,
}

impl CachedExtractor {
    /// Wrap `inner` with a cache rooted at `dir` (created if absent).
    pub fn new(inner: Arc<dyn Extractor>, dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| {
            crate::error::MemoryError::Config(format!(
                "extraction cache dir {dir:?} not creatable: {e}"
            ))
        })?;
        info!("📦 CachedExtractor active: dir={:?} inner={}", dir, inner.id());
        Ok(Self {
            inner,
            dir,
            stats: Arc::new(CacheStats::default()),
        })
    }

    /// Hit/miss counters.
    pub fn stats(&self) -> Arc<CacheStats> {
        self.stats.clone()
    }

    fn key(&self, turns: &[Turn]) -> String {
        let mut h = Sha256::new();
        h.update(self.inner.id().as_bytes());
        for t in turns {
            h.update([0xFF]); // turn separator — prevents boundary ambiguity
            h.update(t.role.as_bytes());
            h.update([0xFE]);
            h.update(t.content.as_bytes());
            if let Some(ts) = t.ts {
                h.update([0xFD]);
                h.update(ts.timestamp_millis().to_le_bytes());
            }
        }
        hex::encode(h.finalize())
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(&key[0..2]).join(format!("{key}.json"))
    }

    fn read_entry(path: &Path, expect_id: &str) -> Option<Vec<Extracted>> {
        let bytes = std::fs::read(path).ok()?;
        let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        if entry.extractor_id != expect_id {
            // Hash collision across ids is practically impossible; this
            // catches manual file moves / dir mixups.
            warn!(
                "extraction cache id mismatch at {:?}: {} != {} — treating as miss",
                path, entry.extractor_id, expect_id
            );
            return None;
        }
        Some(entry.extracted)
    }

    fn write_entry(path: &Path, id: &str, extracted: &[Extracted]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entry = CacheEntry {
            extractor_id: id.to_string(),
            extracted: extracted.to_vec(),
        };
        // Write-then-rename so a crash mid-write can't leave a truncated
        // file that later parses as an empty extraction.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&entry)?)?;
        std::fs::rename(&tmp, path)
    }
}

#[async_trait]
impl Extractor for CachedExtractor {
    fn id(&self) -> &str {
        // Transparent wrapper: provenance records the REAL extractor, so
        // cached and fresh runs produce byte-identical `source.extractor`
        // fields and stay comparable.
        self.inner.id()
    }

    async fn extract(&self, turns: &[Turn]) -> Result<Vec<Extracted>> {
        if turns.is_empty() {
            return Ok(vec![]);
        }
        let key = self.key(turns);
        let path = self.path_for(&key);
        let expect_id = self.inner.id().to_string();

        // Read on the blocking pool.
        let read_path = path.clone();
        let read_id = expect_id.clone();
        let cached = tokio::task::spawn_blocking(move || Self::read_entry(&read_path, &read_id))
            .await
            .unwrap_or(None);

        if let Some(extracted) = cached {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            debug!("extraction cache HIT {} ({} memories)", &key[0..12], extracted.len());
            return Ok(extracted);
        }

        self.stats.misses.fetch_add(1, Ordering::Relaxed);
        debug!("extraction cache MISS {} — calling inner extractor", &key[0..12]);
        let extracted = self.inner.extract(turns).await?;

        let write_path = path.clone();
        let write_id = expect_id;
        let to_write = extracted.clone();
        let stats = self.stats.clone();
        // Fire the write on the blocking pool; a failed write only costs a
        // future re-extraction, never the current result.
        tokio::task::spawn_blocking(move || {
            if let Err(e) = Self::write_entry(&write_path, &write_id, &to_write) {
                stats.write_errors.fetch_add(1, Ordering::Relaxed);
                warn!("extraction cache write failed at {:?}: {e}", write_path);
            }
        })
        .await
        .ok();

        Ok(extracted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Body, FactBody};
    use std::sync::atomic::AtomicU32;

    /// Inner double that counts real extraction calls.
    #[derive(Debug)]
    struct CountingExtractor {
        id: String,
        calls: AtomicU32,
    }

    #[async_trait]
    impl Extractor for CountingExtractor {
        fn id(&self) -> &str {
            &self.id
        }
        async fn extract(&self, turns: &[Turn]) -> Result<Vec<Extracted>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![Extracted {
                body: Body::Fact(FactBody {
                    subject: "user".into(),
                    predicate: "topic".into(),
                    object: serde_json::json!(turns[0].content.clone()),
                    polarity: "asserted".into(),
                    text: format!("user topic {}", turns[0].content),
                    speaker: "user".into(),
                }),
                key: Some("user|topic".into()),
                confidence: 0.9,
                source_indexes: vec![0],
            }])
        }
    }

    fn turn(content: &str) -> Turn {
        Turn {
            role: "user".into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: None,
        }
    }

    fn counting(id: &str) -> Arc<CountingExtractor> {
        Arc::new(CountingExtractor {
            id: id.into(),
            calls: AtomicU32::new(0),
        })
    }

    #[tokio::test]
    async fn second_extract_hits_cache_and_skips_inner() {
        let dir = tempfile::tempdir().unwrap();
        let inner = counting("x@1");
        let cached = CachedExtractor::new(inner.clone(), dir.path()).unwrap();

        let turns = vec![turn("hello world")];
        let first = cached.extract(&turns).await.unwrap();
        let second = cached.extract(&turns).await.unwrap();

        assert_eq!(inner.calls.load(Ordering::Relaxed), 1, "inner called once");
        assert_eq!(first.len(), second.len());
        assert_eq!(cached.stats().hits.load(Ordering::Relaxed), 1);
        assert_eq!(cached.stats().misses.load(Ordering::Relaxed), 1);
        // Cached result round-trips the body faithfully.
        match (&first[0].body, &second[0].body) {
            (Body::Fact(a), Body::Fact(b)) => {
                assert_eq!(a.text, b.text);
                assert_eq!(a.speaker, b.speaker);
            }
            _ => panic!("expected facts"),
        }
    }

    #[tokio::test]
    async fn different_turns_miss() {
        let dir = tempfile::tempdir().unwrap();
        let inner = counting("x@1");
        let cached = CachedExtractor::new(inner.clone(), dir.path()).unwrap();

        cached.extract(&[turn("a")]).await.unwrap();
        cached.extract(&[turn("b")]).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn extractor_id_change_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let turns = vec![turn("same content")];

        let inner_v1 = counting("x@1");
        let cached_v1 = CachedExtractor::new(inner_v1.clone(), dir.path()).unwrap();
        cached_v1.extract(&turns).await.unwrap();

        // New prompt version → different id → must NOT reuse v1's cache.
        let inner_v2 = counting("x@2");
        let cached_v2 = CachedExtractor::new(inner_v2.clone(), dir.path()).unwrap();
        cached_v2.extract(&turns).await.unwrap();

        assert_eq!(inner_v1.calls.load(Ordering::Relaxed), 1);
        assert_eq!(inner_v2.calls.load(Ordering::Relaxed), 1, "v2 must re-extract");
    }

    #[tokio::test]
    async fn corrupt_cache_file_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let inner = counting("x@1");
        let cached = CachedExtractor::new(inner.clone(), dir.path()).unwrap();

        let turns = vec![turn("payload")];
        cached.extract(&turns).await.unwrap();

        // Corrupt the file on disk.
        let key = cached.key(&turns);
        let path = cached.path_for(&key);
        std::fs::write(&path, b"{ not json").unwrap();

        let res = cached.extract(&turns).await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            2,
            "corrupt entry must fall through to inner"
        );
    }

    #[tokio::test]
    async fn turn_timestamps_participate_in_key() {
        use chrono::TimeZone;
        let dir = tempfile::tempdir().unwrap();
        let inner = counting("x@1");
        let cached = CachedExtractor::new(inner.clone(), dir.path()).unwrap();

        let mut dated = turn("same");
        dated.ts = Some(chrono::Utc.with_ymd_and_hms(2023, 5, 20, 2, 21, 0).unwrap());
        cached.extract(&[turn("same")]).await.unwrap();
        cached.extract(&[dated]).await.unwrap();
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            2,
            "dated vs undated turns must not share a cache entry"
        );
    }

    #[tokio::test]
    async fn id_is_transparent() {
        let dir = tempfile::tempdir().unwrap();
        let cached = CachedExtractor::new(counting("chain[rules@v1+openai-v3]"), dir.path()).unwrap();
        assert_eq!(cached.id(), "chain[rules@v1+openai-v3]");
    }
}
