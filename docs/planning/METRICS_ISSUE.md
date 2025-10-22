# Prometheus Metrics Endpoint Issue

**Status**: 🔴 **BLOCKER** for Task 2.5 (Metrics Verification)
**Date Identified**: 2025-10-21
**Severity**: HIGH (blocks production monitoring)

---

## Problem Description

The Prometheus metrics endpoint (`/metrics`) hangs indefinitely when accessed via HTTP, while the `/health` endpoint works correctly.

### Symptoms

```bash
# Health endpoint works
$ curl http://localhost:9101/health
OK

# Metrics endpoint hangs
$ timeout 5 curl http://localhost:9101/metrics
# Hangs until timeout
```

### Ports Affected

- Node 1: http://localhost:9101/metrics
- Node 2: http://localhost:9102/metrics
- Node 3: http://localhost:9103/metrics

---

## Root Cause Analysis

### Server Status

✅ **Metrics server IS running** and accepting connections:
```
[INFO] chronik_monitoring::server: Metrics server listening on 0.0.0.0:9101
[INFO] chronik_server::raft_cluster: Metrics endpoint available at http://0.0.0.0:9101/metrics
```

✅ **Ports ARE listening**:
```bash
$ lsof -i :9101 -i :9102 -i :9103 | grep LISTEN
chronik-s 92966 ... TCP *:bacula-dir (LISTEN)   # Port 9101
chronik-s 92984 ... TCP *:bacula-fd (LISTEN)    # Port 9102
chronik-s 92997 ... TCP *:bacula-sd (LISTEN)    # Port 9103
```

✅ **HTTP server IS responding** (/health endpoint works)

❌ **Metrics gathering IS hanging**

### Technical Analysis

**File**: `crates/chronik-monitoring/src/server.rs:131-145`

```rust
async fn metrics_handler(
    State(registry): State<MetricsRegistry>,
) -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = registry.registry().gather();  // <-- HANGS HERE

    let mut buffer = Vec::new();
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => (StatusCode::OK, buffer),
        Err(e) => {
            tracing::error!("Failed to encode metrics: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Vec::new())
        }
    }
}
```

**Suspected Cause**: Deadlock in `registry.registry().gather()`

### Prometheus Deadlock Scenarios

Known Prometheus deadlock patterns:

1. **Mutex Deadlock**: If metrics are being updated while gathering
2. **Recursive Lock**: If gather() triggers metric updates that try to lock
3. **Arc/Mutex Contention**: If MetricsRegistry is being accessed from multiple threads

---

## Evidence

### Test Results

```bash
# Direct curl test
$ curl -v http://localhost:9101/health
< HTTP/1.1 200 OK
< content-type: text/plain; charset=utf-8
< content-length: 2
OK

$ timeout 3 curl http://localhost:9101/metrics
# Hangs - no response, times out after 3 seconds
```

### Server Logs

No errors or panics in logs:
```bash
$ grep -i "error\|panic\|fail" test-cluster-data/node1/node1.log
# No errors related to metrics server
```

---

## Impact

### Blocked Tasks

- ❌ **Task 2.5**: Metrics Verification (4h) - Cannot verify metrics
- ❌ **Task 2.6**: Prometheus Integration (4h) - Cannot scrape metrics
- ⚠️ **Production Monitoring**: No metrics available for Prometheus/Grafana

### Workarounds

1. **Use Health Endpoint**: Monitor via `/health` for basic availability
2. **Log-based Monitoring**: Parse server logs for metrics
3. **Alternative Metrics**: Use application-level metrics (not Prometheus)

---

## Proposed Solutions

### Option 1: Fix Deadlock (RECOMMENDED)

**Approach**: Identify and fix the Prometheus deadlock

**Steps**:
1. Add timeout to `gather()` call
2. Use `try_lock()` instead of `lock()` in metrics updates
3. Ensure no recursive locking in MetricsRegistry

**Estimated Effort**: 2-4 hours

**File to Modify**: `crates/chronik-monitoring/src/metrics.rs`

### Option 2: Use Simpler Metrics Endpoint

**Approach**: Create custom metrics endpoint that doesn't use `gather()`

**Steps**:
1. Store metrics in atomic counters/gauges
2. Manually serialize to Prometheus format
3. Bypass Prometheus registry entirely

**Estimated Effort**: 4-6 hours

**Trade-off**: Loses Prometheus compatibility, custom format

### Option 3: Use Different Metrics Library

**Approach**: Replace `prometheus` crate with `metrics` crate

**Steps**:
1. Replace `prometheus` with `metrics` + `metrics-exporter-prometheus`
2. Update all metric definitions
3. Test compatibility

**Estimated Effort**: 1 day

**Trade-off**: Large code change, potential compatibility issues

---

## Temporary Fix

For immediate testing, use health/ready endpoints:

```bash
# Check if server is alive
curl http://localhost:9101/health

# Check if server is ready
curl http://localhost:9101/ready
```

---

## Next Steps

### Immediate (Unblock Testing)

1. ✅ Document issue (this file)
2. ✅ Mark Task 2.5 as BLOCKED
3. ⏭️ Skip to Task 2.6/2.7/2.8 (non-metrics tasks)

### Short-term (Fix Issue)

1. Investigate Prometheus deadlock
2. Add logging/debugging to metrics_handler
3. Implement Option 1 (timeout + fix deadlock)
4. Test fix with cluster

### Long-term (Production)

1. Add comprehensive metrics test suite
2. Load test metrics endpoint
3. Monitor metrics gathering performance
4. Consider Option 3 if Prometheus issues persist

---

## References

- Prometheus crate: https://github.com/tikv/rust-prometheus
- Known deadlock issues: https://github.com/tikv/rust-prometheus/issues/353
- Axum async handlers: https://docs.rs/axum/latest/axum/

---

**Status**: OPEN - Needs investigation and fix
**Priority**: HIGH - Blocks production monitoring
**Assigned**: TBD
**Created**: 2025-10-21
