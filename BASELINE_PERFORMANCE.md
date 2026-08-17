# Baseline Performance

**Measured 2026-08-13, throughput tables re-measured 2026-08-16** at 1024
producers with the io_uring WAL path disabled, on the branch where follower-pull
replication became the only data-replication mechanism.

> ### Everything measured before this date was deleted, not archived
>
> Every cluster figure this repo published was taken while replication was
> silently disabled: `acks=1` and `acks=all` replicated **nothing** until PR #29,
> so a "3-node cluster at acks=1" number described one node writing to its own
> disk. The single-node figures alongside them were valid when taken but came
> from v2.2.9/v2.2.10 in November 2025, on different hardware, several hundred
> commits ago.
>
> They are gone rather than annotated. A table of invalid numbers with a warning
> above it is worse than no table: the warning is read once and the numbers get
> cited forever. The tell was visible in the old data and nobody caught it —
> standalone `acks=all` was reported *faster* than `acks=1` (347,585 vs 309,590
> msg/s), which is only possible if `acks=all` was not waiting for anything.

---

## Hardware

These numbers come from **one developer machine**, not a server. Stated up front
because it is the single most important caveat: the 3-node shape runs three
brokers on this one box, sharing its disk, cores and loopback.

| | |
|---|---|
| CPU | AMD Ryzen 9 5900HX (8 cores / 16 threads) |
| Memory | 30 GB (≈20 GB in use by other work during the runs) |
| Storage | NVMe SSD |
| OS | Linux 6.11 |

Bare-metal numbers on the Dell cluster were re-measured 2026-08-16 — see
`BARE_METAL_PERFORMANCE.md`.

## Method

`chronik-bench`, 1024 concurrent producers, 256-byte messages, 30s measured after
a warmup, 3 partitions, no compression, **WAL profile left at its default**
(`low`, 2 ms). Rates below are the harness's summary figure: total messages over
the measured window, which excludes the warmup phase.

**Every figure is the median of three runs, each on a freshly started cluster.**
A single measurement is not reproducible on this box: three brokers, a load
generator and the page cache share one machine, and whatever ran before leaves
the disk busy. `acks=all` on the cluster measured 2,912, 2,970, 6,587 and 3,066
msg/s across four runs of the harness while the same configuration measured on
its own gave 5,475–6,409 four times running. The harness now tears each cluster
down, waits for the ports to close and the disk to drain, and reports a median —
`perf_matrix.sh` prints the individual samples next to it so the spread is
visible rather than implied.

30 seconds, not 10, for a reason. `acks=all` throughput used to fall during a
run — every fetch re-read the whole active WAL segment, so the cost grew with
the file — and a 10-second run reported a number that a 30-second run
did not reproduce. The read path is fixed and the two now agree, but the longer
window is what proves it.

Reproduce: `tests/cluster/perf_matrix.sh`.

`chronik-bench` measures **round-trip throughput at a fixed concurrency** — each
producer waits for its acknowledgement. It is not a maximum-batched-throughput
benchmark; for that, see the kcat figures at the bottom.

## Single node

No replication to do, so this is the ceiling of the local write path.

| acks | throughput | p99 | samples |
|---|---:|---:|---|
| 0 | **515,903 msg/s** (125.95 MB/s) | 8.07 ms | 515,903 · 393,683 · 519,468 |
| 1 | **230,071 msg/s** (56.17 MB/s) | 9.73 ms | 230,971 · 218,485 · 230,071 |
| all | **222,059 msg/s** (54.21 MB/s) | 7.13 ms | 225,230 · 222,059 · 217,377 |

`acks=1` and `acks=all` are within 4% here, and that is correct: with no
followers the in-sync set is the leader alone, so `acks=all` waits for its own
fsync and nothing more. The remaining **2.2× gap to `acks=0`** is that fsync.

An independent run at the same concurrency measured `acks=1` at 228,874 msg/s,
so these are reproducible across harnesses, not an artefact of one.

<details><summary>Superseded: the same table at 64 producers with io_uring on</summary>

| acks | throughput | p99 |
|---|---:|---:|
| 0 | 164,914 msg/s | 7.71 ms |
| 1 | 14,476 msg/s | 5.30 ms |
| all | 14,307 msg/s | 5.55 ms |

