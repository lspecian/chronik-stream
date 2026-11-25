//! Damaged archives: found, kept out of searches and merges, rebuilt.
//!
//! Every Tantivy file of an archive ends with a checksum of its contents.
//! An archive is checked against them ([`TantivySegmentReader::verify`]) when
//! the search cache first opens it, when a merge reads it as a source, and by
//! a background scrub of the archives on this node's disk (an archive no
//! search touches would otherwise rot unnoticed until the day it is needed).
//!
//! A damaged archive is recorded here. Searches skip it at once (they answer
//! with what the other sources hold and say the answer is incomplete); merges
//! leave it out; the repair task rebuilds it from the raw tier
//! ([`crate::reindex::rebuild`], forced) under a new name, swapped into the
//! catalogue as a reindex does. When the raw tier no longer holds all of its
//! records (retention, legacy re-archiving), it cannot be rebuilt: it stays
//! recorded and is logged on every pass.
//!
//! Settings: `CHRONIK_ARCHIVE_SCRUB` (default `true`),
//! `CHRONIK_ARCHIVE_SCRUB_DELAY_SECS` (600) after boot, then every
//! `CHRONIK_ARCHIVE_SCRUB_INTERVAL_SECS` (86400). Repairs run every
//! `CHRONIK_ARCHIVE_REPAIR_INTERVAL_SECS` (60).
//!
//! [`TantivySegmentReader::verify`]: crate::tantivy_segment::TantivySegmentReader::verify

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tracing::{error, info, warn};

use crate::reindex::Outcome;
use crate::tantivy_segment::TantivySegmentReader;
use crate::wal_indexer::WalIndexer;

/// Damaged archives by object key, with what was wrong.
fn damaged() -> &'static Mutex<HashMap<String, String>> {
    static DAMAGED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    DAMAGED.get_or_init(Default::default)
}

/// Record the archive at `key` as damaged.
pub fn mark_damaged(key: &str, reason: &str) {
    let first = damaged().lock().unwrap().insert(key.to_string(), reason.to_string()).is_none();
    if first {
        error!(archive = key, "archive is damaged; searches skip it until it is rebuilt: {}", reason);
    }
}

/// Whether the archive at `key` is known to be damaged.
pub fn is_damaged(key: &str) -> bool {
    damaged().lock().unwrap().contains_key(key)
}

/// Archives known to be damaged.
pub fn damaged_archives() -> Vec<(String, String)> {
    let mut v: Vec<_> = damaged().lock().unwrap().iter().map(|(k, r)| (k.clone(), r.clone())).collect();
    v.sort();
    v
}

/// Damaged archives rebuilt since boot.
pub fn repaired_total() -> u64 {
    REPAIRED.load(std::sync::atomic::Ordering::Relaxed)
}
static REPAIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn forget(key: &str) {
    damaged().lock().unwrap().remove(key);
}

/// Check an opened archive; a damaged one is recorded and refused.
pub fn check(key: &str, reader: &TantivySegmentReader) -> chronik_common::Result<()> {
    reader.verify().inspect_err(|e| mark_damaged(key, &e.to_string()))
}

/// Rebuild every recorded damaged archive the raw tier can rebuild. Returns
/// how many were rebuilt.
pub async fn repair_pass(indexer: &WalIndexer, grace: Duration) -> usize {
    let pending = damaged_archives();
    if pending.is_empty() {
        return 0;
    }
    let catalogue = indexer.segment_index().get_all_segments().await.unwrap_or_default();
    let mut rebuilt = 0;
    for (key, reason) in pending {
        let Some(entry) = catalogue.iter().find(|s| s.object_store_path == key) else {
            forget(&key); // no longer catalogued (merged, truncated, deleted): nothing to repair
            continue;
        };
        match crate::reindex::rebuild(indexer, entry, true, grace).await {
            Ok(Outcome::Rebuilt) => {
                forget(&key);
                rebuilt += 1;
                REPAIRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                info!(archive = %key, topic = %entry.topic, partition = entry.partition, "damaged archive rebuilt from the raw tier");
            }
            Ok(Outcome::Raced) => forget(&key),
            Ok(Outcome::Current) => {} // not returned when forced
            Ok(Outcome::Incomplete { found, expected }) => error!(
                archive = %key, found, expected, reason = %reason,
                "damaged archive cannot be rebuilt: the raw tier no longer holds all of its records"
            ),
            Err(e) => warn!(archive = %key, "rebuilding a damaged archive failed (retried next pass): {}", e),
        }
    }
    rebuilt
}

/// Verify every archive held on this node's disk; damaged ones are recorded.
/// Archives in a remote store are verified when they are opened instead
/// (downloading all of them to check them would cost more than it saves).
/// Returns (checked, damaged).
pub async fn scrub_pass(indexer: &WalIndexer) -> (usize, usize) {
    let store = indexer.object_store();
    let catalogue = indexer.segment_index().get_all_segments().await.unwrap_or_default();
    let (mut checked, mut bad) = (0, 0);
    for entry in catalogue {
        let Some(path) = store.local_path(&entry.object_store_path).filter(|p| p.is_file()) else {
            continue;
        };
        if is_damaged(&entry.object_store_path) {
            continue;
        }
        let key = entry.object_store_path.clone();
        let verdict = tokio::task::spawn_blocking(move || -> chronik_common::Result<()> {
            let reader = TantivySegmentReader::open_archive_file(&path)?;
            reader.verify()
        })
        .await;
        checked += 1;
        match verdict {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // Still catalogued? A merge may have replaced it meanwhile.
                if store.local_path(&key).is_some_and(|p| p.is_file()) {
                    bad += 1;
                    mark_damaged(&key, &e.to_string());
                }
            }
            Err(e) => warn!(archive = %key, "scrub task failed: {}", e),
        }
    }
    (checked, bad)
}

/// Run repairs, and the scrub, in the background.
pub fn spawn(indexer: Arc<WalIndexer>) {
    let env = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string());
    let secs = |name: &str, default: u64| env(name).and_then(|v| v.parse().ok()).unwrap_or(default);
    let grace = Duration::from_secs(secs("CHRONIK_ARCHIVE_MERGE_GRACE_SECS", 60));
    let repair_every = Duration::from_secs(secs("CHRONIK_ARCHIVE_REPAIR_INTERVAL_SECS", 60).max(1));
    let repairer = indexer.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(repair_every).await;
            repair_pass(&repairer, grace).await;
        }
    });

    if matches!(env("CHRONIK_ARCHIVE_SCRUB").as_deref(), Some("false" | "0" | "off" | "no")) {
        return;
    }
    let delay = Duration::from_secs(secs("CHRONIK_ARCHIVE_SCRUB_DELAY_SECS", 600));
    let interval = Duration::from_secs(secs("CHRONIK_ARCHIVE_SCRUB_INTERVAL_SECS", 86_400).max(1));
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        loop {
            let (checked, bad) = scrub_pass(&indexer).await;
            if bad > 0 {
                warn!(checked, damaged = bad, "archive scrub found damaged archives; rebuilding them");
            } else {
                info!(checked, "archive scrub: every local archive passed its checksums");
            }
            tokio::time::sleep(interval).await;
        }
    });
}
