# Cluster catalog consistency — shared problem, goals and acceptance

Status: Option A implemented in v2.17.0; Option B not pursued. The acceptance suite (section 7) lives in `tests/cluster/catalog/t*.sh`. Drafted 2026-11-14; the two candidate designs:
[CLUSTER_CATALOG_OPTION_A_RAFT.md](CLUSTER_CATALOG_OPTION_A_RAFT.md) and
[CLUSTER_CATALOG_OPTION_B_PARTITION.md](CLUSTER_CATALOG_OPTION_B_PARTITION.md).
Both are built on separate branches and judged by the same acceptance suite (section 7).

## 1. Problem

Every node keeps its own copy of the cluster's metadata and changes it locally. Changes are
pushed to peers fire-and-forget, and an anti-entropy loop re-sends the catalog every 300 s to
paper over what the push loses. There is no single writer, no agreed order, and no way for a
node to know it is behind.

Observed consequence (Thunderbird, v2.16.0, minutes after a roll): a topic created with
`--partitions 1` received records in partitions 1 and 2. The Raft leader created it explicitly
with 1 partition while a lagging node auto-created it with 3 and handed producers 3 leaders.
Once the two versions met, the explicit config won and partitions 1–2 disappeared from
Metadata responses. The records written there are unreachable for group consumers.

What the code does today (verified 2026-11-14 against `1dbca7e`):

| Fact | Where |
|---|---|
| Only TopicCreated, TopicDeleted, PartitionAssigned, BrokerRegistered (and TopicUpdated, sent as TopicCreated) are ever replicated. Every other event hits `_ => None`. | `integrated_server/builder.rs:330-408` |
| **ACL and SCRAM changes are never replicated** — a revoked user stays valid on other nodes. (Read in code; not yet reproduced on a cluster.) | same |
| `CatalogSnapshot` is dropped by the same adapter, so catalog pruning never runs in production. | same, `wal_metadata_store.rs:822-923` |
| A topic gets a different `id` on every node (id = event UUID, re-minted by the replicator). | `wal_metadata_store.rs:294`, `metadata_wal_replication.rs:294` |
| Only CreateTopics is forwarded to the Raft leader. DeleteTopics, CreatePartitions, AlterConfigs, ACL, SCRAM, auto-create, `/admin/rebalance` and the partition rebalancer (on **every** node, no leader check) write locally. | `kafka_handler.rs:181-2252`, `partition_rebalancer.rs:93-216` |
| Leader epochs are derived from each node's local view, so two writers can mint the same epoch for different leaders. | `wal_metadata_store.rs:1509-1513` |
| Replicated events — including every anti-entropy repeat — are appended to the follower's metadata WAL, which is never compacted and fully replayed at boot. | `wal_metadata_store.rs:933-956`, `metadata_wal.rs` |
| Replicated changes do not wake the replica fetcher; it notices on its 10 s tick. | `builder.rs:1989`, `fetcher.rs:236` |

Around 1,950 production lines and ~1,200 test lines exist only to reconcile nodes that disagree
(provenance rules, timestamp adoption, stale-epoch guard, partition-count ratchet, config-key merge,
anti-entropy, pruning, "impoverished leader" adoption).

## 2. Goals

- **G1 One writer.** Every change to cluster shape is decided by one node, in one order.
- **G2 Convergence by replay.** A node that was down catches up by reading the log from where it
  stopped. No anti-entropy, no pruning heuristics.
- **G3 No guessing.** A node that is behind knows it is behind and never treats "I have not seen
  it" as "it does not exist".
- **G4 Safe handover.** A new catalog leader always holds every committed change.
- **G5 No hot-path cost.** Produce and fetch read the catalog from local memory, lock-free, as today.
- **G6 Single-node stays trivial.** No network round trips, no extra latency.
- **G7 Less code.** The reconciliation machinery in section 1 is deleted, not kept as a fallback.

## 3. Non-goals

