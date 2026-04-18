//! Bridge between the chronik-storage `ColdFlushListener` trait and the
//! chronik-columnar `HotVectorIndex`. Lives here because chronik-columnar
//! does not depend on chronik-storage (the trait crate), so the impl must
//! live somewhere that depends on both — this crate qualifies.
//!
//! See `docs/ROADMAP_HOT_PATH.md` (HP-2.6).

use std::sync::Arc;

use chronik_columnar::hot_vector_index::HotVectorIndex;
use chronik_storage::wal_indexer::ColdFlushListener;
use tracing::{debug, trace};

/// Adapter that implements `ColdFlushListener` against an `Arc<HotVectorIndex>`.
/// Fires eviction on a detached tokio task so the WalIndexer thread never
/// waits on the hot index write lock.
pub struct HotVectorColdFlushAdapter {
    inner: Arc<HotVectorIndex>,
}

impl HotVectorColdFlushAdapter {
    pub fn new(inner: Arc<HotVectorIndex>) -> Arc<Self> {
        Arc::new(Self { inner })
    }
}

impl ColdFlushListener for HotVectorColdFlushAdapter {
    fn notify_cold_flushed(&self, topic: String, partition: i32, max_offset: i64) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            match inner.evict_up_to(&topic, partition, max_offset).await {
                Ok(n) if n > 0 => {
                    trace!(%topic, partition, evicted = n, "hot vector evicted");
                }
                Ok(_) => {}
                Err(e) => {
                    debug!(%topic, partition, "hot vector eviction failed: {}", e);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronik_columnar::hot_vector_index::{HotDistanceMetric, HotVectorConfig};

    #[tokio::test]
    async fn adapter_evicts_asynchronously() {
        let idx = Arc::new(HotVectorIndex::new(HotVectorConfig {
            max_vectors_per_partition: 100,
            dimensions: 2,
            metric: HotDistanceMetric::Cosine,
        }));
        for i in 0..10 {
            idx.add_vector("t", 0, i, vec![i as f32, 0.0]).await.unwrap();
        }
        let adapter = HotVectorColdFlushAdapter::new(Arc::clone(&idx));
        adapter.notify_cold_flushed("t".to_string(), 0, 4);
        for _ in 0..50 {
            if idx.vector_count("t", 0).await == 5 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(idx.vector_count("t", 0).await, 5);
    }
}
