# Replication Roadmap — Follower-Pull Replication

**Goal**: Replace push-based fire-and-forget WAL replication with Kafka-style follower-pull, so that progress tracking, catch-up, backpressure and retention interlock become **properties of the design** rather than four separate mechanisms that can each be half-built.

**Status values**: `NOT STARTED` → `IN PROGRESS` → `CODE COMPLETE` → `TESTED` → `COMPLETE`

| Phase | Name | Status | Version | Notes |
|-------|------|--------|---------|-------|
| RP-0 | Replication conformance suite | `IN PROGRESS` | — | RP-0.1 `TESTED` — fails on v2.10.10, passes post-#29 |
| RP-1 | Harden the current mechanism | `NOT STARTED` | — | Valuable standalone; independent of pull |
| RP-2 | Follower fetch | `NOT STARTED` | — | `replica_id`, per-follower LEO, `HW = min(LEO)` |
| RP-3 | Leader epochs & truncation | `NOT STARTED` | — | The hard part. Gated behind RP-0 |
| RP-4 | Delete the push stack | `NOT STARTED` | — | ~2,500 lines removed |

---

## Working Model

**This is a breaking change. All work happens on `feat/follower-pull-replication` and nothing releases until the whole thing is complete and tested.**

- **One long-lived branch.** Phases are checkpoints on that branch, not releases. No incremental merges to `main`, no intermediate tags.
- **Rebase onto `main` regularly** — weekly at minimum, and after any release. A stale long-lived branch is its own failure mode in this repo (`origin/feat/memory-hybrid-infra-and-quality` became an unusable pre-rebase snapshot exactly this way).
- **Escape hatch**: RP-0 and RP-1 touch the *existing* mechanism and don't depend on pull. If the effort stalls, they can be cherry-picked to `main` on their own and still leave the system better. That is a fallback, not the plan.
- **Definition of done for release**: every RP-0 test green (including the ones that start `#[ignore]`), a soak on a real 3-node cluster, and the perf numbers in `BASELINE_PERFORMANCE.md` / `BARE_METAL_PERFORMANCE.md` re-measured against the new mechanism.

### Breaking changes this ships

| Change | Consequence |
|---|---|
| Replication protocol replaced by Fetch | **No mixed-version cluster.** A pull follower cannot replicate from a push leader, so a rolling upgrade across the boundary does not work — see Open Question 5 |
| `HW = min(LEO across ISR)` | Consumers may observe *less* than today during follower lag. Today HW is the leader's own write position, which over-reports |
| WAL replication port 9291 retired | Config, CRD, operator and firewall rules all change |
| `acks=all` actually waits | Latency increases for `acks=-1` producers; some workloads will feel it |

---

## Why: what we actually measured (2026-08-10/11)

This roadmap exists because of findings that are empirical, not theoretical. Recorded here so no future session has to rediscover them.

| Finding | Evidence |
|---|---|
| `acks=1` and `acks=all` replicated **nothing**; `acks=0` replicated fine | Controlled A/B on a purpose-built 3-node RF=3 cluster: baseline `acks=1` placed each partition on exactly one node, fixed build placed all three on all nodes. `acks=0` placed all three in both |
| Not a recent regression — present in v2.2.25 | Ran `harbor.lab.specian.de/chronik/chronik-server:v2.2.25-fix-spinloop` (the image behind `BARE_METAL_PERFORMANCE.md`): `acks=all` → `[0]`,`[1]`,`[2]`; `acks=0` → `[0,1,2]` on all three |
| Live clusters ran 33 days with every partition on exactly one node | signal-stream v2.10.7, four topics, `ls /data/wal/<topic>` on each pod |
| ISR is fiction | `/admin/status` reported `replicas:[1,2,3], isr:[1,2,3]` for every partition with zero follower copies on disk. `wal_replication.rs`: *"For now, treat all replicas as in-sync (ISR = replicas)"* |
| No catch-up exists anywhere | The only `catch up` in the tree is metadata replication; every `backfill` is vector embeddings. Followers never pull, and nothing reconciles after the fact |

Fixed in **#29** (`0b4e871`): the async-response path returned before the replication hook, and that path is taken whenever `acks != 0`.

Still open: **#30** (`acks=all` doesn't wait), **#31** (ISR from assignment, not ACKs), **#33** (records dropped with no retry).

### Cost of replication, measured

3-node RF=3, `kafka-producer-perf-test`, 300k × 1KB, after warmup:

| Config | Throughput | p50 | p99 |
|---|---|---|---|
| `acks=1`, replication silently off (pre-#29) | 88,054 rec/s | 289 ms | 374 ms |
| `acks=1`, replicating (post-#29) | 66,181 rec/s | 357 ms | 514 ms |
| `acks=0`, replicating | ~30 rec/s ⚠️ | 14 ms | 91,554 ms |

**Unresolved**: whether 66K is network-bound or sender-bound. Links are 1 GbE and the load generator was co-located with a broker. **Settle this before optimising anything** — see Open Questions.

---

## Architectural Principle

> **A follower is a consumer with a `replica_id`.**

The single most important property of this design, for *this* codebase: replication rides the **same code path as consumer reads**. Every consumer test, every real client, and the 228-test protocol conformance suite then exercise the path replication depends on.

That matters because of how the last two attempts died:

| Attempt | Failure |
|---|---|
| Raft data replication (2025-10, `archive/failed-raft-data-replication-v2.2-v2.3`) | Batch notifier wired for `__raft_internal-0`, **not** for data partitions → user topics fell back to per-message Raft. Measured 61 msg/s. Root cause documented in `docs/CRITICAL_BUG_RAFT_BATCHING.md` on that branch, fix estimated at **1-2 hours**, never done, branch abandoned |
| Push WAL replication (v2.2.9 → v2.11.0) | Replication hook unreachable for `acks != 0`; metadata replicated via a *separate* path (`broadcast_metadata`) and so kept working. Undetected for ~9 months |

**Both are the same bug class: the mechanism was wired for the internal/metadata topic and silently not for user data.** A third mechanism inherits that hazard on day one unless the data path is the path everything else already uses.

⚠️ **Note on the Raft verdict**: the "Raft is terrible (2-5K msg/s)" comment in `produce_handler.rs` quotes a broken implementation. The **270x** figure in `docs/BENCHMARK_RESULTS_v2_3_0.md` was *projected, never measured* — that document's real numbers (31,876 / 54,857 msg/s) are **standalone mode**, no replication. This does not mean we should return to Raft; it means the prior experiment should carry no weight in the decision either way.

---

## Measurement Discipline

**Correctness (primary)** — every phase must keep these green:

- Bytes land on followers: for each of `acks=0`, `acks=1`, `acks=-1`, produce to an RF=3 topic and assert every replica physically holds every partition
- Consumed record count equals produced count, no duplicates
- Reported ISR matches physical reality
- A follower restarted mid-produce rejoins and converges

**Performance (watch for regression)**:

- Produce throughput and p50/p95/p99 at `acks=1` and `acks=-1`
- Replication lag: leader LEO minus slowest follower LEO
- Consumer fetch latency — RP-2 puts followers on the same path, so consumer reads must not regress
- NIC utilisation per node (the currently-unexplained variable)

**Negative result — do not repeat** (2026-08-10): coalescing replication frames into one write+flush per follower per batch (256 records) **regressed** `acks=1` from 66,181 → 19,641 rec/s with p99 514 ms → 10.5 s, and left `acks=0` replication partial. Zero drops, zero write failures, zero queue overflow — so it was not failure handling. Suspected head-of-line blocking on ~4 MB writes, never confirmed. The premise (single sender is the bottleneck) was never validated. Branch deleted.

---

## Phase RP-0: Replication Conformance Suite

**Expected impact**: makes every later phase verifiable; would have caught both prior failures
**Effort**: 2-3 days
**Risk**: none — additive tests only
**Depends on**: nothing
**Written against push, must pass on the current mechanism before RP-2 begins.**

### RP-0.1: Placement assertions

- [x] Test helper: produce N records to an RF=3 topic at a given acks level, return per-node partition placement
- [x] Assert every replica physically holds every partition, for `acks=0`, `acks=1`, `acks=-1`
- [x] Assert consumed count equals produced count (acks=0 exempt — fire-and-forget has no delivery guarantee)
- [x] Run against a real 3-node cluster, not mocks — the bug class is a *wiring* bug and mocks would have passed
- [x] `local` mode (`tests/cluster/`, kcat) and `k8s` mode (`REPL_MODE=k8s`, kubectl injectable so no lab host is hardcoded)

**Status**: `TESTED`. `tests/cluster/regression_replication.sh`. Validated in **both** directions, which is the only way to know a regression test is real:

| Cluster | acks=0 | acks=1 | acks=all | Result |
|---|---|---|---|---|
| chronik-thunderbird v2.10.10 (pre-#29) | `[0 1 2]` on all 3 | `[0]`,`[1]`,`[2]` | `[0]`,`[1]`,`[2]` | **FAIL** (correctly) |
| post-#29 build | `[0 1 2]` on all 3 | `[0 1 2]` on all 3 | `[0 1 2]` on all 3 | **PASS** |

Asserts against the *union* of partitions across nodes rather than a fixed `0..N`, so it stays honest however the client's partitioner distributed records.

⚠️ Found while building this: `kafka-topics.sh --describe` fails against Chronik — `non-nullable field clusterId was serialized as null`. DescribeCluster returns a null cluster id that the Java AdminClient refuses to deserialize, breaking standard Kafka tooling. Filed separately; the RF assertion here is best-effort as a result, and physical placement carries the test.

### RP-0.2: Unit-level guards

- [ ] Port `test_replication_fires_for_every_acks_mode` (already on main from #29) into the suite
- [ ] Assert ISR reported by `/admin/status` matches physical placement
- [ ] Assert a produce that reaches no follower is counted in `total_dropped`, never `total_sent`

**Status**: —

### RP-0.3: Failure-mode coverage

- [ ] Follower down during produce → records land after it returns (currently **fails**: no catch-up. Mark `#[ignore]` with a link to RP-2 until then)
- [ ] Follower restarted mid-produce → converges
- [ ] Leader killed mid-produce → no divergence after election (currently **fails**: no leader epochs. `#[ignore]` until RP-3)

**Status**: —

> Deliberately includes tests that fail today. They define the target and un-ignore as phases land.

---

## Phase RP-1: Harden the Current Mechanism

**Expected impact**: closes #30, #31, and half of #33 on the existing push transport
**Effort**: 3-5 days
**Risk**: low — additive, no transport change
**Depends on**: RP-0.1 (so regressions are visible)

Worth doing **regardless of whether pull ever happens**.

### RP-1.1: Retention interlock

- [ ] Track the minimum offset any follower has acknowledged, per partition
- [ ] `delete_after_index` must not delete a WAL segment above that offset
- [ ] Metric + warning when WAL retention is held back by a lagging follower
- [ ] Test: lagging follower prevents deletion; caught-up follower permits it

**Status**: —

> Postgres's replication-slot equivalent. Required under **every** option including doing nothing — today WAL is deleted on indexing with no regard for whether followers received it.

### RP-1.2: Honest ISR (#31)

- [ ] Leader maintains a *running* acked offset per follower (today's ACKs are one-shot waiters, not a tracked position)
- [ ] ISR = replicas within a lag bound, with a timeout for silent followers
- [ ] `/admin/status` and Metadata responses report the real ISR
- [ ] Under-replicated-partition metric, so this is alertable

**Status**: —

### RP-1.3: `acks=all` waits (#30)

- [ ] Verify/repair `quorum_size` — currently `assignment.replicas.len()` (**all** replicas, not a majority); at RF=3 that may demand 3 ACKs from 2 followers and never complete
- [ ] Change the guard `use_async_responses = response_pipeline.is_some() && acks != 0` → `acks == 1` so `acks=-1` reaches its (already written, currently unreachable) ISR quorum arm
- [ ] Timeout returns `NOT_ENOUGH_REPLICAS` rather than hanging
- [ ] Test: `acks=-1` response stays pending until a follower ACK arrives
- [ ] Measure the latency cost — this moves `acks=-1` off the async fast path

**Status**: —

### RP-1.4: Retry on send failure (#33)

- [ ] Re-queue data records that reached no follower, with bounded retries and backoff (today only metadata is re-queued; data is dropped silently)
- [ ] Surface `total_dropped` as a metric

**Status**: —

---

## Phase RP-2: Follower Fetch

**Expected impact**: catch-up, backpressure and progress tracking become structural
**Effort**: 2-3 weeks
**Risk**: medium — changes the HW computation, which affects consumer visibility
**Depends on**: RP-0 passing on push

### RP-2.1: Recognise follower fetches

- [ ] Branch on `replica_id >= 0` in the fetch path — **the field is already decoded and discarded today** (`fetch_types.rs:136`, `handler.rs:1301`, `kafka_handler.rs:1191`)
- [ ] Followers fetch above the high watermark; consumers remain capped at HW
- [ ] Test: a follower fetch and a consumer fetch at the same offset return different visibility

**Status**: —

### RP-2.2: Per-follower LEO tracking

- [ ] Leader records each follower's fetch offset as that follower's LEO
- [ ] Feeds the ISR computation from RP-1.2 (replacing the ACK-derived position)
- [ ] Expose per-follower lag in `/admin/status`

**Status**: —

### RP-2.3: High watermark from ISR

- [ ] `HW = min(LEO across ISR)` — today HW is the **leader's own write position** from `ProduceHandler` (`fetch_handler.rs:371`)
- [ ] Consumers observe only up to HW
- [ ] `acks=all` completes when HW passes the batch (supersedes RP-1.3's ack-wait)
- [ ] Test: HW does not advance while a follower is behind

**Status**: —

> The single riskiest change in the roadmap: it alters what consumers can see. Needs its own soak before RP-4.

### RP-2.4: Follower fetch loop

- [ ] Background task per replicated partition issuing Fetch to the partition leader
- [ ] Long-poll so steady-state streaming needs no extra round trip
- [ ] Append fetched records to the local WAL (reusing today's follower write path)
- [ ] On restart, resume from local LEO → **catch-up, for free**
- [ ] Test: follower down 5 minutes, restarted, converges without operator action

**Status**: —

---

## Phase RP-3: Leader Epochs & Truncation

**Expected impact**: removes silent log divergence on leader change
**Effort**: 2-3 weeks
**Risk**: high — this is where the subtle bugs live
**Depends on**: RP-2

Kafka needed KIP-101, then KIP-279 and KIP-320 to close this. Do not treat it as a detail.

Scaffolding that already exists: `partition_leader_epoch` is in the RecordBatch wire format (`records.rs:83`, currently written as `-1`), and `OffsetForLeaderEpoch` is enumerated as API 23 with an advertised version range (`kafka_protocol.rs:135`, `parser.rs:759`). No protocol extension needed; the semantics are unimplemented.

### RP-3.1: Populate leader epoch

- [ ] Leader stamps the current epoch into `partition_leader_epoch` on append
- [ ] Epoch increments on leader election, persisted in metadata
- [ ] Epoch→start-offset history retained per partition

**Status**: —

### RP-3.2: Implement `OffsetForLeaderEpoch`

- [ ] Serve API 23: given an epoch, return its last offset
- [ ] Follower queries on leader change to find the divergence point

**Status**: —

### RP-3.3: Truncation on leader change

- [ ] Follower truncates its log to the divergence point before resuming fetch
- [ ] Test: leader killed mid-produce, new leader elected, follower with extra records truncates and converges (un-ignores RP-0.3)

**Status**: —

---

## Phase RP-4: Delete the Push Stack

**Expected impact**: ~2,500 lines removed, one replication mechanism instead of two
**Effort**: 2-3 days
**Risk**: low once RP-2/RP-3 are soaked
**Depends on**: RP-3 `TESTED`, plus a soak on a real cluster

- [ ] Remove `WalReplicationManager`, `WalReceiver`, custom frame format, heartbeats, reconnect/backoff, ACK frames, `partition_followers` discovery
- [ ] Retire WAL replication port 9291 from configs, CRD and operator
- [ ] Keep the metadata path working — `broadcast_metadata` currently rides the same transport; either port `__chronik_metadata` to pull as well, or keep a minimal transport solely for it (**decide in RP-2**)
- [ ] Update `docs/DISTRIBUTED_QUERY_LAYER.md`, `CLAUDE.md`, and cluster docs

**Status**: —

> No push/pull coexistence flag. There is no external consumer of the replication protocol, so rollback is an image redeploy, not a config toggle.

---

## Open Questions

Answer before the phase that depends on them.

1. **Is today's 66K rec/s network-bound or sender-bound?** (blocks any perf claim, and RP-1.4/RP-2 sizing) — 1 GbE links, and the load generator was co-located with a broker. Re-run `acks=1` with the client off broker nodes while sampling per-node NIC utilisation. If we are at ~110 MB/s, the transport is not the constraint and no sender work is justified.
2. **Does `__chronik_metadata` move to pull, or keep a minimal push transport?** (blocks RP-4) — it is the one topic where push currently works correctly, including retry.
3. **What lag bound defines ISR?** (blocks RP-1.2) — Kafka uses time (`replica.lag.time.max.ms`). Offset-based lag misbehaves with uneven partition rates.
4. **Does HW-from-ISR change observable consumer behaviour in existing tests?** (blocks RP-2.3) — consumers currently see the leader's write position; under Kafka semantics they would see less during follower lag.
5. **How does an existing cluster upgrade across the push→pull boundary?** (blocks release, not any phase) — a pull follower cannot replicate from a push leader, so a rolling upgrade breaks replication mid-roll. Options: accept a full-cluster restart in the release notes; or keep the push *receive* path for one release so new leaders can still feed old followers. The second reintroduces coexistence, which we rejected for the steady state — but a bounded upgrade window is a different question from a permanent flag. **Decide before RP-4 deletes the receive path.**

---

## Prior Art in This Repo

Read before starting; both are cautionary and specific.

- `archive/failed-raft-data-replication-v2.2-v2.3` — 12 commits, 2025-10-29. See `docs/CRITICAL_BUG_RAFT_BATCHING.md` on that branch for the root cause and the unfinished 1-2 hour fix
- #29 (`0b4e871`) — the async-return bug, its regression test, and the A/B methodology used to prove it
- `docs/DISTRIBUTED_QUERY_LAYER.md` — Known Limitation #5 documents the *same* "assignment ≠ reality" mistake in the vector fan-out path, found 2026-03-04 and fixed for vector only; the SQL twin (#22) survived five more months