- **Upgrade compatibility.** No customers; clusters are recreated. No migration, no mixed-version
  support. (An offline import tool can be added later if a cluster's data must be kept.)
- High-frequency state stays where it is: records, high watermarks, log start offsets, segment and
  Parquet records, consumer group offsets, transaction state.

## 4. The catalog (what moves)

| State | Notes |
|---|---|
| Topics: name, **stable id** (minted once by the leader), config, partition count, provenance | |
| Partition assignments: replicas, leader, leader epoch, ISR | ISR changes are the only frequent writer; batched |
| Brokers / membership | |
| ACLs, SCRAM credentials | fixes the replication hole by construction |

## 5. Shared design rules (both options)

1. **Commands, not events.** Writers submit intents (`CreateTopic`, `DeleteTopic`,
   `AlterTopicConfig`, `CreatePartitions`, `SetAssignments`, `ElectLeaders`, `UpdateIsr`,
   `Acl*`, `Scram*`, `Broker*`). The leader validates each command against the state that results
   from every earlier command and records the outcome. "Create X if absent" is decided once, in
   order — two concurrent creates cannot both succeed.
2. **Validation is deterministic and happens in order.** Results (accepted/rejected + reason) are
   returned to the caller.
3. **Batching.** A command may carry many partitions (`ElectLeaders` for 10,000 partitions is one
   entry). ISR updates are batched per partition leader per publish interval (today 5 s).
4. **Epochs are owned by the log.** Leader epochs are derived when a command is applied, from the
   agreed state, not from a node's private view. `UpdateIsr` carries the sender's leader epoch and
   is rejected if it is stale (fencing).
5. **Forwarding.** Any node receiving a catalog-changing request forwards it to the catalog leader
   and answers the client only after the change is committed **and applied locally**
   (read-your-writes on the node the client talks to).
6. **Auto-create** is a `CreateTopic{provenance: auto}` command forwarded like any other. Until the
   topic is applied locally, the node answers that topic with `LEADER_NOT_AVAILABLE`, which every
   Kafka client retries.
7. **Caught-up gate.** A node that is booting, or knows it is behind the leader, answers unknown
   topics with `LEADER_NOT_AVAILABLE` and never auto-creates locally.
8. **One read model.** The applied catalog lives in memory behind lock-free reads (the hot-path
   getters in `wal_metadata_store.rs:1550-1582` keep their signatures). Apply also emits local change
   notifications so the replica fetcher re-plans immediately (fixes the 10 s lag).
9. **Snapshots.** The applied catalog can be serialized at a log position; a node that is too far
   behind installs a snapshot then reads the tail. For Thunderbird-size clusters (~5,600 topics,
   ~17,000 partitions) a snapshot is a few MB.
10. **Interface boundary.** A new `ClusterCatalog` component owns the catalog. `MetadataStore`
    keeps its trait; catalog methods route to `ClusterCatalog`, local high-frequency events stay in
    `WalMetadataStore` and its local metadata WAL.

## 6. What gets deleted (both options)

From the mapping on 2026-11-14 (file:line in the review notes):

- `metadata_wal_replication.rs` (460), the event-bus adapter and `init_metadata_replication`
  in `builder.rs` (~375), metadata push and catalog-resync paths in `wal_replication.rs` (~190),
  catalog RPCs in `raft_cluster.rs` (~110), reconciliation in `wal_metadata_store.rs` (~590),
  follower broker self-registration (~20). ≈ 1,950 lines.
- Already-dead code found during mapping: `replication/record_processor.rs` (367),
  `run_raft_cluster` (~187), `ensure_default_topic` (~20).
- Their tests (~1,200 lines), replaced by the acceptance suite below.

Also fixed alongside, because both designs depend on it:
- Duplicate partition assignment on CreateTopics (assigned twice with two schemes,
  `handler.rs:5958` then `produce_handler.rs:3946`).
- **To verify first:** the replica fetcher connects without SASL (`connection.rs:51, 423-463`),
  so follower replication may be refused when `CHRONIK_SASL_ENABLED=true`.

## 7. Acceptance suite (both branches must pass, same scripts)

Run on the local 3-node cluster (`tests/cluster/`; branch B on the `altport-*` configs so both can
run at once). Every check measures behaviour through real Kafka clients and `/admin/status` on
**each** node, never one node's view.

| # | Scenario | Pass |
|---|---|---|
| T1 | Auto-create crossover: explicit `--partitions 1` via node 1 while producers on nodes 2/3 auto-create, with metadata delivery to one node artificially delayed | Exactly one version of each topic on all nodes; every acked record consumable by a group consumer |
| T2 | 1,000 concurrent creates of 200 distinct names from all nodes | Each name created once; losers get `TOPIC_ALREADY_EXISTS`; identical catalogs (including topic ids) on all nodes |
| T3 | Create, delete, alter, ACL and SCRAM changes while node 3 is down; restart node 3 | Node 3 converges with no anti-entropy; a revoked SCRAM user is refused on node 3 |
| T4 | Kill the catalog leader mid-burst of creates | No acknowledged create lost; no unacknowledged create half-applied; new leader serves writes within election + recovery bound |
| T5 | Network partition 1 \| 2 (iptables) | Minority side refuses catalog writes; majority continues; heal converges |
| T6 | Full cluster restart with 5,000 topics | Identical catalogs after boot; boot time not worse than v2.16.5 |
| T7 | Leader failover for 10,000 partitions (kill a node) | All moved leaders committed and visible within 2 s of detection |
| T8 | Snapshot install: wipe node 3's data, restart | Node 3 rebuilds from snapshot + tail |
| T9 | Produce/fetch throughput A/B vs v2.16.5, bytes-landed, alternating run order | Within run-to-run noise (memory: −2.8% was not detectable; need ≥ 5 alternating runs) |
| T10 | Catalog op latency on a settled cluster | CreateTopic p99 < 50 ms; caught-up gate releases within 1 s of apply |
| T11 | Single-node mode | All functional checks pass; no catalog round trips |
| T12 | Existing workspace tests | `cargo test --workspace --lib --bins` and `--tests` green |

T1 needs a fault-injection hook (delay catalog delivery to one node). Both branches implement
the same hook name so the script is shared.
