# Cluster CLI Warning Added

**Date**: 2025-10-24
**Issue**: User confusion between `raft-cluster` (functional) and `cluster` (mock stub)
**Solution**: Added prominent warning message to prevent confusion

---

## What Was Done

Added a clear warning message to the `cluster` CLI that displays whenever any `cluster` command is used.

### File Modified

**[crates/chronik-server/src/cli/client.rs](../crates/chronik-server/src/cli/client.rs)** (lines 24-38)

### Change

```rust
// BEFORE (silent mock):
pub async fn connect(addr: &str) -> Result<Self> {
    // For now, just store the address
    // In Phase 5, this will establish a gRPC connection
    Ok(Self {
        addr: addr.to_string(),
        timeout: Duration::from_secs(10),
    })
}

// AFTER (with warning):
pub async fn connect(addr: &str) -> Result<Self> {
    // IMPORTANT: This is a mock implementation for development/testing only
    eprintln!("╔═══════════════════════════════════════════════════════════════════════╗");
    eprintln!("║  ⚠️  WARNING: Cluster CLI Returns MOCK DATA Only                      ║");
    eprintln!("╠═══════════════════════════════════════════════════════════════════════╣");
    eprintln!("║  This cluster management CLI is a stub implementation that returns    ║");
    eprintln!("║  hardcoded mock data. It does NOT connect to a real cluster.         ║");
    eprintln!("║                                                                       ║");
    eprintln!("║  For actual cluster management, use Kafka protocol tools:            ║");
    eprintln!("║    • kafka-topics --bootstrap-server localhost:9092 --list           ║");
    eprintln!("║    • kafka-topics --describe --topic <name>                          ║");
    eprintln!("║    • kafka-consumer-groups --list                                    ║");
    eprintln!("║                                                                       ║");
    eprintln!("║  This feature is planned for implementation in a future release.    ║");
    eprintln!("╚═══════════════════════════════════════════════════════════════════════╝");
    eprintln!();

    // For now, just store the address
    // TODO: Phase 5 - Implement actual gRPC connection and admin API
    Ok(Self {
        addr: addr.to_string(),
        timeout: Duration::from_secs(10),
    })
}
```

---

## Example Output

```bash
$ chronik-server cluster status
[2025-10-24T17:16:36.385883Z] INFO chronik_server: Chronik Server v1.3.65
[2025-10-24T17:16:36.385982Z] INFO chronik_server: Build features: search=true, backup=true

╔═══════════════════════════════════════════════════════════════════════╗
║  ⚠️  WARNING: Cluster CLI Returns MOCK DATA Only                      ║
╠═══════════════════════════════════════════════════════════════════════╣
║  This cluster management CLI is a stub implementation that returns    ║
║  hardcoded mock data. It does NOT connect to a real cluster.         ║
║                                                                       ║
║  For actual cluster management, use Kafka protocol tools:            ║
║    • kafka-topics --bootstrap-server localhost:9092 --list           ║
║    • kafka-topics --describe --topic <name>                          ║
║    • kafka-consumer-groups --list                                    ║
║                                                                       ║
║  This feature is planned for implementation in a future release.    ║
╚═══════════════════════════════════════════════════════════════════════╝

Cluster Status
==============
Nodes: 3
Healthy: 3
Unhealthy: 0
Metadata Leader: Node 1
...
```

---

## Why This Matters

### The Confusion

Users trying to work with Raft clustering might naturally try:
```bash
chronik-server cluster ...  # ❌ Wrong - this is a mock CLI
```

Instead of:
```bash
chronik-server raft-cluster ...  # ✅ Correct - this starts a real node
```

### The Problem

Without the warning:
1. User runs `cluster status`
2. Sees output that looks legitimate
3. Assumes it's querying a real cluster
4. Makes decisions based on **fake data**
5. Confusion and debugging time wasted

### The Solution

With the warning:
1. User runs `cluster status`
2. **Immediately sees prominent warning**
3. Understands this is mock data
4. Redirected to correct tools (kafka-topics)
5. No confusion, no wasted time

---

## Impact

### User Experience

**Before**:
- ❌ Silent mock data could mislead users
- ❌ No indication the CLI is non-functional
- ❌ Users might waste time debugging why data seems wrong

**After**:
- ✅ Impossible to miss the warning (box-drawn border)
- ✅ Clear explanation this is a stub
- ✅ Provides alternative tools to use
- ✅ Prevents wasted debugging time

### Development

**Benefits**:
- ✅ Shows intent for future implementation
- ✅ Preserves command structure for future use
- ✅ Tests can still run (warning goes to stderr)
- ✅ Low maintenance burden

**No Downsides**:
- ⚠️ Warning adds ~15 lines of output (acceptable for CLI tool)
- ⚠️ Warning can be suppressed by redirecting stderr if needed: `2>/dev/null`

---

## Testing

**Build**:
```bash
cargo build --release --bin chronik-server --features raft
# ✅ Success - no errors
```

**Run Test**:
```bash
$ chronik-server cluster status
# ✅ Warning displays prominently
# ✅ Mock data still displayed (for UI testing)
```

**Suppress Warning** (if needed):
```bash
$ chronik-server cluster status 2>/dev/null
# Only stdout (mock data) displayed, warning hidden
```

---

## Documentation

Created comprehensive documentation:

1. **[docs/CLUSTER_CLI_STATUS.md](./CLUSTER_CLI_STATUS.md)** - Full explanation of cluster CLI status
   - What it is
   - What it actually does
   - Why it exists
   - Alternatives for cluster management
   - When it will be implemented

2. **This file** - Summary of warning implementation

---

## Related Work

This change is **independent** of the Phase 1 Raft stability fixes:

| Change | Status | Purpose |
|--------|--------|---------|
| **Phase 1: Raft Config Fix** | ✅ Complete | Fix election churn in `raft-cluster` mode |
| **Cluster CLI Warning** | ✅ Complete | Prevent confusion about mock CLI |

Both changes improve user experience but address different issues.

---

## Future Work

**If cluster CLI is implemented** (post-v2.0.0):
1. Remove warning from `ClusterClient::connect()`
2. Implement actual gRPC client
3. Connect to real cluster admin API
4. Replace all mock methods with real RPC calls
5. Update [CLUSTER_CLI_STATUS.md](./CLUSTER_CLI_STATUS.md)

**Estimated Effort**: 40+ hours

**Priority**: P3 (Nice-to-have, not blocking)

---

## Checklist

- ✅ Warning message added to `client.rs`
- ✅ Build succeeded with no errors
- ✅ Warning displays correctly when running `cluster` commands
- ✅ Documentation created ([CLUSTER_CLI_STATUS.md](./CLUSTER_CLI_STATUS.md))
- ✅ Summary document created (this file)
- ✅ User confusion prevented

---

## Conclusion

The cluster CLI warning successfully prevents user confusion about the mock implementation while preserving the command structure for future development. Users are now clearly informed and redirected to appropriate tools.

**Status**: ✅ Complete and Working
