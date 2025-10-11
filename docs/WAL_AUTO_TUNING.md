# WAL Auto-Tuning Guide

## Overview

Chronik Stream v1.3.54+ includes intelligent auto-tuning for the GroupCommitWal system that automatically configures performance parameters based on available system resources.

## How It Works

The WAL automatically detects:
- **CPU limits** (including Kubernetes/Docker cgroup constraints)
- **Memory limits** (including container memory limits)
- **Environment overrides** for manual control

Based on these constraints, it selects the optimal performance profile.

## Resource Profiles

### Low Resource Profile
**Target:** Small containers, VMs with limited resources
**Constraints:** ≤1 CPU, <512MB RAM

| Parameter | Value | Description |
|-----------|-------|-------------|
| `max_batch_size` | 500 | Writes per batch |
| `max_batch_bytes` | 5 MB | Maximum batch size |
| `max_wait_time_ms` | 20 ms | Batching window |
| `max_queue_depth` | 2,500 | Pending writes limit |

**Use Cases:**
- Kubernetes pods with `limits.cpu: 0.5-1` and `limits.memory: 256Mi-512Mi`
- Small Docker containers
- Development environments
- Edge deployments

### Medium Resource Profile
**Target:** Typical application servers
**Constraints:** 2-4 CPUs, 512MB-4GB RAM

| Parameter | Value | Description |
|-----------|-------|-------------|
| `max_batch_size` | 2,000 | Writes per batch |
| `max_batch_bytes` | 15 MB | Maximum batch size |
| `max_wait_time_ms` | 10 ms | Batching window |
| `max_queue_depth` | 10,000 | Pending writes limit |

**Use Cases:**
- Kubernetes pods with `limits.cpu: 2-4` and `limits.memory: 1Gi-4Gi`
- Standard Docker deployments
- Multi-tenant environments
- Moderate traffic loads

### High Resource Profile
**Target:** Dedicated servers, high-performance deployments
**Constraints:** 4-16 CPUs, 4-16GB RAM

| Parameter | Value | Description |
|-----------|-------|-------------|
| `max_batch_size` | 5,000 | Writes per batch |
| `max_batch_bytes` | 25 MB | Maximum batch size |
| `max_wait_time_ms` | 5 ms | Batching window (low-latency) |
| `max_queue_depth` | 25,000 | Pending writes limit |

**Use Cases:**
- Bare metal servers
- Large Kubernetes pods with generous limits
- High-throughput production workloads
- Real-time streaming applications

### Ultra Resource Profile
**Target:** Maximum throughput deployments (latency-tolerant)
**Constraints:** 16+ CPUs, 16GB+ RAM

| Parameter | Value | Description |
|-----------|-------|-------------|
| `max_batch_size` | 20,000 | Writes per batch (4x high) |
| `max_batch_bytes` | 100 MB | Maximum batch size (4x high) |
| `max_wait_time_ms` | 100 ms | Batching window (maximum batching) |
| `max_queue_depth` | 100,000 | Pending writes limit (4x high) |

**Use Cases:**
- High-throughput batch processing (where 100ms latency is acceptable)
- Data ingestion pipelines with bulk writes
- Analytics workloads with large batches
- Scenarios where throughput >> latency

**Trade-offs:**
- ✅ Potentially 2-3x higher throughput vs high profile
- ❌ ~100ms p99 produce latency (vs 5ms in high profile)
- ⚠️ Requires significant memory (100MB+ per partition)

## Environment Variable Override

You can manually select a profile using the `CHRONIK_WAL_PROFILE` environment variable:

```bash
# Force low-resource profile (conservative)
export CHRONIK_WAL_PROFILE=low
./chronik-server

# Force medium-resource profile (balanced)
export CHRONIK_WAL_PROFILE=medium
./chronik-server

# Force high-resource profile (aggressive)
export CHRONIK_WAL_PROFILE=high
./chronik-server

# Force ultra-resource profile (maximum throughput)
export CHRONIK_WAL_PROFILE=ultra
./chronik-server
```

