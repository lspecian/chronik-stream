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

`chronik-bench`, 64 concurrent producers, 256-byte messages, 10s measured after
a 3s warmup, 3 partitions, no compression, **WAL profile left at its default**
(`low`, 2 ms). Rates below are the *sustained* per-interval rate, not the
harness's summary figure, which divides by an elapsed time that includes warmup
and drain.

Reproduce: `tests/cluster/perf_matrix.sh`.

`chronik-bench` measures **round-trip throughput at a fixed concurrency** — each
producer waits for its acknowledgement. It is not a maximum-batched-throughput
benchmark; for that, see the kcat figures at the bottom.

## Single node

No replication to do, so this is the ceiling of the local write path.

| acks | throughput | p50 | p99 |
|---|---:|---:|---:|
| 0 | **195,000 msg/s** (~47 MB/s) | 0.05 ms | 7.6 ms |
| 1 | 16,700 msg/s (~4.1 MB/s) | 3.83 ms | 5.3 ms |
| all | 16,700 msg/s (~4.1 MB/s) | 3.83 ms | 5.3 ms |

`acks=1` and `acks=all` are identical here, and that is correct: with no
followers the in-sync set is the leader alone, so `acks=all` waits for its own
fsync and nothing more. The 12× gap to `acks=0` is that fsync.

## Three nodes, RF=3, min_insync_replicas=2

Follower-pull replication running; every record reaches all three nodes.

| acks | throughput | p50 | p99 |
|---|---:|---:|---:|
| 0 | **119,000 msg/s** (~29 MB/s) | 0.19 ms | 13.0 ms |
| 1 | 15,000 msg/s (~3.7 MB/s) | 3.66 ms | 15.6 ms |
| all | 2,300–4,000 msg/s | — | 34–49 ms |

`acks=0` and `acks=1` cost roughly what the single-node shape costs, plus
contention from two extra brokers on the same disk.

⚠️ **`acks=all` is 4–7× slower than `acks=1` and degrades within a run** — 3,993
msg/s in the first interval, 2,285 in the second, p99 rising 34 → 49 ms. That is
a genuine open issue, recorded in `docs/ROADMAP_REPLICATION.md`, not a
measurement artefact: it reproduces on a freshly created cluster with a single
topic.

The likely shape: `acks=all` throughput is bounded by the follower's fetch loop,
which issues one request per leader at a time with `min_bytes=1`. That is
latency-optimal — the leader answers the moment anything lands, which is what
made #36's per-request latency fall from 505 ms to 17 ms — and it means each
round trip carries only what accumulated during the previous one. Raising
`min_bytes` would trade the latency win back for batch size. **Not yet
investigated properly; do not treat this explanation as established.**

## Batched throughput, for contrast

The same 3-node cluster measured with `kcat`, which batches thousands of records
per request instead of waiting per message (`tests/cluster/perf_replication.sh`,
200,000 × 100 B, median of three):

| acks | throughput |
|---|---:|
| 0 | 1,058,201 msg/s |
| 1 | 694,444 msg/s |
| all | 488,997 msg/s |

All three stored 200,001 records with zero client errors. These answer a
different question — how fast the broker ingests when the client batches — and
should never be compared against the round-trip table above.

That `acks=all` reaches 489K msg/s when batched, while managing 2–4K when each
message waits for its own acknowledgement, is the clearest statement of the open
issue: the per-round-trip path, not the broker's raw ingest, is what is slow.

## What is not measured here

- **Bare metal.** All of the above is one machine. `BARE_METAL_PERFORMANCE.md`
  covers what is owed on the Dell cluster.
- **Consume throughput.** `chronik-bench -m consume` exists; these runs are
  produce-only.
- **Searchable / columnar / vector topics.** The old report measured a 33%
  cluster overhead for searchable topics; that figure is void with the rest and
  has not been re-taken.
