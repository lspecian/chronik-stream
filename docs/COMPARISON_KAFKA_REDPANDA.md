# Chronik vs Kafka vs Redpanda — same box, same client, same harness

Measured 2026-08-16. Every previous performance claim in this repo compared
Chronik against Chronik, or against a number from different hardware. This runs
all three brokers on one machine, driven by the same rdkafka-based client
(`chronik-bench`), and reports **bytes landed on disk** next to every throughput
figure — because a broker that rejects work returns faster than one that does it,
which is how a phantom 17× regression got published here (see RP-11 in
`ROADMAP_REPLICATION.md`).

## Setup

| | |
|---|---|
| Host | 8-core / 16-thread Ryzen, single socket, NVMe, `tsc` clocksource |
| Client | `chronik-bench` (librdkafka) — identical for all three |
| Workload | 256 B payloads, 3 partitions, 1024 concurrent producers, 60s |
| Topology | Single node, RF=1, no replication anywhere |
| Chronik | native binary, default WAL profile (`low_resource`: 2 ms group commit) |
| Redpanda | `redpandadata/redpanda:latest`, `--smp 16 --memory 8G`, `--network host`, host bind mount |
| Kafka | `apache/kafka:latest` (KRaft), `--network host`, host bind mount |

Containers use host networking and host-mounted data directories so the network
and disk paths match the native Chronik process as closely as possible.

## Results — `acks=1`

| | msg/s | on disk | bytes/msg | × payload |
|---|---:|---:|---:|---:|
| **Chronik** | **114,031** | 12,166 MB | 1,864 | 7.3× |
| Redpanda | 46,033 | 1,126 MB | 427 | 1.7× |
| Kafka | 24,707 | 774 MB | 548 | 2.1× |

Zero client-reported failures for all three. Chronik is **2.5× Redpanda** and
**4.6× Kafka** on throughput, and writes **4.4× more bytes per message** than
Redpanda to get there.

### Durability, verified rather than assumed

The comparison only means anything if the acknowledgements are worth the same.
Redpanda fsyncs before acking by default; Chronik group-commits and acks after
the fsync of its batch. Tested directly — 200,000 records at `acks=1`, `SIGKILL`
with no chance to flush, restart on the same data directory, count what came
back:

```
run 1: acknowledged=200000  UNIQUE=200000  duplicates=0  LOST=0
run 2: acknowledged=200000  UNIQUE=200000  duplicates=0  LOST=0
run 3: acknowledged=200000  UNIQUE=200000  duplicates=0  LOST=0
run 4: acknowledged=200000  UNIQUE=200000  duplicates=0  LOST=0
```

One earlier reading reported 210,000 records back from 200,000 produced, which
did not reproduce in four subsequent runs and is recorded here as an unexplained
measurement artifact rather than swept up.

**Kafka's `acks=1` does not fsync at all** — its defaults leave
`log.flush.interval.messages` effectively infinite and rely on replication for
durability, so at RF=1 an acknowledged Kafka record can be lost to a power cut.
It is therefore doing *less* work than the other two and is still the slowest
here.

## Scaling — the benchmark, not the broker, was the limit

Throughput at `acks=1` climbs almost linearly with producer count:

| producers | msg/s |
|---:|---:|
| 8 | 2,583 |
| 32 | 7,246 |
| 64 | 13,582 |
| 128 | 26,153 |
| 256 | 49,797 |
| 512 | 90,143 |
| 1024 | **154,189** (median of 3: 154,351 / 154,001 / 154,189) |

`acks=0` at 1024 producers: **513,260 msg/s** (median of 3), 6,629 MB landed.

Every earlier single-node figure in this repo was taken at 64 producers, which
under-loads the broker by an order of magnitude. 12,582 msg/s was never a
ceiling — it was one point on a straight line.

## The headroom: Chronik writes everything twice

```
6.3 GB  /wal
5.7 GB  /segments
```

Kafka and Redpanda have one copy on disk because the log *is* the storage.
Chronik writes each record to the WAL for durability and again into segments for
serving and tiering. That is most of the 7.3× amplification, and it is being paid
on the hot path while still outrunning both competitors — so removing it is the
single largest optimisation available:

1. **Eliminate the double write.** Seal WAL segments *as* the served segments, or
   have segment construction reference WAL bytes instead of copying them.
   Plausibly worth a large fraction of the remaining gap, and halves disk usage.
2. ~~**Group commit window.**~~ Measured — see below. All four profiles land
   within 0.7% of each other at saturation. Not a lever.
3. ~~**`is_topic_vector_enabled` is uncached.**~~ Fixed: it now carries the same
   60 s TTL cache as its sibling `is_topic_searchable`, which had one since
   v2.2.16. It runs on every produce, ungated, so it was a `TopicMetadata` fetch
   and clone per batch. **No measurable throughput change** (153,106 → 153,802,
   inside the noise band) — the metadata store is already in-memory, so what this
   removes is an allocation, not I/O. Kept because it is correct and consistent,
   not because it showed up.

## What this does not measure

- **Single node, RF=1.** No replication for any of the three. Chronik's
  follower-pull replication (RP-0..RP-10) is not in the path, and neither is
  Kafka's ISR nor Redpanda's Raft. Multi-node numbers will differ.
- **One payload shape.** 256 B, 3 partitions. Larger payloads shift the balance
  toward bytes/sec and away from per-record overhead, which is where Chronik's
  amplification hurts most.
- **A laptop.** Sustained runs can thermally throttle; the 60 s runs are short
  enough that this is unlikely to dominate but not short enough to rule out.
- **JVM warmup.** Kafka improved 19,748 → 24,707 going from a 12 s to a 60 s run.
  A longer run would likely help it further; 60 s is where this comparison stops.
- **Consume, latency percentiles, and mixed workloads** — produce throughput only.

## WAL profile: a low-load latency knob, not a throughput knob

`CHRONIK_WAL_PROFILE` sets the group-commit window and batch size
(low=2 ms/500, medium=10 ms/2k, high=50 ms/10k, ultra=100 ms/20k). Median of 3
per profile, 1024 producers, `acks=1`:

| profile | msg/s | p50 ms | p99 ms | disk MB |
|---|---:|---:|---:|---:|
| low (default) | 153,802 | 5.26 | 7.55 | 1998.2 |
| medium | 153,676 | 5.27 | 7.45 | 1996.8 |
| high | 154,691 | 5.23 | 7.32 | 2010.5 |
| ultra | 154,658 | 5.23 | 7.47 | 2009.6 |

**No measurable difference — 0.7% spread across a 50× range of commit windows.**
Under saturation the batch fills by *size* long before the timer expires, so the
window never binds. The profile matters at low load, where it sets how long a
lone record waits for company; it is not a throughput lever, and there is no
throughput left on the table here.

⚠️ A first attempt at this sweep reported low=153,106 against medium=68,569 and
high=68,516, which looked like a dramatic result and was an artifact: a
`cargo build` was running during those two measurements and took the CPU. Same
lesson as RP-11 — the surprising number was measuring the harness, not the
system. Re-run on a quiet machine with a fixed binary, the effect vanishes.
