//! Disaster recovery: the catalog and each node's local metadata, kept in the
//! object store, and restored onto a blank data directory.
//!
//! Message data is already archived to the object store and read back from it.
//! What a node also needs to serve it is its metadata: the cluster catalog
//! (topics, partition assignments, ACLs, SCRAM users) and its node-local state
//! (consumer offsets and groups, segment and Parquet records, watermarks,
//! transactions). Each node uploads both as images:
//!
//! - `dr/catalog/node-<id>.img`: the catalog snapshot, the Raft index it was
//!   applied through and the term, written when the applied index moves;
//! - `dr/local/node-<id>.img`: the node-local state (not the metadata WAL,
//!   which is never compacted), written when it changes.
//!
//! Restore runs only on a blank data directory, before the catalog starts:
//!
//! - **single node**: automatically (`CHRONIK_DR_RESTORE` unset or `auto`);
//! - **cluster**: only with `CHRONIK_DR_RESTORE=true`, for the loss of the
//!   whole cluster. Every node then founds the cluster on the *same* catalog
//!   image (the newest of the configured nodes'), or their logs would disagree
//!   from the first index. One node that lost its disk in a running cluster is
//!   recovered by the cluster instead (`cluster remove-node --force`, `add-node`,
//!   start with `CHRONIK_CLUSTER_JOIN=true`); an older image from the bucket
//!   would be refused by its peers.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use chronik_common::metadata::WalMetadataStore;
use chronik_raft::BootstrapSnapshot;
use chronik_storage::object_store::{ObjectStoreConfig, ObjectStoreError, StorageBackend};
use chronik_storage::ObjectStoreTrait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::cluster_catalog::ClusterCatalog;

/// Where the restored node-local state is kept: the base the metadata WAL
/// replays on top of at every boot. Relative to the data directory.
pub const LOCAL_BASE_STATE_FILE: &str = "metadata_wal/base-state.json.gz";

/// Left by a restore; the first boot that follows reconciles the restored
/// records with what survived (see [`reconcile_after_restore`]).
const RESTORED_MARKER_FILE: &str = "metadata_wal/restored.marker";

/// This node's catalog incarnation (see [`ImageHeader::incarnation`]).
const INCARNATION_FILE: &str = "catalog-raft/dr-incarnation";

