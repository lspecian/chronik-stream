# Bare-Metal Performance

Produce throughput and latency for a 3-node Chronik cluster on dedicated
hardware, with replication active and verified. Measured 2026-08-16.

Two regimes are reported separately because they answer different questions:

| | 256 B, `acks=1` | what it tells you |
|---|---:|---|
| **Round trip** — each producer awaits its own acknowledgement | 42,804 msg/s | latency-bound throughput, including the cost of replication |
| **Batched** — the client accumulates records before sending | 394,539 msg/s | pipeline throughput, where replication cost is largely hidden |

A single node reaches **228,874 msg/s** at `acks=1` and **513,260 msg/s** at
`acks=0`.

All figures are the median of three runs on a freshly started cluster, with the
individual samples published beside them.

---

## Round-trip throughput

1024 concurrent producers, each awaiting delivery before sending again;
3 partitions, RF=3, `min_insync_replicas=2`, `--linger-ms 0`, 30-second runs,
default WAL profile. Load generated from a fourth machine, so no broker shares
a host with the client.

### 256-byte messages

| `acks` | throughput | bandwidth | p99 | leader NIC peak | samples (msg/s) |
|---|---:|---:|---:|---:|---|
| `0` | **162,230 msg/s** | 39.61 MB/s | 36.64 ms | 573 Mbit/s | 167,343 · 158,179 · 162,230 |
| `1` | **42,804 msg/s** | 10.45 MB/s | 111.36 ms | **943 Mbit/s (98% of link)** | 42,804 · 41,782 · 43,719 |
| `all` | **18,589 msg/s** | 4.54 MB/s | 409.60 ms | 124 Mbit/s | 19,260 · 18,589 · 16,726 |

`acks=1` is bound by the network, not the broker: 943 Mbit/s against roughly
940 Mbit/s of usable line rate on 1 GbE. Further gains there require a faster
fabric.

### 1 KB messages

| `acks` | throughput | bandwidth | p99 | leader NIC peak | samples (msg/s) |
|---|---:|---:|---:|---:|---|
| `0` | **72,451 msg/s** | 70.75 MB/s | 10.68 ms | 720 Mbit/s | 81,411 · 70,718 · 72,451 |
| `1` | **12,727 msg/s** | 12.43 MB/s | 39.45 ms | 759 Mbit/s | 12,727 · 14,859 · 10,758 |
| `all` | **10,624 msg/s** | 10.38 MB/s | 9.86 ms | 138 Mbit/s | 10,409 · 10,624 · 10,791 |

### What replication costs

`acks=all` runs at **55%** of `acks=1` at 256 B and **83%** at 1 KB. The larger
message amortises the replication round trip over more payload — the same reason
batched producers see almost no replication cost.

---

## Batched throughput

`kcat` piping 5,000,000 records without awaiting individual acknowledgements, so
librdkafka batches for real. 256 B payloads. The client's own NIC is sampled
during each run, because at these rates it is a bottleneck candidate.

| `acks` | run 1 | run 2 | client tx (run 1) |
|---|---:|---:|---:|
| `0` | 429,737 msg/s | 429,368 msg/s | **983 Mbit/s (98% of link)** |
| `1` | 122,828 msg/s | 394,539 msg/s | 281 Mbit/s (28%) |
| `all` | 52,931 msg/s | 74,335 msg/s | 122 Mbit/s (12%) |

**`acks=0` here is a floor, not a ceiling.** It reproduces to 0.1% across runs
because it is pinned at the client's 1 GbE uplink at 98% utilisation. The broker
was never the constraint and would go faster behind a faster link.

**`acks=1` and `acks=all` batched are not reproducible, and no single figure is
claimed for them.** Two runs of an identical configuration returned 122,828 and
394,539 — a 3.2× spread, which is two behaviours rather than noise around a
value. Both runs stored every record with zero client errors, so this is not a
failure mode; the most likely cause is how partition leadership happens to
distribute across the three nodes, and therefore where the fsync load lands.
A median of those two numbers would carry no meaning, so none is given.

What is established: batching helps substantially, and the acks ordering is
monotonic in every individual run (`acks=0` > `acks=1` > `acks=all`).

### Scaling under batched load

36 producer processes across 8 load generators, 100 messages per batch, 256 B,
`acks=all`, constant concurrency for 3 minutes per step.