`acks=1` is understated **16×** there. Three causes, all established since: 64
producers under-loads the broker by roughly an order of magnitude; the io_uring
WAL path cost about a third of produce throughput; and these were taken while
this machine's kernel had fallen back to the HPET clocksource, where every
`clock_gettime` is a syscall costing 1,213 ns against 19.6 ns on TSC. The old
text called the `acks=0` gap "12×, and that is fsync" — it is 2.2×, and most of
what it was attributing to fsync was overhead.

</details>

## Three nodes, RF=3, min_insync_replicas=2

Follower-pull replication running; every record reaches all three nodes.

| acks | throughput | p99 | samples |
|---|---:|---:|---|
| 0 | **171,398 msg/s** (41.85 MB/s) | 38.17 ms | 171,149 · 172,276 · 171,398 |
| 1 | **85,948 msg/s** (20.98 MB/s) | 24.72 ms | **7,758** · 85,948 · 89,111 |
| all | **40,686 msg/s** (9.93 MB/s) | 112.06 ms | 41,434 · 40,686 · **6,277** |

⚠️ **Two of these samples are ten times below their neighbours and that is not
explained.** `acks=1` produced 7,758 once against 85,948 and 89,111; `acks=all`
produced 6,277 once against 41,434 and 40,686. A median of three absorbs a single
outlier, which is exactly why it is reported that way — but a 10× collapse on one
run in three is a reliability signal, not noise to be smoothed over, and it
appears only in the modes that wait for replication. The single-node rows and
`acks=0` show nothing like it. Candidate causes not yet separated: a node slow to
rejoin the in-sync set after the previous run's teardown, or leadership settling
after cluster formation (a returning ex-leader can wait on metadata
anti-entropy before it learns it was demoted). **This should be understood before the cluster numbers
are quoted as a floor.**

`acks=0` and `acks=1` cost roughly what the single-node shape costs, plus
contention from two extra brokers on the same disk.

`acks=all` is **2.1× slower than `acks=1`**, which is the cost of the follower
round trip: a producer cannot be acknowledged until a follower's *next* fetch
reports a position past the record, so each write pays a fetch plus the
follower's own fsync on top of the leader's.

It used to be **4–7× slower** and to decay during a run — 3,993 msg/s in the
first interval, 2,285 in the second. That was not `acks=all` degrading; every
fetch re-read and re-parsed the whole active WAL segment from byte zero, so the
cost grew with the file. That was replaced with a bounded tail cache and a
sparse offset index, and the sustained figure is now 40,686 msg/s — stable across
the run rather than a function of how long you look.

Single-record `acks=all` latency is 14 ms, level with `acks=1`'s 12 ms
(`tests/cluster/acks_all_latency.sh`).

## Batched throughput, for contrast

The same 3-node cluster measured with `kcat`, which batches thousands of records
per request instead of waiting per message (`tests/cluster/perf_replication.sh`,
200,000 × 100 B, median of three):

| acks | throughput |
|---|---:|
| 0 | 961,538 msg/s |
| 1 | 772,200 msg/s |
| all | 542,005 msg/s |

All three stored 200,001 records with zero client errors. These answer a
different question — how fast the broker ingests when the client batches — and
should never be compared against the round-trip table above.

That `acks=all` reaches 542K msg/s when batched while managing 40.7K when each
message waits for its own acknowledgement is not a contradiction: it is the
difference between amortising one replication round trip over thousands of
records and paying one per record. The broker's ingest was never the constraint.

## What is not measured here

- **Bare metal.** All of the above is one machine, with the client sharing it
  with all three brokers. `BARE_METAL_PERFORMANCE.md` has the Dell cluster
  measured on real hardware over a real network, with the client on a separate
  machine: the Dell cluster is SLOWER at every acks level than this one machine (42,804 vs
  85,948 at `acks=1`), because there the client crosses a 1 GbE link that
  `acks=1` saturates at 943 Mbit/s, while here everything shares loopback.
- **Consume throughput.** `chronik-bench -m consume` exists; these runs are
  produce-only.
- **Searchable / columnar / vector topics.** The old report measured a 33%
  cluster overhead for searchable topics; that figure is void with the rest and
  has not been re-taken.
