# Chronik Stream: Bare Metal Performance

Measured 2026-08-14 on three Dell R630s with follower-pull replication genuinely
running, replacing a report that was deleted rather than annotated.

> ### Why the previous report was deleted
>
> Every figure it contained — 837,284 msg/s at 256 B, 411 MB/s at 1 KB, the
> scaling projections built on them — was measured on v2.2.25, which carried the
> bug fixed in PR #29: `produce_to_partition` returned before reaching the
> replication hook on any path where `acks != 0`, so `acks=1` and `acks=all`
> replicated **nothing**.
>
> Every one of those runs used `acks=all`. The cluster held one copy of the data
> while reporting `isr:[1,2,3]`, so the report measured a 3-node cluster doing
> the work of a single node and called the result "fully replicated, strongest
> durability guarantee". Its headline finding — *"zero data loss across 1B+
> messages... genuine durability"* — was measuring a durability guarantee that
> was not being provided.
>
> Carrying those tables with a warning on top was the wrong call: a warning is
> read once, tables are cited forever. The numbers below share no lineage with
> them.

---

## Read this before comparing against the old numbers

**These measure a different thing than the deleted report did, and the numbers
are not comparable.** The old headline — 837,284 msg/s — was a *batched* figure:
k6 posting 100–200 messages per HTTP request through 12–36 ingestor pods,
aggregated across all of them. The tables below are *round-trip* figures: 64
producers, each one waiting for its own acknowledgement before sending the next.

The two regimes are not close. Measured below on this hardware, batching is
worth **4–5×** at every acks level — and at `acks=0` the batched run stops at
the client's 1 GbE link rather than at anything Chronik does.

Round trip is the harder question and the one this work needed: replication cost
only appears when someone is waiting for it, and a batched pipeline hides it
almost completely. But it means **a reader who remembers 837K and sees 110K
below is comparing a batched aggregate to an unbatched round trip, not a
regression.**

Measured in the old number's own regime, the gap is mostly gone and the rest is
accounted for:

| | msg/s | |
|---|---:|---|
| old report, `acks=all` | 837,284 | batched, **replication disabled**, 12–36 ingestor pods, in-cluster over loopback |
| today, batched `acks=0` | 429,737 | **client's 1 GbE saturated at 98%** — a floor, not the broker's ceiling |
| today, batched `acks=1` | 122,828 | replication running, one client, over a real link |
| today, batched `acks=all` | 52,931 | as above, and waiting for a follower on every batch |

These are not comparable, and the reason is structural rather than a matter of
degree. The old figure was an **aggregate over 12–36 producer pods running
inside the cluster**, with no client-side network hop to limit any of them.
Today's is **one** producer on one machine behind a single 1 GbE link — and at
`acks=0` that link is 98% full, which means the measurement stopped at the wire
and never reached the broker.

What can be said: the old run was not replicating whatever its flag claimed, so
it was doing strictly less work per record than any row here. How much less is
not recoverable from a number taken on a different topology, and the honest
answer to "did throughput drop 7.6×" is that the two runs never measured the
same quantity.

Both regimes are measured below, on the same hardware, in the same session.

## What was measured

Three brokers, one per Dell, as plain host processes — no Kubernetes in the
path. **The load generator runs on a fourth machine.** That is the part every
previous bare-metal number here lacked: the client was co-located with a broker,
so a network-bound result and a sender-bound one were indistinguishable (OQ1).

`chronik-bench` with **`--linger-ms 0`**, 64 concurrent producers each awaiting
its own acknowledgement before sending again, 3 partitions, RF=3,
`min_insync_replicas=2`, 30-second runs, WAL profile left at its default. The
zero linger is the whole point: it makes every message a round trip. **Every figure is the median of three runs, each on a freshly
started cluster**, with the individual samples kept beside it.

Replication was verified before measuring, not assumed: 100 records produced at
`acks=all`, then counted on each node's disk — 100, 100, 100.

Reproduce: `./tests/cluster/baremetal.sh all`

### 256-byte messages

| acks | throughput | bandwidth | p99 | node-1 NIC peak | samples |
|---|---:|---:|---:|---:|---|
| 0 | **110,427 msg/s** | 26.96 MB/s | 7.96 ms | 627 Mbit/s | 45,589 · 114,628 · 110,427 |
| 1 | 22,690 msg/s | 5.54 MB/s | 6.79 ms | 667 Mbit/s | 23,698 · 22,690 · 22,006 |
| all | 12,475 msg/s | 3.05 MB/s | 8.57 ms | 52 Mbit/s | 12,919 · 12,475 · 11,997 |

### 1 KB messages

