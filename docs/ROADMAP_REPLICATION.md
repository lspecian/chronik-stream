# Replication Roadmap — Follower-Pull Replication

**Goal**: Replace push-based fire-and-forget WAL replication with Kafka-style follower-pull, so that progress tracking, catch-up, backpressure and retention interlock become **properties of the design** rather than four separate mechanisms that can each be half-built.

**Status values**: `NOT STARTED` → `IN PROGRESS` → `CODE COMPLETE` → `TESTED` → `COMPLETE`

| Phase | Name | Status | Version | Notes |
|-------|------|--------|---------|-------|
| RP-0 | Replication conformance suite | `TESTED` | — | Placement + ISR honesty; fails pre-#29, passes after |
| RP-1 | Harden the current mechanism | `TESTED` | — | 1.1–1.4 + 3 bugs found by cluster validation |
| RP-2 | Follower fetch | `TESTED` | — | 2.1–2.4 all validated on a 3-node cluster, behind `CHRONIK_REPLICATION_MODE=pull` |
| RP-3 | Leader epochs & truncation | `TESTED (partly)` | — | Handshake proven end-to-end on a cluster; the **truncate** branch still needs manufactured divergence |
| RP-5 | Partition leader failover | `TESTED` | — | Built and validated: leadership moves, RF preserved, ISR shrinks, writes recover |
| RP-6 | Failover recovery latency | `NOT STARTED` | — | A returning replica waits on metadata anti-entropy (up to 5 min) to learn it is no longer leader |
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

#### The suite itself lied twice (found during RP-2.4)

Both faults made a **healthy broker look broken** — on the one test whose job is to tell those apart. Recorded because a guardrail that cries wolf is worse than none.

1. **Shell payloads did not survive `REPL_KUBECTL`.** When it is an ssh wrapper — the usage the header documents — ssh flattens its arguments and the remote login shell re-parses them, so `kubectl exec pod -- bash -c "a | b"` arrives as `bash -c a` with `| b` running on the *ssh host*. Every produce was a no-op and the suite reported "no partitions on any node". The quoting level is now **probed** at startup against output that cannot occur by accident, and the suite aborts loudly if no level works.

2. **The ISR assertion never took the replica down.** The operator recreates a deleted pod in ~4s and the broker resumes fetching well before the pod reports Ready — so `0/1 Running` reads as an outage while the replica is fully caught up. The liveness window is 30s and keeping a replica through a 4s blip is *correct*. Delete-once, delete-in-a-loop and `SIGSTOP` on PID 1 all failed to hold it down, each looking exactly like "ISR is over-reporting". Cordoning the node the replica runs on works: the pod goes Pending and stays there.

   Under push this test passed by accident — the leader had to re-establish its own outbound connection before the follower looked alive, stretching the outage past the window. Pull recovers on the follower's schedule instead. **Better behaviour silently invalidated the test**, which is a failure mode worth watching for in the rest of this roadmap.

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

- [x] Track the minimum offset any follower has acknowledged, per partition
- [x] `delete_after_index` must not delete a WAL segment above that offset
- [x] Metric + warning when WAL retention is held back by a lagging follower
- [x] Test: lagging follower prevents deletion; caught-up follower permits it

**Status**: `CODE COMPLETE`. `ReplicationProgress` trait in chronik-storage keeps the indexer ignorant of ISR/ACKs; impl lives on `IsrTracker`. Followers silent past `max_lag_ms` are excluded so a dead node cannot pin WAL forever (matches Kafka). No progress source = no interlock, so single-node is unchanged. 2 unit tests.

> Postgres's replication-slot equivalent. Required under **every** option including doing nothing — today WAL is deleted on indexing with no regard for whether followers received it.

### RP-1.2: Honest ISR (#31)

- [x] Leader maintains a *running* acked offset per follower — already existed (`IsrTracker`, fed from the ACK reader). It read empty only because nothing was being replicated
- [x] ISR = replicas within a lag bound, with a timeout for silent followers
- [x] `/admin/status` reports the real ISR
- [x] Under-replicated signal — `total_dropped()` accessor (RP-1.4)
- [ ] Metadata responses report the real ISR (admin only so far)

**Status**: `CODE COMPLETE`. Three defects, all making ISR read healthier than reality:

1. **Empty ISR reported as "all replicas in-sync".** `/admin/status` fell back to the assignment whenever the tracker returned nothing, so a partition replicating to *nobody* showed a full ISR. That inversion is why #29 stayed invisible for nine months. The fallback now applies only when the tracker has heard nothing at all for the partition (`is_unknown_for_all`) — genuinely a fresh cluster.
2. **Caught-up followers aged out.** The time bound was applied unconditionally, so every replica of an *idle* partition dropped out of ISR after `max_lag_ms` despite holding exactly the leader's data. It now measures how long a replica has been *behind*, matching `replica.lag.time.max.ms`.
3. **Backwards clock ejected healthy replicas.** `now - last_update` on u64 wraps rather than panicking in release, turning an NTP step into a colossal apparent lag. Saturating subtraction.

Added `SyncState{InSync,Lagging,Unknown}` so "never heard from" is distinguishable from "known behind" — the distinction defect 1 turned on. 5 unit tests.

### RP-1.3: `acks=all` waits (#30)

- [x] Verify/repair `quorum_size`
- [x] Change the guard to `acks == 1` so `acks=-1` reaches its ISR quorum arm
- [x] Timeout returns `NOT_ENOUGH_REPLICAS` rather than hanging
- [x] Test: `acks=-1` response stays pending until a follower ACK arrives
- [ ] Measure the latency cost — this moves `acks=-1` off the async fast path

