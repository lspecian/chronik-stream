# Disaster Recovery Guide

With a remote object store (S3, GCS or Azure), a node whose data directory is
lost, or a whole cluster, can be brought back from the bucket: topics, partition
assignments, topic configs, ACLs, SCRAM users, consumer offsets and groups, and
every archived record.

On a local object store (`OBJECT_STORE_BACKEND=local` or unset) there is nothing
to recover from: the "bucket" is on the same disk. Disaster-recovery uploads
are off there.

## What is in the bucket

| Object | What | Written |
|--------|------|---------|
| `segments/<topic>/<partition>/…` | Archived records (raw segments) | When a WAL segment is sealed and indexed |
| Tantivy archives | Full-text indexes of archived records (searchable topics) | Same |
| `columnar/…` | Parquet files, only with `CHRONIK_COLUMNAR_USE_OBJECT_STORE=true` | Same |
| `dr/catalog/node-<id>.img` | The cluster catalog: topics, assignments, configs, ACLs, SCRAM users, brokers; with the Raft index it was applied through | When the applied index moves, checked every `CHRONIK_DR_UPLOAD_INTERVAL_SECS` (default 60) |
| `dr/local/node-<id>.img` | The node's local metadata: consumer offsets and groups, segment and Parquet records, watermarks, transactions, producer ids | When it changes, same interval |

All keys are under the store's prefix (`S3_PREFIX`, `GCS_PREFIX`,
`AZURE_PREFIX`). The images are snapshots of state, a few MB, not logs: the
node-local metadata WAL is never uploaded (on a long-running node it reaches
gigabytes).

Each node uploads only while it is caught up with the catalog leader, and only
images of its own **incarnation**: a node that founded an empty catalog has a
new one; a restored node continues the incarnation of the image it was restored
from. A node never overwrites an image of another incarnation. A blank node
started against the bucket of a lost cluster without restoring logs:

```
Disaster recovery uploads paused: dr/catalog/node-1.img belongs to another catalog incarnation (…)
```

and leaves the backup alone. To start a new cluster on purpose, use a new
prefix (or delete the `dr/` objects).

## What is restored

Restore runs before the catalog starts, and only when the node's data directory
is blank (no `catalog-raft/`, no `metadata_wal/`). `CHRONIK_DR_RESTORE`
decides:

| Value | Single node | Cluster node |
|-------|-------------|--------------|
| unset / `auto` | restores | never restores (logs how to) |
| `true` | restores; fails if the bucket has no image | restores; fails if the bucket has no image |
| `false` | never | never |

A restore installs the catalog image as the node's first Raft snapshot (at the
image's index and term), writes the local image as
`metadata_wal/base-state.json.gz` (the base the metadata WAL replays on top of
at every boot), and logs:

```
Restored from the object store: catalog of node 3 at index 16 (term 2, 3 topics, taken …); local state of node 1 (taken …)
```

On the first boot after a restore, Parquet records whose files did not survive
(local Parquet files died with the disk) are dropped, and columnar catch-up
rewrites those ranges from the archive (`CHRONIK_COLUMNAR_CATCHUP_DELAY_SECS`
after boot). Until it has, `/_sql` is missing those rows. Vector indexes are
local too; vector catch-up re-embeds archived records, which costs embedding
API calls on large topics.

Records produced after the last upload interval and not yet archived are lost
with the disk, as anything not yet in the bucket is.

## Procedures

### A single node lost its disk

Give it an empty data directory with the same object-store configuration and
start it. It restores automatically.

### One node of a running cluster lost its disk

Do **not** restore it from the bucket: its peers hold a newer catalog, and a
node that comes back with an older log is refused ("this node lost raft state
it had acknowledged"). Recover it from the cluster instead:

```bash
chronik-server cluster remove-node 3 --force --config cluster.toml
chronik-server cluster add-node 3 --kafka … --wal … --raft … --config cluster.toml
CHRONIK_CLUSTER_JOIN=true chronik-server start --config node3.toml   # empty data dir
```

The leader sends it the catalog; its partitions re-replicate from the other
replicas.

### The whole cluster lost its disks

Start **every** node with empty data directories, the same object-store
configuration, and `CHRONIK_DR_RESTORE=true`. Each node reads the catalog
images of the configured nodes and picks the same one (highest index, then
term, then node id), so all of them found the cluster on one catalog; each
loads its own local image. Remove `CHRONIK_DR_RESTORE` afterwards (it does
nothing on a non-blank directory, but `auto` is the safe default).

The node ids must be those of the lost cluster (local images are per node id).

## Known limits

- **Consumer groups in a cluster** are coordinated by whichever broker a client
  reaches, and their offsets are kept on that broker. A restore brings back each
  node's own offsets; it cannot merge offsets a group committed through
  different brokers.
- Parquet files and vector indexes are local unless
  `CHRONIK_COLUMNAR_USE_OBJECT_STORE=true`; they are rebuilt from the archive
  after a restore (see above).
- There is no point-in-time restore: the bucket holds the latest image of each
  node.

## Verifying

- `tests/cluster/e2e/dr_restore.sh`: one node; data directory deleted; topics,
  config, consumer offsets, every record (byte for byte), `/_sql` after
  catch-up; a control run with `CHRONIK_DR_RESTORE=false` proves the restore
  did it and that the blank node does not overwrite the backup.
- `tests/cluster/e2e/dr_cluster_restore.sh`: three nodes; every data directory
  deleted; a control run without the flag; then restored with
  `CHRONIK_DR_RESTORE=true`: the same image on every node, topics, config,
  SCRAM logins, records, offsets, acks=all writes and full ISR.
- `tests/cluster/e2e/s3_roundtrip.sh`: archived records are read back from S3
  and not at all with the bucket stopped.
