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

The two regimes are not close. At `acks=0` — the one batched figure that
reproduces here — batching is worth **3.9×**, and the batched run stops at the
client's 1 GbE link rather than at anything Chronik does.

Round trip is the harder question and the one this work needed: replication cost
only appears when someone is waiting for it, and a batched pipeline hides it
almost completely. But it means **a reader who remembers 837K and sees 110K
below is comparing a batched aggregate to an unbatched round trip, not a
regression.**

Measured in the old number's own regime, the gap is mostly gone and the rest is
accounted for:

### Saturation sweep: where is the ceiling?

36 ingestors, 8 k6 runners, 100 messages per batch, 256 B, `acks=all`, constant
VUs for 3 minutes per step (constant, not a ramp — a ramp reports one average
across every load level it passed through).

| VUs | msg/s | batch latency | errors | broker CPU |
|---:|---:|---:|---:|---|
| 1,000 | 8,500 | 11.28 s | 0 | |
| 3,000 | 23,741 | 12.04 s | 0 | |
| 6,000 | 42,396 | 13.36 s | 0 | |
| 12,000 | 82,643 | 13.62 s | 0 | 238m / 265m / 256m |
| 24,000 | **160,329** | 14.21 s | 0 | |
| 48,000 | — | — | — | **k6 runners OOMKilled** |

**No saturation point was found.** Throughput is linear in VUs across a 24×
range (8,500 → 160,329) while latency rises only 26% (11.3 → 14.2 s), and not
one message errored at any level. The run ended because the *load generator* ran
out of memory at 6,000 VUs per runner, not because the cluster did.

At 12,000 VUs each broker was using **~0.25 of a core** — of 32 available. At
24,000 VUs the nodes were at 10–29% CPU. Chronik was never the constraint at any
point on this curve.

Two things the sweep had to fix before it measured anything real:

- **k6 was the bottleneck, not Chronik.** Building each 256-byte payload
  character-by-character in JS costs ~25,600 `charAt`/`random` calls per request;
  the runners burned 2–3 cores each while the brokers sat at 148–431 millicores.
  Precomputing a pool of 64 random payloads at init dropped the runners to ~500
  millicores. (A pool, not one constant string — the ingestor produces with
  `compression.type=snappy`, and identical payloads would compress to nothing
  and inflate the result into fiction.)
- **The flat ~12 s latency is queueing in the ingestor pipeline, not the
  broker.** It follows Little's Law exactly: at 1,000 VUs, ~100,800 records in
  flight ÷ 8,500 msg/s ≈ 11.9 s. Latency barely moves as throughput rises 19×,
  which is the signature of a fixed pipeline delay rather than a saturating
  resource.

📌 Every cluster figure in this document was measured with the io_uring WAL path
active, which was the default until 2026-08-16 and costs roughly a third of
single-node produce throughput (see "io_uring is disabled by default"). They are
floors, not ceilings, and have not been re-run.

