# Baseline Performance

**Measured 2026-08-13** on the branch where follower-pull replication became the only data-replication mechanism.

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

Bare-metal numbers on the Dell cluster are **not yet re-measured** — see
`BARE_METAL_PERFORMANCE.md`.

## Method

`chronik-bench`, 64 concurrent producers, 256-byte messages, 30s measured after
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
the file (see RP-9) — and a 10-second run reported a number that a 30-second run
did not reproduce. The read path is fixed and the two now agree, but the longer
window is what proves it.

Reproduce: `tests/cluster/perf_matrix.sh`.

`chronik-bench` measures **round-trip throughput at a fixed concurrency** — each
producer waits for its acknowledgement. It is not a maximum-batched-throughput
benchmark; for that, see the kcat figures at the bottom.

## Single node

No replication to do, so this is the ceiling of the local write path.

| acks | throughput | p99 |
|---|---:|---:|
| 0 | **164,914 msg/s** (40.3 MB/s) | 7.71 ms |
| 1 | 14,476 msg/s (3.53 MB/s) | 5.30 ms |
| all | 14,307 msg/s (3.49 MB/s) | 5.55 ms |

`acks=1` and `acks=all` are identical here, and that is correct: with no
followers the in-sync set is the leader alone, so `acks=all` waits for its own
fsync and nothing more. The 12× gap to `acks=0` is that fsync.

## Three nodes, RF=3, min_insync_replicas=2

Follower-pull replication running; every record reaches all three nodes.

| acks | throughput | p99 |
|---|---:|---:|
| 0 | **111,146 msg/s** (27.1 MB/s) | 14.49 ms |
| 1 | 8,029 msg/s (1.96 MB/s) | 10.72 ms |
| all | 6,197 msg/s (1.51 MB/s) | 41.05 ms |

`acks=0` and `acks=1` cost roughly what the single-node shape costs, plus
contention from two extra brokers on the same disk.

`acks=all` is **1.3× slower than `acks=1`**, which is the cost of the follower
round trip: a producer cannot be acknowledged until a follower's *next* fetch
reports a position past the record, so each write pays a fetch plus the
follower's own fsync on top of the leader's.

It used to be **4–7× slower** and to decay during a run — 3,993 msg/s in the
first interval, 2,285 in the second. That was not `acks=all` degrading; every
fetch re-read and re-parsed the whole active WAL segment from byte zero, so the
cost grew with the file. RP-9 replaced that with a bounded tail cache and a
sparse offset index, taking the sustained figure from ~1,400 to 6,197 msg/s and
making it stable across the run rather than a function of how long you look.

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

That `acks=all` reaches 542K msg/s when batched while managing 3.8K when each
message waits for its own acknowledgement is not a contradiction: it is the
difference between amortising one replication round trip over thousands of
records and paying one per record. The broker's ingest was never the constraint.

## What is not measured here

- **Bare metal.** All of the above is one machine. `BARE_METAL_PERFORMANCE.md`
  covers what is owed on the Dell cluster.
- **Consume throughput.** `chronik-bench -m consume` exists; these runs are
  produce-only.
- **Searchable / columnar / vector topics.** The old report measured a 33%
  cluster overhead for searchable topics; that figure is void with the rest and
  has not been re-taken.
