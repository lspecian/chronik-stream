# Option B — the cluster catalog as an internal replicated partition

Status: Not pursued; Option A (Raft) was chosen and shipped in v2.17.0. Drafted 2026-11-14. Shared problem, goals, rules and acceptance suite:
[CLUSTER_CATALOG_COMMON.md](CLUSTER_CATALOG_COMMON.md). Branch: `feat/catalog-partition`.

## 1. Idea

Catalog commands are records in one internal partition, `__cluster/0`, replicated to every
voter with the existing follower-pull machinery (v2.12.0). The partition's leader is the only
writer; followers fetch it in order and apply records to their in-memory catalog. Raft keeps
only what it does today: membership, and choosing who leads.

**The important caveat up front:** follower-pull alone does not give safe handover. Raft elects
a leader by looking at the Raft log, which knows nothing about `__cluster`. A newly elected Raft
leader may be missing committed catalog records. Option B therefore needs a **leader recovery
protocol** that we design and prove ourselves (section 5). That protocol is consensus logic —
it is the part of Raft that Option B re-implements.

## 2. Bootstrap rules (breaking the circular dependencies)

The normal replication path reads its plan from the catalog. `__cluster` cannot, because it
*is* the catalog. Fixed rules instead:

| Normal partitions | `__cluster/0` |
|---|---|
| Replicas and leader from catalog assignments (`fetcher.rs:634`) | Replicas = Raft voters; leader = Raft leader (from `raft_cluster`, never from the catalog) |
| Leader epoch derived in the metadata store | Leader epoch = Raft term at which the leader took over |
| ISR published every 5 s into the catalog | **No ISR.** Commit = majority of voters (below). Avoids the ISR-publisher feedback loop |
| Fetch handler validates topic/partition and leadership from the catalog (`fetch_handler.rs:659, 693`) | Special-cased: served only by the node that believes it is the `__cluster` leader at the requested term |
| acks quorum from assignment, silent fallback to 1 (`produce_handler.rs:2859`) | Not produced through the Kafka produce path at all; appended by the catalog writer |
| Segments, indexing, object store, rotation | WAL-resident only; compacted by snapshot (section 6). `__*` dirs are already skipped by rotation (`group_commit.rs:818`) |
| Failover by the failover controller | Moves with Raft leadership |

## 3. Commit rule