**Status**: `CODE COMPLETE`. The quorum arm was fully written and **unreachable since v2.2.10** — `use_async_responses = response_pipeline.is_some() && acks != 0` sent `acks=-1` down the fast path, which answers on the leader's own fsync. Now `acks == 1`, so `acks=-1` falls through and waits. Only viable because #29 made followers actually receive data; before that this would have hung to timeout on every request.

`quorum_size` was `assignment.replicas.len()` — *every* assigned replica — so one slow follower blocked all writes until the 30s timeout even at RF=3/minISR=2. Now `min_insync_replicas` (leader's own ACK counted), matching Kafka. Corrects my earlier reading that it was unsatisfiable: the leader does self-ACK, so it *completed*, it was just far stricter than intended.

Test asserts the produce stays outstanding while only the leader has ACKed, then completes on a follower ACK. **Confirmed it fails with the old guard restored.**

### RP-1.4: Retry on send failure (#33)

- [x] Re-queue data records that reached no follower, with bounded retries and backoff
- [x] Surface `total_dropped` as a metric

**Status**: `CODE COMPLETE`. Only metadata was re-queued; a data record reaching no follower was discarded silently, so a transient follower restart left a permanent under-replicated gap. Data records now retry with the same 100ms backoff, bounded at `MAX_REPLICATION_ATTEMPTS` (300 ≈ 30s) so a dead follower cannot stall the queue. The attempt counter is `#[serde(skip)]` and never enters the frame, so the wire format is unchanged. 2 unit tests.

### RP-1 cluster validation — three bugs the unit tests could not have found

Every one surfaced only by running the conformance suite against a real 3-node cluster. All three share a root cause worth carrying into RP-2: **the push model has no reliable signal that a follower is alive.**

1. **ACK offset was in the wrong unit.** Followers ACKed the batch's *base* offset while ISR lag is measured against the leader's high watermark — an LEO. A follower that had written a batch in full still looked behind by the batch size, so it never counted as caught up. Followers now ACK `base_offset + record_count`, and the leader registers quorum waits on `last_offset + 1`. Both sides had to move together or quorum would never match.

2. **A replica that died while caught up never left ISR.** Fixing (1) — plus not ageing out caught-up replicas — meant a dead node stayed in-sync forever: the lag bound does not fire for a caught-up replica and it never ACKs again. Observed: a node killed for 60s still reported `isr=[1,2,3]`. Followers now answer heartbeats with a liveness ACK, carried on the existing ACK frame with an empty topic.

   ⚠️ The first attempt at this used **connection state** and did nothing on a real cluster: a TCP write succeeds into the local send buffer long after the peer is gone. Application-level liveness is the only thing that works here.

3. **A restarted follower was never reconnected — replication to it stopped permanently.** Same root cause: the stale connection stayed in `connections`, so the reconnect loop's `contains_key` check passed and never redialled. Newly created topics landed only on partitions that node led, with nothing reporting a problem. Connections to followers that stop answering heartbeats are now retired so the existing reconnect path fires. Pruning runs on the connection-manager loop, not beside the heartbeat send — heartbeats only fire when the queue is empty and any successful send resets their timer, so with one live and one dead follower under load they would never fire.

**Known limitation carried to RP-2**: `/admin/status` answered by a *non-leader* reports the assignment, not real ISR — only the leader receives ACKs. RP-2 fixes this structurally, since the leader learns each follower's position from its fetches.

---

## Phase RP-2: Follower Fetch

**Expected impact**: catch-up, backpressure and progress tracking become structural
**Effort**: 2-3 weeks
**Risk**: medium — changes the HW computation, which affects consumer visibility
**Depends on**: RP-0 passing on push

### RP-2.1: Recognise follower fetches

- [x] Branch on `replica_id >= 0` in the fetch path
- [x] Record the follower position — its fetch offset IS its LEO
- [ ] Followers fetch above the high watermark; consumers remain capped at HW (needs RP-2.3)
- [x] Test: follower fetch records progress, consumer fetch does not

**Status**: `TESTED` on a 3-node cluster. Wired in cluster mode only, so single-node is untouched. The fetch doubles as a liveness signal, which under push needed a separate heartbeat-ACK mechanism — and that mechanism had two bugs only a live cluster exposed.

### RP-2.2: Per-follower LEO tracking

- [x] Leader records each follower's fetch offset as that follower's LEO
- [x] Feeds the ISR computation from RP-1.2 (the same `IsrTracker`, now fed from fetches as well as ACKs)
- [x] Expose per-follower lag in `/admin/status`

**Status**: `TESTED` on a 3-node cluster — with one replica killed, the leader reported `isr=[1,2]`, `under_replicated=true`, `replica_lag=[{node_id:2,lag:0}]`. `/admin/status` now carries `replica_lag` (`node_id:lag` per follower) and `under_replicated` per partition.

`under_replicated` is the field worth alerting on, and it is exactly what should have been firing during the nine months `acks!=0` replicated nothing. `replica_lag` makes that alert actionable by naming the replica and the distance, instead of leaving an operator to exec into pods and list WAL directories — which is how the original outage actually had to be found.

Still leader-only: followers report to the leader, so a non-leader returns an empty list rather than a misleading zero. RP-2.4 removes the asymmetry.

### Bugs RP-2.4 surfaced outside replication

Three defects reached from this work that were not replication bugs at all, and would have hit any user:

| Bug | Effect | Fix |
|---|---|---|
| `max_wait_ms` applied **per partition** | `handle_fetch` serves partitions serially and gave each the full budget, so an idle N-partition fetch took N × `max_wait_ms`. At Kafka's default 500ms an 8-partition consumer waited 4s for an empty response, past its own timeout. A partition with data sat behind every idle one ahead of it. | One deadline per request; the waiting path shares it, the data path keeps its own read timeout |
| `IsrAckTracker` never reaped | Unbounded growth, one entry per unsatisfied `acks=all` produce | Reaper started by the builder |
| A dead replica reported `lag: 0` | Its last offset is frozen where it died, so the subtraction says "caught up". Printed beside `under_replicated: true`, it reads as a false alarm | Report no lag for a replica outside the liveness window |

The conformance suite itself had **two** faults that made a healthy broker look broken — see the RP-0 section.

### ✅ Was a blocker: followers did not reliably know who leads (FIXED 2026-08-12)

Pull moves a dependency that push never had. Under push the *leader* drives everything, so only the leader's metadata has to be right. Under pull the **follower** must know which node leads each partition in order to fetch from it — and today, after a restart, it frequently does not.

Measured on the 3-node pull cluster, same moment, same 69 partitions:

| Node | Partitions with a known leader |
|---|---|
| 1 | 66 of 69 |
| 2 | **0 of 69** |
| 3 | 21 of 69 |

Node 2 held no partition assignments at all, so it planned nothing and **replicated nothing**, while node 1's `/admin/status` reported `isr:[1,2,3]` for those partitions from its own healthy view. That is this roadmap's founding bug reproduced exactly, by a different route.

This is a **pre-existing metadata replication defect**, not a fault in RP-2 — see the `bug-metadata-recovery-diverges-at-scale` note (a `broadcast::channel(1000)` dropping `TopicCreated`, with an uncommitted buffer fix). Push masked it. Pull cannot.

Why the conformance suite still passes: it creates topics and produces immediately, and leadership for a freshly created topic propagates at creation. The divergence appears for topics that predate a restart.

**Root cause**: the catalog anti-entropy loop re-broadcast `TopicCreated` and nothing else. A follower healed into a state where it knew every topic and not one partition assignment — the exact state `admin_api.rs` already had a comment describing. The assignment is what carries the partition leader.

**Fix**: `broadcast_all_topics` now re-broadcasts partition assignments too, after the topics they belong to. Note the event-bus buffer must now be sized against `topics * (1 + partitions_per_topic)`, not topic count.

**Verified on a 3-node pull cluster**, all three nodes restarted simultaneously:

| | node 1 | node 2 | node 3 |
|---|---|---|---|
| immediately after restart | 3/3 | **0/0** | 9/9 |
| after the first anti-entropy pass (~42s) | 12/12 | **12/12** | 12/12 |

Converged and stable for 4+ minutes. Fetch coverage then matched leadership exactly — node 1 led 4 partitions, every fetch request carried 4, all 4 reported follower lag. Conformance suite passes after the restart.

**Healing takes up to one anti-entropy period** (first pass 45s, then `CHRONIK_METADATA_REBROADCAST_SECS`, default 300s). A follower that restarts mid-cycle replicates nothing until the next pass. Acceptable for now; if it matters, trigger a re-broadcast when a follower connects.

**A follower that plans nothing also says so loudly now** (`warn_if_replicating_nothing`) — silence is how this cost nine months the first time. That is observability, not the fix.

Also open, and related: `/admin/status` falls back to reporting the assignment as ISR when the tracker knows nothing about a partition (`is_unknown_for_all`). Under push an idle partition legitimately never reports, so the fallback is defensible. Under pull, followers fetch continuously and silence is genuinely suspicious — **when RP-4 deletes push, that fallback should become "under-replicated", not "healthy"**.

### RP-2.3: High watermark from ISR

- [x] `HW = min(LEO across ISR)` — was the **leader's own write position** from `ProduceHandler`
- [x] Consumers observe only up to HW; followers still read to the leader's LEO
- [x] `acks=all` completes when the quorum has reached the batch's offset
- [ ] Test: HW does not advance while a follower is behind

**Status**: `TESTED` on a 3-node cluster under pull. Both halves are done: `acks=all` settles off follower fetch offsets, and consumers are capped at `min(LEO across ISR)`.

The `acks=all` half forced a latent bug into the open. `IsrAckTracker` matched an **exact** `(topic, partition, offset)`, which only worked because push emitted one ACK per pushed batch. A follower's fetch offset is a watermark that skips across many batch boundaries and rarely lands on a registered offset, so under pull every `acks=all` produce would have waited out the full 30s timeout. Replica progress is monotonic — a replica reporting N holds everything below N — so waits are now released by any report at or above their offset. That is strictly more correct under push too: a follower demonstrably at 500 satisfies a wait at 437 even if the ACK for 437 was lost or coalesced.

The same rewrite closed a memory leak: `cleanup_expired()` had **no caller anywhere in the tree**. A wait that never reached quorum was never removed — the producer's own `timeout()` released the caller but left the registration behind. While `acks!=0` replicated nothing (#22), that was every `acks=all` produce the broker ever served. Same shape as the v2.10.8 produce-reservation leak.

> `HW = min(LEO)` remains the single riskiest change in the roadmap: it alters what consumers can see. Needs its own soak before RP-4.

> The single riskiest change in the roadmap: it alters what consumers can see. Needs its own soak before RP-4.

### RP-2.4: Follower fetch loop

- [x] Background task per **leader** (not per partition) issuing Fetch to the partition leader
- [x] Long-poll so steady-state streaming needs no extra round trip
- [x] Append fetched records to the local WAL (shared apply path with the push receiver)
- [x] On restart, resume from local LEO → **catch-up, for free**
- [ ] Test: follower down 5 minutes, restarted, converges without operator action

**Status**: `TESTED` on a 3-node cluster, behind `CHRONIK_REPLICATION_MODE=pull` (default remains `push`). Conformance suite passes in **both** modes: placement correct at acks=0/1/all with 300/300 consumed, and ISR shrinks honestly when a replica is held down.

**Shape.** One fetch task per *leader*, not per partition — a follower batches every partition it replicates from the same leader into one long-polled Fetch, as Kafka's `ReplicaFetcherThread` does. Three leaders means three in-flight requests regardless of partition count.

**The Kafka client had to be hand-rolled.** `chronik-server` deliberately excludes `rdkafka` (librdkafka does not cross-compile against musl without a vendored zlib/OpenSSL). `crates/chronik-server/src/replication/replica_fetcher/protocol.rs` encodes Fetch requests and decodes Fetch responses at **v11** — the highest non-flexible version, so no varints or tagged fields, and it still carries `current_leader_epoch` (v9) for RP-3. The risk in hand-rolling a codec is drift from the server it talks to, so its tests round-trip against the server's own `parse_fetch_request` / `encode_fetch_response` and assert the server consumes every byte.

**The apply path refuses rather than guesses.** `plan_batches` is pure and classifies each fetched batch against the follower's LEO: duplicate (the leader answers with whole batches from the one *containing* the requested offset, so re-receipt is routine), gap, straddle, or partial tail (the leader cuts at a byte budget — flow control, not corruption). A refusal aborts before any append, so a blob lands whole or not at all. A straddle halts that partition, because resolving it needs RP-3's epoch history and the alternative is interleaving two histories in one log.

**Ordering held: RP-2.4 preceded RP-2.3.** `HW = min(LEO across ISR)` is only safe once followers actually fetch.

**What it deletes.** The three mechanisms RP-1 had to fix — ACK channel for progress, heartbeat replies for liveness, connection pruning for restart detection — are all redundant under pull. A fetch offset is progress, liveness and resume position in one value.

---

## Phase RP-3: Leader Epochs & Truncation

**Expected impact**: removes silent log divergence on leader change
**Effort**: 2-3 weeks
**Risk**: high — this is where the subtle bugs live
**Depends on**: RP-2

Kafka needed KIP-101, then KIP-279 and KIP-320 to close this. Do not treat it as a detail.

Scaffolding that already exists: `partition_leader_epoch` is in the RecordBatch wire format (`records.rs:83`, currently written as `-1`), and `OffsetForLeaderEpoch` is enumerated as API 23 with an advertised version range (`kafka_protocol.rs:135`, `parser.rs:759`). No protocol extension needed; the semantics are unimplemented.

### RP-3.1: Populate leader epoch

- [x] Leader stamps the current epoch into `partition_leader_epoch` on append
- [x] Epoch increments on leader change, persisted in metadata
- [x] Epoch→start-offset history retained per partition

**Status**: `CODE COMPLETE` — unit-tested, not yet exercised on a cluster.

**The epoch lives on `PartitionAssignment`**, so it is already durable (metadata WAL), already replicated, and already re-broadcast by the anti-entropy loop. `assign_partition` derives it rather than accepting it from callers: there are a dozen construction sites across the tree, and each would be a chance to skip the bump or reuse a value.

The case that matters most is the one that must *not* bump — re-asserting the same leader. The anti-entropy loop rewrites every assignment every few minutes; if that manufactured a leadership change, every follower would conclude it had to truncate, repeatedly, on a healthy cluster.

**The history is derived from the log**, not stored separately: each batch carries the epoch of the leader that wrote it, so every replica builds the same history by watching its own appends. That is what makes the truncation exchange a single request.

**Stamping is CRC-safe.** Kafka's CRC-32C starts at ATTRIBUTES (offset 21); `partition_leader_epoch` is at offset 12, before the CRC field and outside its input — deliberately, so a broker can assign it without re-checksumming. That is tested against a real encoded batch rather than asserted in a comment. ⚠️ A stale comment in `produce_handler.rs` claimed the CRC started at `partition_leader_epoch`; it was corrected in place, since RP-3 depends on the opposite being true.

### RP-3.2: Implement `OffsetForLeaderEpoch`

- [x] Serve API 23: given an epoch, return its last offset
- [x] Follower queries on leader change to find the divergence point (RP-3.3)

**Status**: `CODE COMPLETE` — v0 only, matching the advertised range. Advertising more than is implemented hands clients malformed frames, so the two move together.

An epoch the node cannot speak to — newer than anything it holds, or aged out — is answered `-1`, never a plausible-looking offset. A guess there makes a follower discard a correct log or keep a divergent one, which is the exact damage epochs exist to prevent.

The current epoch is answered with the leader's **log end offset**, not RP-2.3's replicated watermark: a follower may read that far, and capping it would deadlock replication.

### RP-3.3: Truncation on leader change

- [x] Follower detects a leader/epoch change and asks the new leader where its epoch ended
- [x] Follower truncates its log to that point before resuming fetch
- [x] WAL suffix truncation primitive (the gate below — it did not exist)
- [x] Conformance test written (RP-0.4)
- [x] Handshake proven end-to-end on a 3-node cluster
- [ ] The **truncate** branch specifically — needs manufactured divergence (see below)

**Status**: `TESTED (partly)`. The exchange runs on real hardware; the branch that actually deletes data has not been triggered because nothing has yet produced a divergent log.

Verified on a cluster, all three steps of the chain, from one follower restart:

```
Leader-epoch history rebuilt for 1 partition(s), 1 leadership transition(s), in 254µs
OffsetForLeaderEpoch hshake-0: epoch 0 ends at 100 (log end 100)      ← leader answered
hshake-0: log is a prefix of the leader's (ours ends at 100,
          the epoch ran to 100) — nothing to truncate                 ← follower decided
```

and again across five partitions at once on a replica returning from a failover.

#### Two ordering bugs that made this inert, both silent

1. **The epoch warm-up ran after the replica fetcher started.** `warm_up_leader_epochs` was called after all 17 builder stages; the fetcher starts at stage 16. Measured: the fetcher logged `Replicating 1 partition(s)` **553µs before** the warm-up finished, so its first reconcile read an empty epoch store, found nothing to ask about, skipped the handshake and cleared its reconcile flag. The history then arrived too late to matter, and a returning replica would never truncate. The warm-up now runs before stage 16.
2. **The warm-up enumerated partitions from `list_topics()`**, which races catalog recovery at startup — a topic not yet in the catalog was skipped even though its log was on disk. It now enumerates from the WAL directory, which is what it rebuilds from. And it logs unconditionally: it previously printed only when it warmed something, so "warmed nothing" and "never ran" were the same observation.

#### What is still untested: the cut itself

Every cluster run so far ends in `log is a prefix — nothing to truncate`, which is the **correct** outcome for the failure being staged. Killing a leader does not produce divergence when writes are acknowledged by the full ISR: the survivors already hold everything the dead node had.

Real divergence needs the leader to accept writes its followers never received, then lose the election — i.e. a **network partition**, not a node kill: partition the leader from its followers, produce with `acks=1` so it acknowledges alone, then let the others elect a new leader and heal the partition. That is the test still owed. Until it runs, the truncation branch has 12 integration tests and 15 unit tests behind it and no hardware.

#### The storage half: `WalManager::truncate_to`

The gate was that **no suffix truncation existed anywhere in the storage layer**. `truncate_before` is a documented no-op; `delete_records_before` removes segments *below* a low watermark, the opposite operation. Now built, split so the decisions are testable without a filesystem:

- **`chronik-wal/src/truncate.rs`** plans over bytes. The rule: *no record containing an offset at or above the target survives*. A target landing inside a batch therefore takes that whole batch, and the log ends **below** what was asked for — callers resume from the returned offset, not their target. Erring low costs a re-fetch; erring high keeps a divergent log and calls it converged.
- **`GroupCommitWal::truncate_to`** does the surgery, because it owns the writer and can stop it first: hold the partition's pending and file locks (the same two `commit_batch` takes, in the same order), drop buffered writes, cut, then rotate to a fresh segment so the survivor stays immutable and no deleted segment id is reused.

Placement reads only each segment's **first** record — ids ascend with write order so offsets do too, so at most one segment straddles the target and needs scanning. Scanning every segment would be linear in the size of the log, and DV-2c's was 43 GB.

**Two bugs the tests caught, both silent:**

1. The straddling segment is the last one *below* the target, not the first one at or above it — that is the segment *after*. Truncating a 3-segment log to offset 5 deleted segments 1 and 2 and left offsets 5–9 alive in segment 0.
2. The delete loop unlinked zero-length segments, which is normally the one the writer holds open. Writes would have kept succeeding and fsyncing into an orphaned inode — accepted, durable, readable by nobody.

**The in-flight race.** `commit_batch` drains the pending queue and *then* takes the writer lock, so a batch can be off the queue but not yet on disk when a truncation runs. Writing it afterwards restores exactly the records that were just discarded. A truncation epoch — read at drain, re-read under the writer lock — discards those batches and fails their callers. The test pins the commit worker in that window by holding the writer lock itself, and fails without the guard.

**`update_high_watermark` could not do this.** It is deliberately monotonic (the v2.2.9 fix, stopping stale WAL data from walking a watermark backwards), so it silently ignores the one update truncation needs. `reset_offsets_after_truncation` is the narrow exception, and moves `next_offset` too — unused on a follower, but if that replica is later elected it assigns offsets from there, and a stale value leaves a hole.

#### The protocol half: `reconcile_with_leader`

A follower runs the epoch handshake **before its first fetch from a leader** and again whenever it sees divergence. The first case matters most: the fetch task is rebuilt whenever assignments change, which includes a leader change — the one moment a follower can hold records the new leader never committed.

`plan_reconciliation` is pure, because it is the decision that deletes data. Every branch that *does not* truncate is as load-bearing as the one that does: an error code, or the `-1` meaning "I cannot answer", must never be read as an offset. Reconciliation failure is not swallowed — the loop **does not fetch** until it succeeds, since an unreconciled fetch is how divergence becomes permanent.

Divergence now re-triggers the handshake instead of halting the partition, and `OFFSET_OUT_OF_RANGE` does too: retention having moved past us and a real divergence look identical from the fetch offset alone, and the epoch query is what distinguishes them.

#### ⚠️ Known hazard: truncation is local to the WAL

`WalIndexer` uploads sealed segments to the object store on **followers as well as leaders** — only the metadata registration is leader-gated. If a follower's divergent tail was already published, truncating the local WAL does not retract it: re-indexing the shortened segment writes a *different* `{min}-{max}` key rather than replacing the old one, so the divergent object survives.

Latent rather than active today, because object-store use is opt-in (`CHRONIK_COLUMNAR_USE_OBJECT_STORE`, default off, local-first). **Before that default changes**, either gate raw-segment upload on leadership, or hold a follower's uploads until the records are known committed. Recorded rather than fixed here: it is an indexer change, not a truncation change, and guessing at it would widen a destructive commit.

#### Open: the leader does not fence stale epochs

The Fetch request carries `current_leader_epoch`, and this broker *parses* it — but never validates it. So the follower leaves it `-1`: populating it would cost a metadata read per fetch and fence nothing. Divergence is caught after the fact by the handshake instead of being refused up front. Real fencing needs the leader to reject stale epochs first; the follower side is then one line.

#### ✅ Done: history survives restart

The epoch cache is rebuilt from the WAL at startup (`warm_up_leader_epochs`), replaying each partition through the same `observe_append` the live path uses — so a recovered node answers truncation queries identically to the one that wrote the log. A pre-RP-3 log (all `-1`) rebuilds to *no* history rather than a fabricated epoch at offset 0.

Cost is a WAL scan per partition at startup. Kafka avoids it with a `leader-epoch-checkpoint` file; that is the answer if startup time becomes a problem.

⚠️ **Test methodology**: by RP-2's lesson, killing the leader must genuinely keep it down — deleting a pod brings it back in ~4s. Cordon the node. And beware the inverse trap RP-2 hit: better behaviour can silently invalidate a test that used to pass for the wrong reason.

---

## Phase RP-5: Partition leader failover — `TESTED` (built 2026-08-12)

**Status**: `TESTED` on a 3-node cluster. Built because it blocked RP-3.3's validation, and because it was a live correctness gap in its own right.

### Result

Same probe that previously showed leadership frozen for 150 seconds, against the new build:

```
initial: "leader":1,"replicas":[1,2,3],"isr":[1,2,3]
  t=30s  "leader":1,"replicas":[1,2,3],"isr":[1,2,3]
  t=45s  "leader":2,"replicas":[1,2,3],"isr":[2,3]     ← failover
produce (acks=all, leader down): 0 errors  (was 63)
```

Leadership moves to a live replica in ~45s (one liveness window plus a tick), the replica set stays RF=3, ISR shrinks honestly to the live members, and writes resume. In the conformance suite the same thing shows as `leader after: node 2` with `distinct consumed 400 / acknowledged 400` — **no acknowledged record lost across a failover**.

### How it works

- **Liveness comes from Raft** (`RaftCluster::sample_active_peers`). The data path cannot supply it: followers report to their leader, so a leader's death is exactly the case with nobody left to observe it — which is also why ISR never shrank before. Raft heartbeats run between all members regardless of who leads what. Kafka's controller tracks broker liveness the same way, and for the same reason.
- **Only the Raft leader acts**, and not until a full liveness window after being elected. Two nodes electing independently would hand one partition to two leaders — the divergence RP-3.3 exists to clean up after. A newly elected leader has heard from nobody yet, so without the grace period its first pass would fail every partition in the cluster over at once.
- **`plan_failover` is pure**, because it decides where writes go. A healthy cluster must plan *nothing*: every failover bumps a leader epoch, and an epoch bump sends followers into the RP-3.3 handshake, so churn here would leave the cluster permanently reconciling. A test asserts the plan converges after one pass.
- **Persisted through `assign_partition`**, which derives the epoch — so a real leader change bumps it exactly once. That bump is what makes RP-3.3 reachable at all.

### Three bugs found by running it, not by writing it

1. **The liveness input was a constant.** `recent_active` is set when a peer replies and cleared only by `check_quorum_active`, which Raft calls only when `check_quorum` is enabled — and this cluster leaves it at the default `false`. Nothing ever cleared the flag, so every peer that had ever been seen read alive forever, *including after it died*. The controller deployed clean and did nothing. The sampler now owns the reset, which leaves consensus behaviour untouched; enabling `check_quorum` instead would make a leader step down whenever it missed a quorum of replies for one election timeout, and this config is already tuned around election storms.
2. **Failover shrank the replica set.** Writing the *live* replicas back as the assignment meant a partition returned from a transient failure at RF=2, and the returning node was no longer a replica at all — so it never resumed replicating and never ran the handshake. Repeat the failure and RF reaches 1 with nothing reporting it. Failover moves leadership; it is not a reassignment. ISR is what shrinks, and `IsrTracker` already does that.
3. **It was silent.** Nothing logged what the controller believed, so a no-op and a healthy cluster were the same observation. It now logs the live set on change.

4. **A returning node undid the failover, and every read went to zero.** The worst result of this effort, and it is a *metadata* bug that only working failover could expose.

   Measured by the conformance suite: `distinct consumed 0 / acknowledged 400`. Node 1 led partition 0; it was held down; the partition failed over to node 2; node 2 accepted 400 acknowledged records. Node 1 came back, replayed its own metadata WAL — stale by exactly the change that demoted it — and its `PartitionAssigned` event overwrote the newer one **on every node**. All three then agreed the leader was node 1, which held no data for that partition. Consumers routed to the leader read nothing. The records were intact on node 2 the whole time; metadata had simply pointed away from them.

   The apply path was a blind `insert` — last writer wins regardless of age. Nothing in the metadata model expressed that one assignment supersedes another.

   Leader epochs already express exactly that: monotonic per partition, derived in one place. They are now the causality token — an assignment carrying an older epoch is ignored. Equal epochs still apply, because anti-entropy re-asserts unchanged assignments constantly and a node that missed the original event must still be healable by a re-broadcast.

   ⚠️ **This is the generalisable lesson of the phase.** Every replicated piece of state needs a version that says which of two copies is newer. Partition assignments had one (`leader_epoch`) and were not using it. Worth auditing the other replicated metadata — topic configs in particular, given the partition-count disagreement recorded below — for the same shape.

### What was removed

Both stubs that reported success while doing nothing: `elect_leader_from_isr` (the module is now an honest shim; the push stack still wires the type, RP-4 deletes both) and `propose_set_partition_leader` (no callers).

### The original measurement

### What was measured

On a 3-node cluster, a topic's partition leader was held down by cordoning its node (the RP-0.3 method — deleting the pod brings it back in ~4s). Observed from a **surviving** node, for 150 seconds:

```
initial: "leader":1,"replicas":[1,2,3],"isr":[1,2,3],"under_replicated":false
  t=15s  "leader":1,"replicas":[1,2,3],"isr":[1,2,3],"under_replicated":false
  ...
  t=150s "leader":1,"replicas":[1,2,3],"isr":[1,2,3],"under_replicated":false
produce (acks=all, 60s cap): 64 connection errors, zero records written
```

The leader never moves, the dead node stays in ISR, the partition reports `under_replicated: false`, and **writes to it are unavailable until that exact node returns**.

**Reproduced identically under `CHRONIK_REPLICATION_MODE=push` on the same build**, so this is not a pull regression — it is pre-existing and affects both mechanisms.

### Why: the election is a stub that re-elects the dead node

The machinery *runs*. `WalReceiver::monitor_timeouts` fired and the worker logged:

```
WARN  Triggering leader election for pfail2-…-0: WAL stream timeout (30s)
INFO  ✅ Elected new leader for pfail2-…-0: node 1 (reason: WAL stream timeout (30s))
```

Node 1 was the node that was down. `LeaderElector::elect_leader_from_isr` (`leader_election.rs`):

1. **ignores ISR**, despite the name — its own comment says *"For now, treat all replicas as in-sync (ISR = replicas). Proper ISR tracking will be added later"*;
2. returns `replicas[0]`, which is by construction the incumbent leader, so the "new" leader is always the old one;
3. never consults liveness, so a dead replica is as electable as a live one;
4. **never persists the result** — the Raft proposal is commented out with *"let the system self-heal via produce requests"*;
5. logs `✅ Elected new leader` regardless.

This is the founding bug of this roadmap in a different costume: a mechanism that reports success while doing nothing. `docs`/`CLAUDE.md` advertise "automatic leader election" and "fault tolerance (can lose minority of nodes)" — true for *Raft/metadata* leadership, which does fail over, and false for *partition* leadership.

### Why RP-3 makes this tractable

The pieces RP-3 added are what a correct election needs:

- `assign_partition` already derives the leader epoch and **bumps it when the leader changes** (and deliberately does not when it is re-asserted, so anti-entropy cannot cause spurious truncations);
- a bumped epoch propagates to followers, which re-plan and run the RP-3.3 handshake;
- `IsrTracker::get_follower_lag` already returns `None` for a node that is not alive (RP-2.1), which is the liveness signal the election lacks.

So the shape of the fix is: elect a **live** replica other than the failed leader, persist it through `assign_partition`, and let the epoch bump do the rest. The hard part is not the selection — it is that **nothing currently tracks the liveness of a partition's leader**. Followers ACK to the leader, so a dead leader has no one to evict it; that is also why ISR stayed `[1,2,3]` above. Leader liveness has to come from somewhere else (Raft membership is the obvious candidate).

⚠️ Do not fix by having each node elect independently. The `am_i_leader()` Raft guard is already there and is correct — a split election would hand two nodes the same partition, which is precisely the divergence RP-3.3 exists to clean up after.

### Other findings from the same run (not replication bugs, not chased)

- **A subscribing consumer can read a partition twice.** One conformance run consumed 490 records of 300 produced at acks=1; a later topic held 400 readable records for 200 produced. Reading the same topic with an explicit `--partition` returns **exactly** the right count, and the WAL holds one copy — so the log is correct and the duplication is in the subscribe/consumer-group path, most likely a rebalance re-reading from the beginning. Intermittent: five consecutive direct reproductions were clean. This is issue #36, now with a sharper characterisation.

- **A topic's partition count can disagree between nodes — and this probably *causes* the above.** A topic created with `--partitions 1` was later reported by `/admin/status` as having three, with real assignments and leaders for partitions 1 and 2, while every record sat in partition 0 and only partition 0 existed on disk. The producer had clearly seen one partition when it wrote.

  That is a coherent explanation for #36: a consumer that subscribes gets partitions the producer never knew about, and a metadata refresh that changes the partition count mid-consume triggers a rebalance — which, with no committed offsets and `auto.offset.reset=earliest`, re-reads from the beginning. Duplicates without any duplicate on disk, intermittently, exactly as observed.

  **Mechanism found.** `TopicCreated`'s apply path ratchets the partition count upward and never down — its own comment says *"Only update when incoming > existing, never downgrade"*, added so an auto-create with **fewer** partitions could not overwrite an explicit `CreateTopics` with more. The rule is asymmetric, and it is wrong in the other direction: an auto-create carrying the default (`TopicConfig::default()` = 3, `traits.rs:40`) silently **expands** a topic explicitly created with 1. The same comment names the race that delivers it — *"auto_create_topics() on a follower can race with the real CreateTopics event from the leader"*.

  So any topic created with fewer partitions than the auto-create default can be quietly widened to the default, after which the producer's view (1) and the cluster's view (3) disagree permanently.

  Confirmed to need churn: a 1-partition topic created on a quiet cluster stayed at 1 on all three nodes across 60s, and no expansion was logged. Both observed expansions happened while the cluster was in flux from cordon/uncordon.

  **The fix is not "ratchet the other way".** Both directions are wrong because count is being used as a proxy for authority. What matters is whether the config came from an explicit `CreateTopics` or from auto-creation; that distinction needs to be carried on the event and to win regardless of count. This is the same shape as the assignment bug above — replicated state without a marker saying which copy is authoritative.
- The conformance suite asserts `consumed == produced` for acks=1. Without idempotence — which the Java producer silently disables when `acks=1` is set explicitly — duplicates are permitted by the protocol, so that assertion is stricter than the contract. The honest check is *distinct* count equals produced. Left as-is for now because the observed duplication is a broker-side artefact worth failing on, but the assertion should be split before it is trusted.
- **A broker that is healthy and serving can sit at `0/1 Running` indefinitely.** After a restart, one node served replica fetches normally, with no errors in its log, while never passing its readiness probe. Worth a look: readiness that disagrees with reality makes every k8s-level test ambiguous, which is how RP-0.3 wasted three attempts.

---

## Phase RP-6: Failover recovery latency (found 2026-08-12)

**Status**: `NOT STARTED`. Not a correctness bug — the cluster converges — but it makes failover recovery take minutes instead of seconds, and it is the reason RP-0.4 still fails.

A node that was leading a partition when it died comes back believing it is *still* the leader: it recovers metadata from its own WAL, which is stale by exactly the change that demoted it. `plan_assignments` skips partitions whose leader is this node, so it fetches nothing for them. It learns the truth only from the metadata anti-entropy re-broadcast — first pass at 45s, then `CHRONIK_METADATA_REBROADCAST_SECS`, default **300s**.

Measured: after a failover the returning replica held nothing for that topic, while a topic from an earlier run — with ~10 minutes to heal — had fully converged on all three nodes. So it heals; it just takes an anti-entropy period, and RP-0.4's 150s convergence window is not enough.

**The fix is a catch-up read, not a shorter timer.** A node starting up should ask the Raft leader for current assignments rather than trusting a local copy that is stale precisely when it matters most. Shortening the re-broadcast interval trades a constant broadcast cost against a window that would still exist.

⚠️ This interacts with RP-3.3: the returning node cannot run the truncation handshake for a partition it does not know it follows. So a divergent replica stays divergent — serving nothing, but also repairing nothing — for up to the anti-entropy period.

---

## Phase RP-4: Delete the Push Stack

**Scope, now that Open Question 2 is decided**: delete the **data** push path only. Metadata keeps the push transport, so `WalReceiver` and `wal_replication.rs` survive in reduced form rather than being removed.

Concrete targets:

| Target | Where | Note |
|---|---|---|
| `ProduceHandler::wal_replication_manager` + `set_wal_replication_manager` | `produce_handler.rs` (fields at ~463/1195/1248, use at ~2430/2594/3978) | The produce-path fan-out, including `serialized_for_replication` which exists only to feed it |
| The `else` branch building the data `WalReplicationManager` | `builder.rs` `wire_raft_dependencies` | Pull becomes unconditional |
| `ReplicationMode` | `replica_fetcher/fetcher.rs` | Enum, `from_env`, and every `is_pull()` gate (builder stages 15/16, `set_hw_from_isr`) |
| `LeaderElector` shim | `leader_election.rs` (72 lines) | Superseded by RP-5; delete with its wiring |
| Election trigger machinery | `wal_replication.rs` `run_election_worker`, `monitor_timeouts`, `last_heartbeat`; `replication/connection_state.rs` `setup_timeout_monitoring` | Fed only the elector. ⚠️ `last_heartbeat` is also used by `consumer_group.rs` and `leader_lease.rs` for unrelated purposes — do not follow the name blindly |

⚠️ **Removing `ReplicationMode` removes the escape hatch.** Push is currently still the default; every phase from RP-2 on was validated with `CHRONIK_REPLICATION_MODE=pull` explicitly set. Flip the default to pull and soak it *before* deleting the switch, so the two changes fail separately.



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
2. ~~**Does `__chronik_metadata` move to pull, or keep a minimal push transport?**~~ — **DECIDED 2026-08-12: keep the push transport for metadata; RP-4 deletes only the data path.**

   Metadata is not a partitioned topic with replicas and a leader epoch; it is a single Raft-managed log whose leadership is Raft's, and it is the store that *holds* the partition assignments the pull path reads. Moving it to pull would make the mechanism that discovers who leads a partition depend on already knowing who leads a partition. The push transport works correctly there today, including retry, and `MetadataWalReplicator` is built on it.

   This shrinks RP-4 from "delete `wal_replication.rs`" to "delete the data push path": `ProduceHandler::set_wal_replication_manager` and its produce-path use, the `else` branch in `wire_raft_dependencies` that builds the data `WalReplicationManager`, the `ReplicationMode` switch (pull becomes unconditional), and the `LeaderElector` shim plus the election trigger machinery in `WalReceiver` that RP-5 superseded. `WalReceiver` itself stays, serving metadata.
3. **What lag bound defines ISR?** (blocks RP-1.2) — Kafka uses time (`replica.lag.time.max.ms`). Offset-based lag misbehaves with uneven partition rates.
4. **Does HW-from-ISR change observable consumer behaviour in existing tests?** (blocks RP-2.3) — consumers currently see the leader's write position; under Kafka semantics they would see less during follower lag.
5. **How does an existing cluster upgrade across the push→pull boundary?** (blocks release, not any phase) — a pull follower cannot replicate from a push leader, so a rolling upgrade breaks replication mid-roll.

   **Leaning: accept a full-cluster restart, and say so in the release notes.** Two reasons, both from this effort. First, the version being upgraded *from* did not replicate at all on `acks=1`/`acks=all` (PR #29), so there is no working replication to preserve across the roll — the "safe rolling upgrade" being protected is protecting a mechanism that was not running. Second, keeping the push receive path for one release means shipping the coexistence we rejected, in the release where the new path is least soaked, and every bug found in RP-1/RP-2/RP-5 was found by *removing* ambiguity about which mechanism was live.

   Not yet decided, because it depends on whether any deployment is running RF>1 on a version new enough to replicate correctly. **Confirm that before RP-4 lands.**

---

## Prior Art in This Repo

Read before starting; both are cautionary and specific.

- `archive/failed-raft-data-replication-v2.2-v2.3` — 12 commits, 2025-10-29. See `docs/CRITICAL_BUG_RAFT_BATCHING.md` on that branch for the root cause and the unfinished 1-2 hour fix
- #29 (`0b4e871`) — the async-return bug, its regression test, and the A/B methodology used to prove it
- `docs/DISTRIBUTED_QUERY_LAYER.md` — Known Limitation #5 documents the *same* "assignment ≠ reality" mistake in the vector fan-out path, found 2026-03-04 and fixed for vector only; the SQL twin (#22) survived five more months