| acks | throughput | bandwidth | p99 | node-1 NIC peak | samples |
|---|---:|---:|---:|---:|---|
| 0 | **72,451 msg/s** | 70.75 MB/s | 10.68 ms | 720 Mbit/s | 81,411 · 70,718 · 72,451 |
| 1 | 12,727 msg/s | 12.43 MB/s | 39.45 ms | **759 Mbit/s** | 12,727 · 14,859 · 10,758 |
| all | 10,624 msg/s | 10.38 MB/s | 9.86 ms | 138 Mbit/s | 10,409 · 10,624 · 10,791 |

### Batched, 256-byte messages — the other regime

`kcat` piping 5,000,000 records without waiting on any of them, so librdkafka
batches for real. The client's own NIC is sampled during each run, because at
these rates it is a candidate for the bottleneck.
Reproduce: `./tests/cluster/baremetal.sh batched`

| acks | batched | run length | client tx | round trip (above) | batching is worth |
|---|---:|---:|---:|---:|---:|
| 0 | 429,737 msg/s | 11 s | **983 Mbit/s — 98% of link** | 110,427 msg/s | 3.9× |
| 1 | 122,828 msg/s | 40 s | 281 Mbit/s — 28% | 22,690 msg/s | 5.4× |
| all | 52,931 msg/s | 94 s | 122 Mbit/s — 12% | 12,475 msg/s | 4.2× |

**`acks=0` is not a measurement of Chronik.** It saturates the client's 1 GbE
uplink at 98%, so it is a floor: the broker was never the constraint and would
go faster behind a faster client link. `acks=1` and `acks=all` sit at 28% and
12% of that link, so those two *are* measurements of the cluster.

Batching is worth a consistent **4–5×** across all three acks levels, which is
the sane shape: it amortises per-request overhead, and it cannot amortise away
either the leader's fsync or the follower round trip, because both happen once
per batch however large the batch is.

> ⚠️ **An earlier version of this table was wrong, and it is worth saying how.**
> It used 200,000 records, which at these rates finishes in about half a second
> — long enough to measure the client's send buffer draining and nothing else.
> It reported `acks=0` at 355,871 and `acks=1` at **386,100**, i.e.
> acknowledging every batch on the leader's disk came out *free, and slightly
> faster than not acknowledging at all*. That cannot be true, and it is what
> gave it away. Over a proper window the same runs give 429,737 and 122,828 — a
> 3.5× gap in the direction physics requires. The harness now defaults to
> 5,000,000 records and prints the run length beside every row, with a warning
> under ten seconds, so a too-short window is visible instead of silently
> becoming a number.

⚠️ **`--linger-ms` cannot produce these numbers, and trying it is a trap.**
`chronik-bench` awaits each message's delivery before sending the next, so a
producer never has more than one message in flight and linger has nothing to
accumulate — it only adds dead time. Measured at 256 B `acks=0`: **5,359 msg/s
with `--linger-ms 10` against 110,427 with 0**, NIC falling from 627 to 18
Mbit/s. 64 producers ÷ 10 ms = 6,400, which is the whole of the result. A
synchronous benchmark cannot be batched by a client setting.

---

## Open Question 1, answered: it depends on the acks level

The link is 1 GbE — ~940 Mbit/s of usable line rate. Sampling each broker's NIC
during the runs settles what the old report could not:

- **`acks=0` and `acks=1` are close to the network.** 627–759 Mbit/s, i.e.
  **67–81% of line rate**. At 1 KB `acks=1` the transport is a first-order
  constraint, and no amount of sender-side work will move it. A 10 GbE link is
  the change that matters there, not a code change.
- **`acks=all` is nowhere near the network.** 52 Mbit/s at 256 B and 138 Mbit/s
  at 1 KB — **6% and 15% of line rate**. It is bounded by the replication round
  trip, not by bytes on the wire.

That split is the useful finding. It says the two regimes need different work,
and it retires the assumption that one number describes the cluster.

## What replication costs

`acks=all` runs at **55% of `acks=1`** at 256 B and **83%** at 1 KB. The larger
message amortises the round trip over more payload, which is the same reason
batched producers see almost no replication cost at all.

Both numbers are far better than they were before RP-9 — the developer-machine
figure went from ~1,400 to ~6,100 msg/s there — but the ratios are what transfer
across hardware, and the ratio here is 1.2–1.8×, not the 4–7× this work started
from.

## Against the developer machine

`BASELINE_PERFORMANCE.md` measures the same benchmark with all three brokers and
the client on one 16-core box. The comparison is worth stating because it is not
a simple speed-up:

| | dev box (3 brokers + client, 16 cores) | bare metal (3 Dells, client separate) |
|---|---:|---:|
| `acks=0` | 111,146 msg/s | 110,427 msg/s |
| `acks=1` | 8,029 msg/s | 22,690 msg/s |
| `acks=all` | 6,197 msg/s | 12,475 msg/s |