So the honest ceiling statement is: **≥160,329 msg/s at `acks=all` with zero
errors, and that is a floor, not a limit.** What this harness measures is the
HTTP-ingestor pipeline. The direct-to-Kafka figures earlier in this file
(429,368 msg/s at `acks=0`, saturating the client's 1 GbE) are the better guide
to what the broker itself does.

### The old test, re-run on the current build

Rather than argue from a different benchmark, the original harness was run again:
`tests/k8s-perf`, the same 12 in-cluster ingestor pods with the same producer
settings (`batch.num.messages=10000`, `linger.ms=5`, **snappy**, `acks=all`),
the same k6 max-load ramp to 5,000 VUs, 256-byte payloads. The only difference
from the original is that replication now actually happens.

The original spec, recovered from `390e4ab`, is exact: **12 ingestors, 8 k6
runners, 100 messages per batch, `acks=all`**, stages 30s→200, 1m→1000, 2m→3000,
5m→5000, 3m→8000, 2m→3000, 1m→0 VUs. Reproduced to the parameter:

| | msg/s | |
|---|---:|---|
| original claim, `acks=all` | **837,284** | 8 runners × ~104,660, 245 MB/s, median 282 ms |
| **this build, that exact config** | **25,647** | 22,681,100 messages, **0 errors**, 8 runners |
| this build, 36 ingestors, 24,000 VUs | **160,329** | 0 errors, and still not saturated |

⚠️ **These figures are extremely sensitive to harness configuration, and "same
harness" is not "same configuration".** The repo's `max-load-test.js` batches
**10** messages per request; the 837,284 run used **100**, with 8 runners and a
peak of 8,000 VUs rather than 6 and 5,000. Batch size alone swings the result
3.6× (7,085 → 25,647 msg/s). Confirm batch size, runner count and VU peak match
before comparing any two runs from this stack.

**The gap to 837,284 is still unexplained, but it is not the broker.** At every
load level measured the brokers ran at well under one core of the 32 available,
and throughput scaled linearly until the load generator itself died. To reach
837,284 through this pipeline at the measured ~6.7 msg/s per VU would need
~125,000 VUs; the original reports achieving it at 8,000, which is ~15× more
work per VU than anything reproducible here.

What can be said with evidence: there is no throughput regression attributable to
this build. Chronik is not the limiting component anywhere on the measured
curve.

Replication was verified during the run, not assumed: a probe record at
`acks=all` appeared in all three nodes' WALs before the load started, and after
4.1M messages each node held ~2.9–3.1 GB of the topic. p95 batch latency reached
17 s at 5,000 VUs, so this is a saturation ceiling, not a comfortable rate.

### Against the single-client measurements above

| | msg/s | |
|---|---:|---|
| today, batched `acks=0` | 429,368 | **client's 1 GbE saturated at 98%** — a floor, not the broker's ceiling |
| today, batched `acks=1` | 122,828–394,539 | not reproducible; see above |

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

| acks | run 1 | run 2 | client tx (run 1) | round trip (above) |
|---|---:|---:|---:|---:|
| 0 | 429,737 msg/s | 429,368 msg/s | **983 Mbit/s — 98% of link** | 110,427 msg/s |
| 1 | 122,828 msg/s | **394,539 msg/s** | 281 Mbit/s — 28% | 22,690 msg/s |
| all | 52,931 msg/s | 74,335 msg/s | 122 Mbit/s — 12% | 12,475 msg/s |

**Only the `acks=0` row is trustworthy, and it is not a measurement of Chronik.**
It reproduces to 0.1% across runs because it is pinned at the client's 1 GbE
uplink — 98% full. That makes it a floor: the broker was never the constraint
and would go faster behind a faster link. Batching is worth ~3.9× there
(110,427 → 429,368).

⛔ **`acks=1` and `acks=all` batched are NOT reproducible and no figure is
claimed for them.** Two runs of the identical configuration gave 122,828 and
394,539 — a 3.2× spread, which is not noise around a value, it is two different
behaviours. Both runs stored every record with zero client errors, so it is not
a failure; something about the run (most likely how partition leadership
happened to distribute across the three nodes, and therefore how the fsync load
landed) changes the regime. Until that is understood and the runs repeat, a
median of these would be a number with no meaning behind it.

What *is* established: batching helps, the round-trip table above is the
reproducible one, and the acks ordering is monotonic in every individual run
(`acks=0` > `acks=1` > `acks=all`) — which is the question that started this.

> ⚠️ **A run shorter than ten seconds measures the client's send buffer
> draining, not the broker.** At these rates 200,000 records complete in about
> half a second, which yields `acks=1` at 386,100 against `acks=0` at 355,871 —
> acknowledging every batch on the leader's disk coming out *free, and faster
> than not acknowledging at all*. That is the tell. Over a proper window the same
> runs give 429,737 and 122,828, a 3.5× gap in the direction physics requires.
> The harness defaults to 5,000,000 records and prints run length beside every
> row, warning under ten seconds, so a too-short window is visible rather than
> silently becoming a number.

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

---

## The 10 GbE fabric: configured, and blocked by RP-10

The three Dells each have two 10 G ports. They were cabled and negotiating at
10000 Mbit/s but had no addresses, so nothing used them. Now addressed
(`172.16.10.31/32/33` on `eno1`, via `/etc/netplan/60-10gbe.yaml` rather than an
edit to the cloud-init file) and measured with `iperf3` at **9.14 Gbit/s**.
Kubernetes is untouched: `eno3` keeps the `192.168.1.x` addresses Calico and the
Thunderbird cluster are bound to.

It is worth using. With replication on the same 1 GbE port as client traffic, a
leader's link runs at **690–693 Mbit/s — 69% of line rate** at `acks=0` and
`acks=1` (measured below), and RF=3 means a leader's egress is twice its
ingress. Moving that off the client path is the single biggest lever left on
this hardware.

**It cannot be configured today.** `[[peers]].kafka` and `[advertise].kafka` look
independent but are not: the broker list in Metadata responses is built from the
peers list, so pointing peers at the 10 G subnet advertises it to *clients*, who
then cannot route to it. Recorded as RP-10 in `docs/ROADMAP_REPLICATION.md`.

### Baseline for the comparison, once RP-10 is fixed

Replication on 1 GbE, 256 B, RF=3, `min_insync_replicas=2`, median of 2 runs,
client on a separate machine:

| acks | throughput | p99 | node-1 NIC peak |
|---|---:|---:|---:|
| 0 | 104,937 msg/s | 8.52 ms | **693 Mbit/s — 69% of link** |
| 1 | 21,802 msg/s | 6.94 ms | **690 Mbit/s — 69%** |
| all | 11,499 msg/s | 8.99 ms | 54 Mbit/s — 5% |

`acks=0` and `acks=1` are pressed against the link; `acks=all` is not, which is
the same split Open Question 1 found and is why the 10 G is expected to help the
first two and do little for the third.

---

## The 10 GbE fabric: what moving replication off the client link buys

Each Dell has two 10 G ports. They were cabled and negotiating at 10000 Mbit/s
but unaddressed, so nothing used them. Now `172.16.10.31/32/33` on `eno1` via
`/etc/netplan/60-10gbe.yaml` (a separate file, not an edit to the cloud-init
one), measured with `iperf3` at **9.14 Gbit/s**. Kubernetes is untouched: `eno3`
keeps the `192.168.1.x` addresses Calico and Thunderbird are bound to.

Replication is pointed at it with the per-peer `replication` field added in
RP-10. Clients keep the `kafka` address. Same binary, same client, same machine
— the only difference is which network the followers fetch over.

| acks | replication on 1 GbE | on 10 GbE | change | node-1 NIC peak |
|---|---:|---:|---:|---|
| 0 | 104,937 msg/s | 106,690 msg/s | +1.7% | **693 → 131 Mbit/s** |
| 1 | 21,802 msg/s | **28,587 msg/s** | **+31%** | **690 → 46 Mbit/s** |
| all | 11,499 msg/s | 11,877 msg/s | +3.3% | 54 → 19 Mbit/s |

p99 at `acks=1` fell from 6.94 ms to **4.91 ms** (−29%).

**`acks=1` was genuinely transport-bound and is now not.** Its 31% gain is the
whole reason to have a separate fabric: at RF=3 a leader sends every record twice
more than it received it, so its egress was double its ingress and both shared
one 1 GbE port.

**`acks=0` was not transport-bound, despite looking like it.** It sat at 693
Mbit/s — 69% of line rate — which reads as "nearly saturated", and yet freeing
the link moved throughput by 1.7%. The 69% was replication traffic riding along,
not the client path straining. A high link utilisation is not by itself evidence
that the link is the constraint, and this is the measurement that shows it.

**`acks=all` barely moved (+3.3%)**, as expected: it was already at 5% of the
link, so it is bounded by the replication round trip rather than by bandwidth.
The same split Open Question 1 found.

The 1 GbE is now carrying 46–131 Mbit/s where it carried 690–693. The headroom
is real, but taking it needs load the current harness cannot generate — one
client on one 1 GbE link.

## Single-node produce ceiling, and why io_uring is off

Measured 2026-08-16 on the dev workstation (8-core/16-thread Ryzen, single
socket, NVMe). `chronik-bench`, 256 B payloads, 3 partitions, `acks=1`,
interleaved median-of-3. Bytes landed on disk reported alongside throughput.

| | msg/s | p50 |
|---|---:|---:|
| `acks=1`, 1024 producers | **228,874** | 3.42 ms |
| `acks=0`, 1024 producers | **513,260** | — |

Throughput scales close to linearly with producer count up to ~1024. Figures
taken at low concurrency (64 producers → ~13,500 msg/s) measure the load
generator, not the broker.

Durability at `acks=1` is verified, not assumed: 200,000 records acknowledged,
`SIGKILL` with no flush, restart on the same data directory — 200,000 unique
records recovered, zero lost, zero duplicates, across four runs.

### io_uring is disabled by default

`AsyncIoConfig::use_io_uring` has always defaulted to `false`. `GroupCommitWal`
now honours it; previously it gated only on the compile-time `async-io` feature,
which is on by default, so the io_uring path ran regardless. Set
`CHRONIK_WAL_IO_URING=true` to opt in.

It is off because it costs throughput here:

| | msg/s | p50 |
|---|---:|---:|
| io_uring | 152,851 | 5.30 ms |
| standard tokio file I/O | **228,874** | **3.42 ms** |

At a single producer the gap is 5.8× (194 msg/s at 3.99 ms against 1,132 at
0.69 ms).

**Why it does not pay.** io_uring's advantage is amortising I/O syscalls at high
IOPS. After group commit there are few left to amortise — at 229k msg/s the
broker issues 11,618 `fsync` per 10 s, i.e. **197 messages per fsync**, and I/O
syscalls (`write` + `fsync`) account for **6.3%** of syscall time. `futex` —
thread wake-ups and scheduling — is **92%**. Perfect io_uring batching would
collapse 42,818 I/O syscalls/s to ~1,161 and save ~0.65 core-seconds per second,
on a machine already running 16 cores at 14% utilisation. It optimises the
resource that is not scarce.

**The implementation's obvious defects were fixed, and they are not the cause.**
The command loop's blocking `crossbeam::recv_timeout` inside `tokio_uring`'s
runtime is now an async receive (`sched_yield` fell from 200,507 per 12 s to
2,099); writes run concurrently across partitions instead of one at a time in a
sequential loop; and `Bytes` is submitted through an `IoBuf` impl instead of a
`to_vec` copy per write. Throughput did not move — 150,493 msg/s against 233,319
standard at 1024 producers, 2,597 against 3,309 at 8.

The remaining gap is architectural. `tokio-uring` submits one operation per
`io_uring_enter` — 54,035 calls for ~31,000 messages — so it delivers no syscall
amortisation, while still paying a cross-thread channel round-trip per operation
that standard I/O does not. Its API exposes no linked SQEs, no registered
buffers, and no batched submission, so this cannot be fixed within the library.

**A version that could win** would drop `tokio-uring` for the raw `io-uring`
crate and hand-write the driver: an entire group-commit batch submitted as one
linked write→fsync, one ring per core rather than one global thread, registered
buffers, and no channel on the hot path. That is roughly 500-700 lines of new
queue- and buffer-lifetime management on the durability path. Worth it when the
workload is I/O-bound — cloud block storage at 125-250 MB/s rather than this
1.4 GB/s NVMe — and not before.