/// This node's catalog incarnation, created on first use.
pub fn incarnation(data_dir: &Path) -> Result<String> {
    let path = data_dir.join(INCARNATION_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        Ok(_) => set_incarnation(data_dir, &uuid::Uuid::new_v4().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            set_incarnation(data_dir, &uuid::Uuid::new_v4().to_string())
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn set_incarnation(data_dir: &Path, id: &str) -> Result<String> {
    let path = data_dir.join(INCARNATION_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, id).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(id.to_string())
}

const MAGIC: &[u8] = b"CHRONIKDR1\n";

/// What an image holds and where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageHeader {
    /// `catalog` or `local`.
    pub kind: String,
    pub node_id: u64,
    /// Catalog: the Raft index the snapshot was applied through.
    pub index: u64,
    /// Catalog: the node's Raft term when the image was taken.
    pub term: u64,
    pub created_ms: i64,
    /// Version of the broker that wrote it.
    pub version: String,
    /// The catalog lineage the image belongs to: created when a node founds
    /// an empty catalog, carried over by a restore. A node never overwrites
    /// an image of another incarnation (a blank node started against the same
    /// bucket would otherwise replace the backup with an empty catalog).
    #[serde(default)]
    pub incarnation: String,
}

pub fn catalog_key(node_id: u64) -> String {
    format!("dr/catalog/node-{node_id}.img")
}

pub fn local_key(node_id: u64) -> String {
    format!("dr/local/node-{node_id}.img")
}

/// `MAGIC`, header length (u32 BE), JSON header, payload.
pub fn encode_image(header: &ImageHeader, payload: &[u8]) -> Bytes {
    let header_json = serde_json::to_vec(header).expect("image header serializes");
    let mut out = Vec::with_capacity(MAGIC.len() + 4 + header_json.len() + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(header_json.len() as u32).to_be_bytes());
    out.extend_from_slice(&header_json);
    out.extend_from_slice(payload);
    Bytes::from(out)
}

pub fn decode_image(bytes: &[u8]) -> Result<(ImageHeader, &[u8])> {
    let rest = bytes.strip_prefix(MAGIC).context("not a disaster-recovery image")?;
    if rest.len() < 4 {
        bail!("truncated disaster-recovery image");
    }
    let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let rest = &rest[4..];
    if rest.len() < len {
        bail!("truncated disaster-recovery image header");
    }
    let header: ImageHeader = serde_json::from_slice(&rest[..len]).context("disaster-recovery image header")?;
    Ok((header, &rest[len..]))
}

/// `CHRONIK_DR_UPLOAD_INTERVAL_SECS`: how often changed images are uploaded
/// (default 60).
pub fn upload_interval_secs() -> u64 {
    std::env::var("CHRONIK_DR_UPLOAD_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&s: &u64| s > 0)
        .unwrap_or(60)
}

/// `CHRONIK_DR_RESTORE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePolicy {
    /// Restore a blank single node; never a cluster node.
    Auto,
    /// Restore a blank node, cluster or not; fail if there is nothing to restore.
    Always,
    Never,
}

impl RestorePolicy {
    pub fn from_env() -> Self {
        Self::parse(std::env::var("CHRONIK_DR_RESTORE").ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if matches!(v.as_str(), "true" | "1" | "yes" | "always") => Self::Always,
            Some(v) if matches!(v.as_str(), "false" | "0" | "no" | "never") => Self::Never,
            _ => Self::Auto,
        }
    }
}

/// Remote backends only: on a local backend the "bucket" is on the disk that
/// was lost.
pub fn is_remote(config: &ObjectStoreConfig) -> bool {
    matches!(
        config.backend,
        StorageBackend::S3 { .. } | StorageBackend::Gcs { .. } | StorageBackend::Azure { .. }
    )
}

/// True when the data directory holds no catalog and no node-local metadata.
pub fn data_dir_is_blank(data_dir: &Path) -> bool {
    let has_files = |dir: PathBuf| {
        std::fs::read_dir(dir).map(|mut entries| entries.next().is_some()).unwrap_or(false)
    };
    !has_files(data_dir.join("catalog-raft")) && !has_files(data_dir.join("metadata_wal"))
}

async fn get_image(store: &dyn ObjectStoreTrait, key: &str) -> Result<Option<Bytes>> {
    match store.get(key).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(ObjectStoreError::NotFound { .. }) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {key}: {e}")),
    }
}

/// Restore the metadata of a blank node from the object store, as
/// `CHRONIK_DR_RESTORE` allows. Returns the catalog snapshot to found the
/// catalog on; writes the node-local state to [`LOCAL_BASE_STATE_FILE`].
///
/// `cluster_voters` is `Some` in cluster mode (the founding voters, which are
/// the nodes whose images are considered); `None` for a single node.
pub async fn restore_if_blank(
    data_dir: &Path,
    node_id: u64,
    cluster_voters: Option<&[u64]>,
    object_store: Option<&ObjectStoreConfig>,
) -> Result<Option<BootstrapSnapshot>> {
    restore_with_policy(data_dir, node_id, cluster_voters, object_store, RestorePolicy::from_env()).await
}

async fn restore_with_policy(
    data_dir: &Path,
    node_id: u64,
    cluster_voters: Option<&[u64]>,
    object_store: Option<&ObjectStoreConfig>,
    policy: RestorePolicy,
) -> Result<Option<BootstrapSnapshot>> {
    if policy == RestorePolicy::Never {
        return Ok(None);
    }
    if !data_dir_is_blank(data_dir) {
        if policy == RestorePolicy::Always {
            warn!("CHRONIK_DR_RESTORE=true but {} already holds metadata; nothing restored", data_dir.display());
        }
        return Ok(None);
    }
    let Some(config) = object_store.filter(|c| is_remote(c)) else {
        if policy == RestorePolicy::Always {
            bail!("CHRONIK_DR_RESTORE=true needs a remote object store (S3, GCS or Azure)");
        }
        return Ok(None);
    };
    let store: Arc<dyn ObjectStoreTrait> = Arc::from(
        chronik_storage::object_store::ObjectStoreFactory::create(config.clone())
            .await
            .map_err(|e| anyhow::anyhow!("opening the object store for restore: {e}"))?,
    );
    let candidates: Vec<u64> = match cluster_voters {
        Some(voters) => voters.to_vec(),
        None => vec![node_id],
    };

    if cluster_voters.is_some() && policy == RestorePolicy::Auto {
        let mut found = Vec::new();
        for id in &candidates {
            if get_image(store.as_ref(), &catalog_key(*id)).await?.is_some() {
                found.push(*id);
            }
        }
        if !found.is_empty() {
            info!(
                "Blank data directory, and the object store holds catalog images of nodes {:?}. A cluster \
                 node is not restored automatically: if the whole cluster was lost, start every node with \
                 CHRONIK_DR_RESTORE=true; if only this node was, recover it from the cluster (remove-node \
                 --force, add-node, CHRONIK_CLUSTER_JOIN=true).",
                found
            );
        }
        return Ok(None);
    }

    // The newest catalog image among the candidates. Every restoring node
    // picks the same one: highest index, then term, then node id.
    let mut best: Option<(ImageHeader, Bytes)> = None;
    for id in &candidates {
        let Some(bytes) = get_image(store.as_ref(), &catalog_key(*id)).await? else {
            continue;
        };
        let (header, _) = decode_image(&bytes).with_context(|| format!("catalog image of node {id}"))?;
        if header.kind != "catalog" {
            bail!("{} is a {} image, not a catalog image", catalog_key(*id), header.kind);
        }
        let better = match &best {
            None => true,
            Some((b, _)) => (header.index, header.term, header.node_id) > (b.index, b.term, b.node_id),
        };
        if better {
            best = Some((header, bytes));
        }
    }
    let Some((header, bytes)) = best else {
        if policy == RestorePolicy::Always {
            bail!(
                "CHRONIK_DR_RESTORE=true but the object store holds no catalog image for nodes {:?} (keys dr/catalog/node-<id>.img)",
                candidates
            );
        }
        info!("Blank data directory and no disaster-recovery image in the object store; starting empty");
        return Ok(None);
    };
    let (_, payload) = decode_image(&bytes)?;
    let catalog = chronik_common::metadata::catalog::CatalogState::from_snapshot(payload)
        .map_err(|e| anyhow::anyhow!("catalog image of node {}: {e}", header.node_id))?;

    // The restored node continues the image's lineage, so its uploads replace
    // the images it was restored from.
    let lineage = if header.incarnation.is_empty() { uuid::Uuid::new_v4().to_string() } else { header.incarnation.clone() };
    set_incarnation(data_dir, &lineage)?;

    // This node's local state: offsets, groups, segment records, watermarks.
    let local_restored = match get_image(store.as_ref(), &local_key(node_id)).await? {
        Some(bytes) => {
            let (local_header, local_payload) = decode_image(&bytes).context("local state image")?;
            if local_header.kind != "local" {
                bail!("{} is a {} image, not a local state image", local_key(node_id), local_header.kind);
            }
            let path = data_dir.join(LOCAL_BASE_STATE_FILE);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, local_payload).with_context(|| format!("writing {}", tmp.display()))?;
            std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
            std::fs::write(data_dir.join(RESTORED_MARKER_FILE), b"")
                .with_context(|| format!("writing {}", RESTORED_MARKER_FILE))?;
            Some(local_header.created_ms)
        }
        None => {
            warn!(
                "No local state image for node {node_id} ({}): topics are restored, but this node's consumer \
                 offsets, groups and segment records are not",
                local_key(node_id)
            );
            None
        }
    };

    info!(
        "Restored from the object store: catalog of node {} at index {} (term {}, {} topics, taken {}){}",
        header.node_id,
        header.index,
        header.term,
        catalog.topics().count(),
        chrono::DateTime::from_timestamp_millis(header.created_ms).map(|t| t.to_rfc3339()).unwrap_or_default(),
        match local_restored {
            Some(ms) => format!(
                "; local state of node {node_id} (taken {})",
                chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339()).unwrap_or_default()
            ),
            None => String::new(),
        }
    );
    Ok(Some(BootstrapSnapshot { index: header.index, term: header.term, data: payload.to_vec() }))
}