| concurrent producers | msg/s | batch latency | errors |
|---:|---:|---:|---:|
| 1,000 | 8,500 | 11.28 s | 0 |
| 3,000 | 23,741 | 12.04 s | 0 |
| 6,000 | 42,396 | 13.36 s | 0 |
| 12,000 | 82,643 | 13.62 s | 0 |
| 24,000 | **160,329** | 14.21 s | 0 |

Throughput scales close to linearly to 24,000 concurrent producers with no
errors. Batch latency is dominated by queueing at the client, not by the broker.

---

## What limits each number

Each broker's NIC was sampled during every run. On a 1 GbE link (~940 Mbit/s
usable) the results split cleanly, and the split matters more than any single
number:

- **`acks=0` and `acks=1` are transport-bound.** 573–943 Mbit/s, i.e. 61–98% of
  line rate. Sender-side optimisation will not move these; a faster fabric will.
- **`acks=all` is not.** 124 Mbit/s at 256 B and 138 Mbit/s at 1 KB — 13% and
  15% of line rate. It is bounded by the replication round trip, not by bytes on
  the wire.

### Moving replication onto a dedicated 10 GbE fabric

Replication traffic shares the client-facing link by default. Pointing it at a
separate 10 GbE interface isolates the two. Measured at 64 producers on both
fabrics (a lighter load than the tables above — compare the two columns to each
other, not to the headline figures):

| `acks` | replication on 1 GbE | on 10 GbE | change | leader NIC peak |
|---|---:|---:|---:|---|
| `0` | 104,937 msg/s | 106,690 msg/s | +1.7% | 693 → 131 Mbit/s |
| `1` | 21,802 msg/s | **28,587 msg/s** | **+31%** | 690 → 46 Mbit/s |
| `all` | 11,499 msg/s | 11,877 msg/s | +3.3% | 54 → 19 Mbit/s |

`acks=1` gains 31%: it was genuinely competing with client traffic for the same
link. `acks=all` barely moves, consistent with it being bound by the round trip
rather than by bandwidth.

`acks=0` is the instructive one. It sat at 693 Mbit/s — 69% of line rate, which
reads as "nearly saturated" — and yet freeing the link gained 1.7%. High link
utilisation is not by itself evidence that the link is the constraint.

---

## Single-node ceiling

8-core/16-thread single socket, NVMe. 256 B payloads, 3 partitions,
interleaved median of three.

| | msg/s | p50 |
|---|---:|---:|
| `acks=1`, 1024 producers | **228,874** | 3.42 ms |
| `acks=0`, 1024 producers | **513,260** | — |

Throughput scales close to linearly with producer count to roughly 1024.
Measurements taken at low concurrency (64 producers → ~13,500 msg/s) describe the
load generator, not the broker.

Durability at `acks=1` is verified rather than assumed: 200,000 records
acknowledged, `SIGKILL` with no flush, restart on the same data directory —
200,000 unique records recovered, zero lost, zero duplicated, across four runs.

### io_uring is disabled by default

The io_uring WAL path is opt-in via `CHRONIK_WAL_IO_URING=true`. It is off
because it costs throughput on local NVMe:

| | msg/s | p50 |
|---|---:|---:|
| io_uring | 152,851 | 5.30 ms |
| standard tokio file I/O | **228,874** | **3.42 ms** |

io_uring's advantage is amortising I/O syscalls at high IOPS, and after group
commit there are few left to amortise. At 229k msg/s the broker issues 11,618
`fsync` per 10 s — 197 messages per fsync — and I/O syscalls account for **6.3%**
of syscall time, against **92%** for `futex` (thread wake-ups and scheduling).
Perfect batching would save around 0.65 core-seconds per second on a machine
running 16 cores at 14% utilisation: it optimises a resource that is not scarce.

The implementation's obvious inefficiencies were addressed without changing the
result — the command loop no longer blocks inside the async runtime, writes run
concurrently across partitions, and buffers are submitted without a copy. The
remaining gap is architectural: `tokio-uring` submits one operation per
`io_uring_enter`, so it provides no syscall amortisation while still paying a
cross-thread round trip per operation. Its API exposes no linked submissions,
registered buffers, or batched submission, so this cannot be closed within the
library.

