# Chronik Stream: Bare Metal Performance

**Status: not measured.** This report has no numbers in it, on purpose.

> ### The previous report was deleted, not annotated
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
> read once, tables are cited forever. They are gone. What was still true —
> hardware, method, and the spin-loop bug found during the runs — is kept below.

---

## What is owed here

A re-measurement on the Dell cluster, with follower-pull replication running, of:

- Throughput and latency at `acks=0`, `acks=1` and `acks=all`, 256 B and 1 KB.
- The same across ingestor counts, to see whether the old "scales 12 → 36 pods"
  claim survives now that replication actually moves bytes.
- Consume throughput, which the old report never isolated from the HTTP pipeline.

Until then, the only valid figures are in `BASELINE_PERFORMANCE.md`, taken on a
single developer machine and labelled as such.

⚠️ One finding from those local runs should be settled **before** a bare-metal
run, or it will dominate the results: `acks=all` sustains only 2,300–4,000 msg/s
when each producer waits per message, against 15,000 for `acks=1`, and degrades
within a single run. Recorded in `docs/ROADMAP_REPLICATION.md`.

---

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

### Software Stack

| Component | Version |
|-----------|---------|
| Kubernetes | MicroK8s |
| Container Runtime | containerd |
| CNI | Calico |
| Container Registry | Harbor (self-hosted) |
| Load Generator | Grafana k6 (k6-operator) |

⚠️ **1 GbE is 125 MB/s per link.** With replication genuinely running, every
byte produced at RF=3 crosses the network twice more than it did in the old
runs. Whether the next measurement is network-bound or sender-bound is Open
Question 1 in the replication roadmap, and it should be answered *with NIC
utilisation sampled*, not inferred from throughput alone.

---

## Method (unchanged, still valid)

### Load Generator

- **Tool**: Grafana k6 via k6-operator on Kubernetes
- **Parallelism**: 8 runner pods, each running independent VU ramps
- **VU Profile**: Ramp 0 → 8000 virtual users over ~12 minutes, then ramp down
- **Batch Size**: 100–200 messages per HTTP request
- **Message Sizes**: 256 bytes and 1 KB

### Ingestor

- **Language**: Rust (chronik-perf crate)
- **Kafka Client**: rdkafka (librdkafka wrapper)
- **Pattern**: HTTP POST → JSON decode → Kafka produce → await ack → HTTP response
- **Scaling**: 12 to 36 replicas across 3 nodes

### Consumer

- **Language**: Rust (chronik-perf crate)
- **Kafka Client**: rdkafka
- **Pattern**: Kafka consumer group with partition assignment, metrics endpoint

### Reproducibility

```bash
./tests/k8s-perf/run-all.sh    # deploy the full test stack
./tests/k8s-perf/cleanup.sh    # tear it down
```

**Verify replication before trusting any number.** The old report's central
error was assuming `acks=all` implied replication. Check the bytes on disk on
each node, not `isr:` in a status response — `tests/cluster/regression_replication.sh`
is the pattern: produce, then count records in each node's log.

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