/// Load a restored base state into the node-local store, before the metadata
/// WAL replays on top of it. A no-op when there is none.
pub async fn load_local_base_state(data_dir: &Path, store: &WalMetadataStore) -> Result<bool> {
    let path = data_dir.join(LOCAL_BASE_STATE_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    store
        .import_state(&bytes)
        .await
        .map_err(|e| anyhow::anyhow!("loading {}: {e}", path.display()))?;
    info!("Loaded restored node-local metadata from {}", path.display());
    Ok(true)
}

/// First boot after a restore: drop the restored Parquet records whose files
/// did not survive (local files died with the disk; with columnar uploads on,
/// the object store still has them). Columnar catch-up then rewrites those
/// ranges from the archive: while a record claims a range, nothing does, and
/// `/_sql` would answer without those rows for good.
pub async fn reconcile_after_restore(
    data_dir: &Path,
    metadata: &dyn chronik_common::metadata::MetadataStore,
    object_store: &dyn ObjectStoreTrait,
) -> Result<()> {
    let marker = data_dir.join(RESTORED_MARKER_FILE);
    if !marker.exists() {
        return Ok(());
    }
    let mut kept = 0usize;
    let mut dropped = 0usize;
    for topic in metadata.list_topics().await.map_err(|e| anyhow::anyhow!("listing topics: {e}"))? {
        let segments = metadata
            .list_parquet_segments(&topic.name, None)
            .await
            .map_err(|e| anyhow::anyhow!("listing Parquet segments of {}: {e}", topic.name))?;
        for seg in segments {
            let survives = Path::new(&seg.object_store_path).exists()
                || matches!(object_store.exists(&seg.object_store_path).await, Ok(true));
            if survives {
                kept += 1;
                continue;
            }
            metadata
                .delete_parquet_segment(&seg.topic, seg.partition, &seg.segment_id)
                .await
                .map_err(|e| anyhow::anyhow!("dropping Parquet record {}: {e}", seg.segment_id))?;
            dropped += 1;
        }
    }
    std::fs::remove_file(&marker).with_context(|| format!("removing {}", marker.display()))?;
    info!(
        "After the restore: {dropped} Parquet segment record(s) whose files were lost dropped (columnar \
         catch-up rewrites them from the archive), {kept} kept"
    );
    Ok(())
}

fn fingerprint(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// An image under this node's keys that belongs to another incarnation:
/// `(key, its incarnation)`.
async fn foreign_image(
    store: &dyn ObjectStoreTrait,
    node_id: u64,
    ours: &str,
) -> Result<Option<(String, String)>> {
    for key in [catalog_key(node_id), local_key(node_id)] {
        if let Some(bytes) = get_image(store, &key).await? {
            let (header, _) = decode_image(&bytes).with_context(|| format!("existing {key}"))?;
            if header.incarnation != ours {
                return Ok(Some((key, header.incarnation)));
            }
        }
    }
    Ok(None)
}

/// Upload this node's catalog and local images whenever they change.
pub fn spawn_uploader(
    store: Arc<dyn ObjectStoreTrait>,
    catalog: Arc<ClusterCatalog>,
    local: Arc<WalMetadataStore>,
    node_id: u64,
    incarnation: String,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_index = 0u64;
        let mut last_local = 0u64;
        let mut failing = false;
        // Until the images under this node's keys are known to be of this
        // incarnation (or absent), nothing is uploaded.
        let mut owned = false;
        let mut refused_logged = false;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            // A node still catching up would upload an image older than its
            // peers' (or empty); the newest one wins at restore, but there is
            // no point writing it.
            if !catalog.is_caught_up() {
                continue;
            }
            if !owned {
                match foreign_image(store.as_ref(), node_id, &incarnation).await {
                    Ok(None) => owned = true,
                    Ok(Some((key, theirs))) => {
                        if !refused_logged {
                            warn!(
                                "Disaster recovery uploads paused: {key} belongs to another catalog incarnation ({theirs}; \
                                 this node is {incarnation}). Not overwriting it. If this node should have been \
                                 restored from it, stop it, empty its data directory and start it again (with \
                                 CHRONIK_DR_RESTORE=true in a cluster); if the old backup is unwanted, delete the \
                                 dr/ objects or use another prefix."
                            );
                            refused_logged = true;
                        }
                        continue;
                    }
                    Err(e) => {
                        warn!("Disaster recovery: could not check existing images: {e}");
                        continue;
                    }
                }
            }
            let result: Result<()> = async {
                let (index, snapshot) = {
                    let state = catalog.read();
                    (state.applied_index(), state.to_snapshot())
                };
                if index != last_index {
                    let header = ImageHeader {
                        kind: "catalog".into(),
                        node_id,
                        index,
                        term: catalog.status().term,
                        created_ms: chrono::Utc::now().timestamp_millis(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        incarnation: incarnation.clone(),
                    };
                    store
                        .put(&catalog_key(node_id), encode_image(&header, &snapshot))
                        .await
                        .map_err(|e| anyhow::anyhow!("uploading the catalog image: {e}"))?;
                    if last_index == 0 {
                        info!("Disaster recovery: catalog image uploaded ({}, index {index})", catalog_key(node_id));
                    }
                    last_index = index;
                }
                let state = local.export_state().await.map_err(|e| anyhow::anyhow!("exporting local state: {e}"))?;
                let fp = fingerprint(&state);
                if fp != last_local {
                    let header = ImageHeader {
                        kind: "local".into(),
                        node_id,
                        index: 0,
                        term: 0,
                        created_ms: chrono::Utc::now().timestamp_millis(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        incarnation: incarnation.clone(),
                    };
                    store
                        .put(&local_key(node_id), encode_image(&header, &state))
                        .await
                        .map_err(|e| anyhow::anyhow!("uploading the local state image: {e}"))?;
                    if last_local == 0 {
                        info!(
                            "Disaster recovery: local state image uploaded ({}, {} bytes)",
                            local_key(node_id),
                            state.len()
                        );
                    }
                    last_local = fp;
                }
                Ok(())
            }
            .await;
            match result {
                Ok(()) if failing => {
                    info!("Disaster recovery uploads succeed again");
                    failing = false;
                }
                Ok(()) => {}
                Err(e) if !failing => {
                    warn!("Disaster recovery upload failed (retrying every {}s): {e}", interval.as_secs());
                    failing = true;
                }
                Err(_) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: &str, node_id: u64, index: u64) -> ImageHeader {
        ImageHeader { kind: kind.into(), node_id, index, term: 4, created_ms: 1, version: "t".into(), incarnation: "i".into() }
    }

    #[test]
    fn images_round_trip_and_reject_garbage() {
        let h = header("catalog", 2, 77);
        let bytes = encode_image(&h, b"payload");
        let (back, payload) = decode_image(&bytes).unwrap();
        assert_eq!((back, payload), (h, &b"payload"[..]));
        assert!(decode_image(b"something else").is_err());
        assert!(decode_image(&bytes[..MAGIC.len() + 2]).is_err());
    }

    #[test]
    fn restore_policy_parses() {
        assert_eq!(RestorePolicy::parse(None), RestorePolicy::Auto);
        assert_eq!(RestorePolicy::parse(Some("auto")), RestorePolicy::Auto);
        assert_eq!(RestorePolicy::parse(Some("TRUE")), RestorePolicy::Always);
        assert_eq!(RestorePolicy::parse(Some("false")), RestorePolicy::Never);
    }

    #[test]
    fn a_data_dir_with_metadata_is_not_blank() {
        let dir = tempfile::tempdir().unwrap();
        assert!(data_dir_is_blank(dir.path()));
        std::fs::create_dir_all(dir.path().join("segments/x")).unwrap();
        assert!(data_dir_is_blank(dir.path()), "data alone does not make it non-blank");
        std::fs::create_dir_all(dir.path().join("catalog-raft")).unwrap();
        assert!(data_dir_is_blank(dir.path()), "an empty catalog directory is still blank");
        std::fs::write(dir.path().join("catalog-raft/state"), b"x").unwrap();
        assert!(!data_dir_is_blank(dir.path()));
    }

    #[tokio::test]
    async fn never_and_local_backends_restore_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let local = ObjectStoreConfig::local(dir.path().join("store").to_string_lossy().into());
        assert_eq!(restore_with_policy(dir.path(), 1, None, Some(&local), RestorePolicy::Auto).await.unwrap(), None);
        assert!(restore_with_policy(dir.path(), 1, None, Some(&local), RestorePolicy::Always).await.is_err());
        assert_eq!(restore_with_policy(dir.path(), 1, None, None, RestorePolicy::Never).await.unwrap(), None);
    }
}