A record is **committed** when a majority of voters have it durably (leader's fsynced end plus
followers' fetch offsets, which already arrive with every fetch — `fetch_handler.rs:1144`).
Committed offset = the majority-th highest durable offset. Followers learn it from the fetch
response's high watermark and apply only up to it. This is quorum commit, not ISR commit; it is
what lets a minority partition refuse writes (T5).

## 4. Write path

1. Any node receives a catalog-changing request or internal writer fires.
2. If not the `__cluster` leader: forward over the Raft gRPC channel to the Raft leader (same
   forwarding as option A).
3. Leader serializes commands through one writer task: validate against its applied catalog
   (it is the only writer, so its catalog is authoritative once recovery finished), append the
   record to `__cluster/0`, wait for majority commit, apply, return the result.
4. Batching: the writer drains its queue into one record per append.

## 5. Leader recovery protocol (the hard part)

When node N becomes Raft leader at term T:

1. **Fence:** N stops accepting catalog writes. Followers refuse to fetch from any `__cluster`
   leader whose term < the highest term they have seen; leaders validate `current_leader_epoch`
   on `__cluster` fetches (today it is parsed but never checked — `docs/ROADMAP_REPLICATION.md:688`).
2. **Survey:** N asks every reachable voter for its `__cluster` last epoch and log end
   (OffsetForLeaderEpoch already exists, `kafka_handler.rs:2655`). N needs answers from a majority
   (it has one — it just won an election — but the voters that elected it must answer this too).
3. **Choose:** among the majority's answers, the replica with the highest (last epoch, log end) holds
   every committed record (any committed record is on a majority; two majorities intersect). If that
   is not N, N fetches the missing tail from it.
4. **Commit the new epoch:** N appends an `EpochStart{term: T}` record and waits for majority commit.
   Everything before it in N's log is now committed; divergent tails on other replicas are truncated
   by the existing reconcile path (`fetcher.rs:825-1216`) when they fetch.
5. Only then does N accept catalog writes and act as failover controller.

Correctness depends on steps 2–4 matching Raft's election restriction exactly, including the case
where the chosen replica dies mid-fetch and the case where N loses leadership mid-recovery. This
needs a written proof sketch and fault-injection tests (T4, T5) before the rest builds on it.

## 6. Snapshots and compaction

- Leader writes a `Snapshot{catalog, at_offset}` record periodically (by records or bytes), then
  advances `__cluster`'s log start offset past it once a majority has it.
- A follower whose position is below log start fetches from the latest snapshot record.
- Requires a log-start-offset path for an internal WAL-only partition (normal partitions get theirs
  from segments and `list_segments`, which reads the catalog — circular).

## 7. Read path and the caught-up gate

- Hot-path getters unchanged.
- Caught-up gate: a follower knows the committed offset from fetch responses; until it has applied
  up to the committed offset once after boot, and whenever its `__cluster` fetches fail for longer
  than the liveness window, unknown topics answer `LEADER_NOT_AVAILABLE`.

## 8. What it reuses vs what is new

| Reused (hardened on Thunderbird) | New |
|---|---|
| Kafka Fetch transport, follower position tracking, batch apply (`apply.rs`) | Fixed-rule plan for `__cluster` in the fetcher supervisor |
| WAL append/read/truncate (`chronik-wal`) | Majority commit tracking for one partition |
| OffsetForLeaderEpoch reconcile and truncation | Leader recovery protocol (section 5) and fencing |
| Leader epoch cache | Snapshot record + log start for a WAL-only partition |
| Raft leader election (membership only) | Writer task, command validation, forwarding |

Fetcher caveat from mapping: any plan change aborts and rebuilds **every** fetch task
(`fetcher.rs:441-447`). Catalog applies trigger plan changes, so this must become incremental, or
data replication churns on every catalog write. The `__cluster` fetch must also run on its own
task so it is never starved by data partitions.

## 9. Work breakdown (agent-sized)

| Step | Scope | Parallel with |
|---|---|---|
| B1 | Proof sketch + fault model for section 5; reviewed before B4 merges | B2, B3 |
| B2 | `__cluster` partition: WAL-only storage, fixed-rule fetch plan, dedicated fetch task, special-cased fetch serving with epoch validation | B3 |
| B3 | Catalog state machine + commands + snapshot serialization (identical in shape to option A step A4) | B2 |
| B4 | Majority commit tracking + leader recovery protocol + fencing | after B1, B2 |
| B5 | `ClusterCatalog` behind `MetadataStore`; writer task; forwarding; caught-up gate; notifications | after B3, B4 |
| B6 | Incremental fetcher re-planning | after B2 |
| B7 | Move every writer to commands | after B5 |
| B8 | Delete push replication, anti-entropy, reconciliation, dead code | after B7 |
| B9 | Acceptance suite T1–T12 | scripts in parallel; runs after B8 |

## 10. Risks and open questions

- **Section 5 is new consensus code we own.** It is smaller than Raft but it is exactly the class of
  logic where subtle bugs lose committed data. The rest of option B is low-risk reuse.
- Two notions of leadership (Raft leader, `__cluster` leader) are kept equal by construction, but
  there are windows (leader elected, recovery not finished) that every caller must handle.
- Replica fetcher has no SASL/TLS (`connection.rs:51`); a catalog carrying SCRAM credentials over
  it needs at least an authenticated channel when SASL is enabled.
- Open: should `__cluster` replicate to all voters (simple, RF = cluster size) or to a fixed 3/5
  (scales to large clusters)?