**Aliases:**
- `low` = `small` = `container`
- `medium` = `balanced`
- `high` = `aggressive` = `dedicated`
- `ultra` = `maximum` = `throughput`

## Kubernetes Configuration Examples

### Small Pod (Low Profile)
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: chronik-small
spec:
  containers:
  - name: chronik
    image: ghcr.io/lspecian/chronik-stream:latest
    resources:
      requests:
        cpu: "0.5"
        memory: "256Mi"
      limits:
        cpu: "1"
        memory: "512Mi"
    # Auto-detects: Low profile (500 batch, 5MB, 20ms)
```

### Medium Pod (Medium Profile)
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: chronik-medium
spec:
  containers:
  - name: chronik
    image: ghcr.io/lspecian/chronik-stream:latest
    resources:
      requests:
        cpu: "2"
        memory: "1Gi"
      limits:
        cpu: "4"
        memory: "4Gi"
    # Auto-detects: Medium profile (2K batch, 15MB, 10ms)
```

### Large Pod (High Profile)
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: chronik-large
spec:
  containers:
  - name: chronik
    image: ghcr.io/lspecian/chronik-stream:latest
    resources:
      requests:
        cpu: "4"
        memory: "4Gi"
      limits:
        cpu: "8"
        memory: "8Gi"
    # Auto-detects: High profile (5K batch, 25MB, 5ms)
```

### Ultra-Large Pod (Ultra Profile)
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: chronik-ultra
spec:
  containers:
  - name: chronik
    image: ghcr.io/lspecian/chronik-stream:latest
    resources:
      requests:
        cpu: "16"
        memory: "16Gi"
      limits:
        cpu: "32"
        memory: "32Gi"
    # Auto-detects: Ultra profile (20K batch, 100MB, 100ms)
    # Best for: High-throughput batch ingestion where latency is acceptable
```

### Override Auto-Detection
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: chronik-override
spec:
  containers:
  - name: chronik
    image: ghcr.io/lspecian/chronik-stream:latest
    env:
    - name: CHRONIK_WAL_PROFILE
      value: "high"  # Force high-performance config
    resources:
      limits:
        cpu: "8"
        memory: "8Gi"
```

## Docker Configuration Examples

### Small Container
```bash
docker run -d \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  --cpus="1" \
  --memory="512m" \
  ghcr.io/lspecian/chronik-stream:latest
# Auto-detects: Low profile
```

### Medium Container
```bash
docker run -d \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  --cpus="4" \
  --memory="4g" \
  ghcr.io/lspecian/chronik-stream:latest
# Auto-detects: Medium profile
```

### Large Container with Override
```bash
docker run -d \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  -e CHRONIK_WAL_PROFILE=high \
  --cpus="8" \
  --memory="8g" \
  ghcr.io/lspecian/chronik-stream:latest
# Forces: High profile
```

### Ultra-Large Container (Maximum Throughput)
```bash
docker run -d \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  --cpus="32" \
  --memory="32g" \
  ghcr.io/lspecian/chronik-stream:latest
# Auto-detects: Ultra profile (20K batch, 100MB, 100ms)
# Best for batch ingestion where latency tolerance is high
```

## How Detection Works

### 1. Environment Variable Check
First checks for `CHRONIK_WAL_PROFILE` environment variable. If set, uses that profile immediately.

### 2. cgroup v2 Detection (Modern K8s/Docker)
Reads from:
- `/sys/fs/cgroup/cpu.max` - CPU quota/period
- `/sys/fs/cgroup/memory.max` - Memory limit

**Example:**
```bash
# 2 CPUs allocated
$ cat /sys/fs/cgroup/cpu.max
200000 100000