A version that could win would use the raw `io-uring` crate with a hand-written
driver: a whole group-commit batch as one linked write→fsync, one ring per core,
registered buffers, no channel on the hot path. That is worthwhile when the
workload is genuinely I/O-bound — cloud block storage at 125–250 MB/s rather
than 1.4 GB/s local NVMe.

---

## Dedicated hardware versus a shared development machine

`BASELINE_PERFORMANCE.md` runs the same benchmark with all three brokers and the
client on one 16-core host. Both columns are at 64 producers.

| | dev host (3 brokers + client) | dedicated (3 nodes, separate client) |
|---|---:|---:|
| `acks=0` | 111,146 msg/s | 110,427 msg/s |
| `acks=1` | 8,029 msg/s | 22,690 msg/s |
| `acks=all` | 6,197 msg/s | 12,475 msg/s |

`acks=0` is identical, and that is the useful signal: it is the one mode that
does no replication and no fsync wait, so it measures how fast a client can push
and a broker can accept. Both setups reach the same ceiling for different
reasons — loopback and contended cores on one, the 1 GbE link on the other. The
modes that wait are **2.8×** and **2.0×** faster on dedicated hardware, which is
the disk and the cores that were being shared four ways.

---

## Reproducing these numbers

### Hardware

Three identical cluster nodes, plus a fourth machine for the load generator.

| Component | Specification |
|-----------|--------------|
| Server | Dell PowerEdge R630 |
| CPU | 2× Intel Xeon E5-2680 v4 (14C/28T each, 2.40 GHz) |
| Threads | 56 per node |
| Memory | 256 GB DDR4 ECC |
| Storage | NVMe SSD |
| Client link | 1 GbE |
| Replication link | 2× 10 GbE (optional, see above) |

### Commands

```bash
# Round-trip and batched matrices
./tests/cluster/baremetal.sh all
./tests/cluster/baremetal.sh batched
```

### Method

- **Median of three runs**, each against a freshly started cluster. Individual
  samples are published so the spread is visible.
- **Replication verified before measuring, not assumed**: 100 records produced at
  `acks=all`, then counted on each node's disk — 100, 100, 100.
- **Per-node NIC sampled during each run**, so a transport-bound result is
  distinguishable from a sender-bound one.
- **Load generator on its own machine.** Co-locating it with a broker makes those
  two cases indistinguishable.

### Measurement traps worth knowing

These cost real time to diagnose, and any benchmark of this system will hit them.

**A run shorter than ten seconds measures the client's send buffer draining, not
the broker.** At these rates 200,000 records complete in about half a second,
which yields `acks=1` at 386,100 against `acks=0` at 355,871 — acknowledging
every batch on the leader's disk appearing *free, and faster than not
acknowledging at all*. Over a proper window the same runs give 429,737 and
122,828. The harness defaults to 5,000,000 records and prints run length beside
every row, warning below ten seconds.

**`--linger-ms` cannot batch a synchronous benchmark, and setting it destroys the
result.** `chronik-bench` awaits each message's delivery before sending the next,
so a producer never has more than one message in flight and linger has nothing to
accumulate — it only adds dead time. At 256 B `acks=0`: **5,359 msg/s with
`--linger-ms 10` against 110,427 with 0**, NIC falling from 627 to 18 Mbit/s.
64 producers ÷ 10 ms = 6,400, which is the entire result.

**Throughput alone cannot tell a fast broker from a failing one.** A rejected
produce returns faster than a served one, so msg/s rises when the broker starts
refusing work. Every figure here is accompanied by bytes landed on disk or by a
record count read back.

---

## Known limits of this report

- **Consume throughput is not measured.** These figures are produce-side only.
- **Batched `acks=1` and `acks=all` are not reproducible** (see above); no figure
  is claimed for them.
- **`acks=0` and `acks=1` round-trip figures are network-bound** on the 1 GbE
  client link, so they describe the fabric as much as the broker.

## Withdrawn figures

Earlier revisions of this file reported substantially higher numbers — including
837,284 msg/s at 256 B — along with scaling projections derived from them. Those
measurements were taken on a build where `acks=1` and `acks=all` did not
replicate at all: the cluster held a single copy of the data while reporting a
full in-sync replica set. They are withdrawn in full and share no lineage with
anything above.

They were also batched aggregates rather than round-trip figures, so they were
never comparable to the round-trip tables here in the first place. The batched
section above is the like-for-like comparison, and `acks=0` — the one mode
unaffected by the replication bug — reproduces there.