`acks=0` is identical, and that is the tell: it is the one mode that does no
replication and no fsync wait, so it measures how fast the client can push and
the broker can accept. Both setups hit the same ceiling — on the dev box the
loopback and the client's own cores, on bare metal the 1 GbE link at 627
Mbit/s. The modes that wait — `acks=1` and `acks=all` — are **2.8× and 2.0×**
faster on real hardware, which is the disk and the cores that were being shared
four ways on the dev box.

---

## Still owed

- **Ingestor-count scaling.** The old report's "scales 12 → 36 pods" claim has
  not been re-tested with replication running; that shape needs the k6/k8s stack,
  not this harness.
- **Consume throughput**, which the old report never isolated from its HTTP
  pipeline.
- **10 GbE.** Two of the six rows are pressed against the 1 GbE link, so the
  cluster's produce ceiling at `acks=0`/`acks=1` is currently a property of the
  network, not of Chronik.

## Hardware (unchanged, still the target)

### Cluster Nodes (x3: dell-1, dell-2, dell-3)

| Component | Specification |
|-----------|--------------|
| Server | Dell PowerEdge R630 |
| CPU | 2x Intel Xeon E5-2667 v4 @ 3.20GHz (8 cores / 16 threads each) |
| Total Cores | 32 logical CPUs per node (96 total across cluster) |
| Memory | 256 GB DDR4 per node (768 GB total) |
| Storage | 1.8TB PERC H730P RAID + 1TB Kingston NVMe SSD |
| Filesystem | ext4 on RAID |
| Network | **1 GbE** (eno3/eno4) |
| OS | Ubuntu 24.04.3 LTS, Kernel 6.8.0-94-generic |

⚠️ **1 GbE is 125 MB/s per link**, and four of the six rows above are pressed
against it. That is now measured rather than suspected — see Open Question 1.

---

## Method

### The shape used for the numbers above

Three brokers as plain host processes, one per Dell, on ports Kubernetes is not
using — so the MicroK8s clusters on those machines keep running untouched. The
load generator (`chronik-bench`) runs on a **fourth** machine.

```bash
./tests/cluster/baremetal.sh all     # deploy, measure, tear down
```

Deliberately no Kubernetes in the path. The question this report exists to
answer is what replication costs on real hardware over a real network, and a CNI
between the brokers is a second variable measured at the same time. The k8s
shape is a different experiment, and it is still owed.

Each row: median of three 30-second runs, each on a freshly started cluster,
with the individual samples kept. A single run is not reproducible — the 256 B
`acks=0` samples were 45,589 · 114,628 · 110,427 — so a report of one number per
row would be reporting noise.

### Verify replication before trusting any number

The old report's central error was assuming `acks=all` implied replication.
Check the bytes on disk on each node, not `isr:` in a status response.
`tests/cluster/regression_replication.sh` is the pattern: produce, then count
records in each node's log. This run did that first — 100 records at `acks=all`,
then 100 / 100 / 100 on disk — before any measurement was taken.

### The Kubernetes shape (still owed, unchanged from the original report)

| Component | Version |
|-----------|---------|
| Kubernetes | MicroK8s |
| Container Runtime | containerd |
| CNI | Calico |
| Container Registry | Harbor (self-hosted) |
| Load Generator | Grafana k6 (k6-operator) |

- **Load generator**: Grafana k6 via k6-operator, 8 runner pods, ramp 0 → 8000
  VUs over ~12 minutes, 100–200 messages per HTTP request.
- **Ingestor**: Rust (chronik-perf), rdkafka, HTTP POST → JSON decode → Kafka
  produce → await ack → HTTP response, 12 to 36 replicas across 3 nodes.
- **Consumer**: Rust (chronik-perf), rdkafka, consumer group with partition
  assignment and a metrics endpoint.

```bash
./tests/k8s-perf/run-all.sh    # deploy the full test stack
./tests/k8s-perf/cleanup.sh    # tear it down
```

---

## Appendix: Bug Fix During The Original Testing

### Closed-Socket Spin Loop (v2.2.25)

Kept because the bug and its fix are real, whatever happened to the numbers.

**Symptom**: After stress tests, all 32 CPU cores on each node pinned at 100% permanently.

**Root Cause**: In `server.rs`, when `read_request_frame()` returned `Ok(None)`
(connection closed/EOF), the handler loop used `continue` instead of `break`,
causing infinite `recvfrom()` system calls on dead file descriptors (~17,000
calls/sec per FD, ~100 dead FDs per node).

**Fix**: `Ok(None) => continue` became `Ok(None) => break` in both the TCP and
TLS connection handlers.