# 4GB memory limit
$ cat /sys/fs/cgroup/memory.max
4294967296
```

### 3. cgroup v1 Detection (Legacy K8s/Docker)
Reads from:
- `/sys/fs/cgroup/cpu/cpu.cfs_quota_us` - CPU quota
- `/sys/fs/cgroup/cpu/cpu.cfs_period_us` - CPU period
- `/sys/fs/cgroup/memory/memory.limit_in_bytes` - Memory limit

### 4. Fallback Detection
If cgroups not available (bare metal, macOS, Windows):
- Uses `std::thread::available_parallelism()` for CPU count
- Defaults to conservative 2GB memory assumption
- Usually results in Medium or High profile for bare metal

### 5. Profile Selection
Chooses profile based on **BOTH** CPU and memory constraints:
- Uses the **most conservative** constraint
- Prevents OOM kills or CPU throttling

**Example:**
```
Detected: 8 CPUs (High tier), 1GB RAM (Medium tier)
Selected: Medium profile (more conservative)
```

## Monitoring Auto-Detection

Check server logs on startup to see which profile was selected:

```bash
# Start server
./chronik-server

# Look for this log line:
# INFO chronik_wal::manager: GroupCommitWal configured:
#   batch_size=2000, batch_MB=15, wait_ms=10, queue_depth=10000
#   (use CHRONIK_WAL_PROFILE=low/medium/high to override)
```

## Performance Benchmarks

### Low Profile (Container: 1 CPU, 512MB RAM)
- **Throughput:** ~15K msgs/sec
- **Latency:** p99 < 50ms
- **Memory:** ~50MB peak

### Medium Profile (Server: 4 CPUs, 4GB RAM)
- **Throughput:** ~30K msgs/sec
- **Latency:** p99 < 20ms
- **Memory:** ~150MB peak

### High Profile (Bare Metal: 8+ CPUs, 8GB+ RAM)
- **Throughput:** ~60K+ msgs/sec
- **Latency:** p99 < 10ms
- **Memory:** ~250MB peak

## Troubleshooting

### Q: My pod has 8 CPUs but uses Low profile?
**A:** Check memory limits. The WAL uses the most conservative constraint.
```bash
kubectl describe pod <pod-name> | grep -A2 Limits
```

### Q: How do I force a specific profile?
**A:** Use `CHRONIK_WAL_PROFILE` environment variable:
```yaml
env:
- name: CHRONIK_WAL_PROFILE
  value: "high"
```

### Q: Can I customize individual parameters?
**A:** Not via environment variables. You would need to modify `GroupCommitConfig::custom()` in code.

### Q: Does this work on Windows?
**A:** Yes, but without cgroup detection. Falls back to CPU count and conservative memory defaults.

### Q: What if I'm running in Docker Desktop on Mac?
**A:** Docker Desktop on Mac doesn't expose cgroups properly. Use `CHRONIK_WAL_PROFILE` to override.

## Best Practices

1. **Let auto-detection work** - It handles most cases correctly
2. **Set explicit limits** - Define CPU and memory limits in K8s/Docker
3. **Monitor startup logs** - Verify the selected profile matches expectations
4. **Override sparingly** - Only use `CHRONIK_WAL_PROFILE` when auto-detection fails
5. **Test with load** - Validate performance under real workloads

## Technical Details

### Why Both CPU and Memory?

**CPU-bound scenario:**
- 8 CPUs, 512MB RAM
- Without memory check: High profile (25MB batches)
- Result: OOM kills ❌

**Memory-bound scenario:**
- 1 CPU, 8GB RAM
- Without CPU check: High profile (5ms latency)
- Result: CPU thrashing ❌

**Solution:**
- Check both constraints
- Use most conservative
- Ensures stability ✅

### cgroup v1 vs v2

| Feature | cgroup v1 | cgroup v2 |
|---------|-----------|-----------|
| **K8s Version** | < 1.25 | ≥ 1.25 |
| **Docker** | Older | Recent |
| **CPU Path** | `/sys/fs/cgroup/cpu/cpu.cfs_*` | `/sys/fs/cgroup/cpu.max` |
| **Memory Path** | `/sys/fs/cgroup/memory/memory.limit_in_bytes` | `/sys/fs/cgroup/memory.max` |
| **Detection** | Supported ✅ | Supported ✅ |

Chronik detects both automatically, trying v2 first then falling back to v1.

## See Also

- [WAL Architecture](./wal/architecture.md)
- [Performance Tuning](./wal/performance.md)
- [Kubernetes Deployment](./deployment/kubernetes.md)
