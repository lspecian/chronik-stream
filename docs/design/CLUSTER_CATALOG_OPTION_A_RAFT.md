# Option A — the cluster catalog in Raft

Status: Implemented in v2.17.0 (PR #71). Drafted 2026-11-14. Shared problem, goals, rules and acceptance suite:
[CLUSTER_CATALOG_COMMON.md](CLUSTER_CATALOG_COMMON.md). Branch: `feat/catalog-raft`.

## 1. Idea

Catalog commands become entries in the Raft log the cluster already runs for membership. Raft
orders them, replicates them, decides when they are committed, and guarantees that any elected
leader holds every committed entry. Each node applies committed entries, in order, to its
in-memory catalog.

**The Raft algorithm is not the work. The integration around it is.** The algorithm is TiKV
raft-rs (main @ `1fd05e0`, vendored in `crates/raft`, changed only for the prost codec plus one
safety change described below). The integration — storage, message loop, apply — is ours, and it
has the worst track record in this codebase (see section 6). Option A is therefore mostly a
rewrite of that integration, followed by moving the catalog onto it.

## 2. What exists today and must change

| Today | Problem | Required |
|---|---|---|
| Message loop runs on a fixed 100 ms `interval` (`raft_cluster.rs:2307`); proposals and incoming messages wait for the next tick | A commit costs ~2–3 tick boundaries (≈100–300 ms) — the "slow raft" of `cfb53bd` | Event-driven loop: wake on proposal, incoming message, or tick timer. Ticks stay at 100 ms for election timing only |
| `propose()` returns when queued (`raft_cluster.rs:886-946`) | Callers cannot know if or when a command committed, or whether it was accepted | Proposals return a future resolved with the command's **result** after apply. Track by proposal id carried in the entry; fail pending futures on leadership loss |
| Apply clones the whole state machine per batch (`raft_cluster.rs:1338`, `MetadataStateMachine::clone`) and logs every entry at INFO | O(catalog) per commit; log floods | Apply mutates in place; reads go through persistent (structurally shared) maps behind `ArcSwap`, or DashMap with a version counter. Per-entry logging at debug |
| Storage pads gaps with `Entry::default()` (`raft_storage_impl.rs:445-455`), keeps 50,000 entries in memory with blocking WAL fallback, truncates only the in-memory copy after a snapshot (`:722-760`) | Fabricated entries, memory/disk disagreement, blocking reads inside raft-rs calls | New storage: append-only segmented log file with an index; suffix truncation on conflict; prefix compaction on snapshot applied to disk **and** memory; no gap filling (a gap is a fatal error) |
| `crates/raft/src/raft_log.rs` `commit_to` clamps instead of panicking (added in `ba4139d`, "clamp instead of fatal on empty log") | Hides a node that lost acknowledged Raft state | Restore the upstream fatal check. Fix the real cause: a freshly bootstrapped node must start from a proper initial `ConfState`/snapshot rather than an empty log at index 0 |
| Raft shares the process with request handling; moved to its own runtime in `fc61c64` (#58) | Starvation history | Keep the dedicated runtime; add a metric for loop latency and apply lag |
| Snapshot trigger every +500 applied entries, state = bincode of `MetadataStateMachine` | Fine in shape | Snapshot = catalog serialization; trigger on entries or bytes; install path tested (T8) |

## 3. Catalog state machine

- Lives in the Raft apply path; one `apply(command) -> CommandResult` function, deterministic,
  with all validation inside it (rule 5.1/5.2 of the common doc). Because validation happens at
  apply time, two concurrent `CreateTopic(X)` proposals are both committed but only the first
  applied one succeeds — no read-index or leader lease needed for correctness.
- State: topics (with stable id minted from the entry's `(term, index)`), assignments with leader
  epochs, brokers, ACLs, SCRAM. Epoch derivation moves here from `wal_metadata_store.rs:1509`.
- Commands are versioned with an enum tag; no compatibility shims are needed (non-goal).

## 4. Write path

1. Any node receives a catalog-changing request (CreateTopics, DeleteTopics, CreatePartitions,
   AlterConfigs, ACL, SCRAM, auto-create, admin rebalance) or an internal writer fires
   (failover, rebalancer, ISR publisher).
2. If not Raft leader: forward over the existing Raft gRPC channel (`forward_write_to_leader`,
   `raft_cluster.rs:1893`, extended to carry catalog commands).
3. Leader proposes; batching window of ≤ 2 ms collects concurrent commands into one entry.
4. On commit, every node applies. The proposing node's future resolves with the result; the
   forwarding node waits until its own applied index ≥ that entry before answering the client.

Internal writers move to the leader or become forwarded commands:
- Failover and preferred-leader rebalance already run only on the Raft leader → `ElectLeaders`.
- The partition rebalancer currently runs on every node with no leader check → leader only.
- The ISR publisher runs on each partition leader → forwards batched `UpdateIsr{leader_epoch}`;
  stale epochs rejected.

## 5. Read path and the caught-up gate

- Hot-path getters unchanged, served from the applied catalog in memory.
- **Caught-up gate:** a follower knows the leader's commit index from Raft messages
  (`raft.raft_log.committed`). Until `applied ≥ committed` once after boot, and whenever it has
  heard nothing from a leader for an election timeout, unknown topics answer
  `LEADER_NOT_AVAILABLE`.

## 6. Why the integration is the risk (history from git)

| When | Event |
|---|---|
| Oct 2025 | Record data through Raft: 78,500 → 1,200 msg/s; branch archived |
| 31 Oct 2025 | All Raft code removed (`5dc93e7`) |
| 1–2 Nov 2025 | Re-added for metadata; 4 critical bugs incl. entries and HardState never persisted (`b22a4ad`) |
| 8–11 Nov 2025 | Split-brain fix, empty voters, election storms, message-loop deadlock |
| 12 Nov 2025 | Catalog moved off Raft (`cfb53bd`, "remove slow raft") |
| Feb 2026 | raft-rs vendored with the `commit_to` clamp (`ba4139d`) |
| Nov 2026 | Raft starvation on shared runtime (#58, `fc61c64`) |

Option A succeeds only if section 2 is done properly and proven by tests that deliberately crash
nodes between persist and apply (raft-rs `failpoints` feature is available in the vendored crate).

## 7. Work breakdown (agent-sized, in dependency order)

| Step | Scope | Parallel with |
|---|---|---|
| A1 | New Raft storage (segmented log, truncate, compact, snapshot save/load) + unit tests incl. crash-recovery | A2 |
| A2 | Event-driven message loop, proposal futures with results, leadership-loss failure of pending proposals | A1 |
| A3 | Restore `commit_to` fatal; fix bootstrap initial state; election config review | after A1 |
| A4 | Catalog state machine + commands + snapshot serialization + incremental apply | after A2 |
| A5 | `ClusterCatalog` behind `MetadataStore`; forwarding; caught-up gate; change notifications | after A4 |
| A6 | Move every writer (table in common doc §1) to commands | after A5 |
| A7 | Delete push replication, anti-entropy, reconciliation rules, dead code | after A6 |
| A8 | Acceptance suite T1–T12 | scripts in parallel from A1; runs after A7 |

## 8. Risks and open questions

- **Rewriting storage and the loop is the bulk of the work**, and it is the code that broke before.
- Membership changes (add/remove node) share the log with catalog commands; ConfChange handling in
  the loop (`raft_cluster.rs:2423-2520`) must survive the rewrite.
- Leadership of Raft, failover controller and catalog writer are the same node — simple, but a
  Raft election pauses all catalog writes (≈2 s election timeout today).
- Open: keep 100 ms ticks / 20-tick election, or shorten now that the loop is event-driven?
