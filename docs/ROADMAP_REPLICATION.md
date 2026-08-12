# Replication Roadmap — Follower-Pull Replication

**Goal**: Replace push-based fire-and-forget WAL replication with Kafka-style follower-pull, so that progress tracking, catch-up, backpressure and retention interlock become **properties of the design** rather than four separate mechanisms that can each be half-built.

**Status values**: `NOT STARTED` → `IN PROGRESS` → `CODE COMPLETE` → `TESTED` → `COMPLETE`

| Phase | Name | Status | Version | Notes |
|-------|------|--------|---------|-------|
| RP-0 | Replication conformance suite | `TESTED` | — | Placement + ISR honesty; fails pre-#29, passes after |
| RP-1 | Harden the current mechanism | `TESTED` | — | 1.1–1.4 + 3 bugs found by cluster validation |
| RP-2 | Follower fetch | `TESTED` | — | 2.1–2.4 all validated on a 3-node cluster, behind `CHRONIK_REPLICATION_MODE=pull` |
| RP-3 | Leader epochs & truncation | `IN PROGRESS` | — | 3.1+3.2 code complete & unit-tested; 3.3 (truncation) remains |
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
- [ ] Follower queries on leader change to find the divergence point (RP-3.3)

**Status**: `CODE COMPLETE` — v0 only, matching the advertised range. Advertising more than is implemented hands clients malformed frames, so the two move together.

An epoch the node cannot speak to — newer than anything it holds, or aged out — is answered `-1`, never a plausible-looking offset. A guess there makes a follower discard a correct log or keep a divergent one, which is the exact damage epochs exist to prevent.

The current epoch is answered with the leader's **log end offset**, not RP-2.3's replicated watermark: a follower may read that far, and capping it would deadlock replication.

### RP-3.3: Truncation on leader change

- [ ] Follower detects a leader/epoch change and asks the new leader where its epoch ended
- [ ] Follower truncates its log to that point before resuming fetch
- [ ] Test: leader killed mid-produce, new leader elected, follower with extra records truncates and converges (un-ignores RP-0.3)

**Status**: `NOT STARTED` — the remaining core of RP-3, and the only phase where a bug **destroys** data rather than stalling it.

What exists to build on: the follower's Fetch client already speaks v11, which carries `current_leader_epoch`; `LeaderEpochStore` answers "what is my latest epoch?" and supports `truncate_from_end`; and the leader now answers API 23.

#### ⛔ The gate: the WAL cannot truncate its tail

**There is no suffix truncation anywhere in the storage layer.** `WalManager::truncate_before` is a documented **no-op** (`manager.rs`, "v1.3.53+: No-op, GroupCommitWal manages truncation internally via rotation"), and `delete_records_before` removes whole segments *below* a low watermark — front truncation, the opposite operation. Nothing can discard records at and above an offset, which is the entire physical act RP-3.3 exists to perform.

So the protocol exchange is the easy half. The hard half is a storage primitive that does not exist.

**It is implementable.** WAL records are self-delimiting — `magic(2) version(1) flags(1) length(4) crc(4)` then body, and the read path already advances a cursor by the parsed size — so the byte offset where a given record begins is recoverable by a forward scan. Sketch:

1. delete whole segment files whose lowest offset is `>= N`;
2. in the segment straddling `N`, scan forward to the first record with `base_offset >= N` and `set_len()` the file at that byte;
3. reset the partition's in-memory position (`next_offset`, high watermark) to `N`;
4. `LeaderEpochCache::truncate_from_end(N)`.

**The complication is `GroupCommitWal`.** It owns the write path, buffers records before flushing, and manages the active segment and rotation. Truncating files underneath it would race in-flight writes and leave its in-memory position disagreeing with what is on disk. Truncation therefore has to go *through* the writer — quiesce the partition, discard its buffered batches, truncate, reset position — not around it.

This is a genuine storage change on a destructive path, and should be budgeted and reviewed as one rather than treated as wiring.

#### Also missing

Populating the epoch cache during **WAL recovery**: it is currently built only from live appends, so a restarted follower begins with no history and would answer every `OffsetForLeaderEpoch` with UNDEFINED. Kafka keeps a `leader-epoch-checkpoint` file for exactly this; rebuilding by scanning the WAL on startup is the cheaper first step, at the cost of a startup scan.

⚠️ **Test methodology**: by RP-2's lesson, killing the leader must genuinely keep it down — deleting a pod brings it back in ~4s. Cordon the node. And beware the inverse trap RP-2 hit: better behaviour can silently invalidate a test that used to pass for the wrong reason.

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
