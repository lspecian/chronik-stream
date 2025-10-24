# Known Issues - Chronik v2.0.0

**Last Updated**: 2025-10-22
**Version**: v2.0.0-rc.1

---

## Active Issues

None - all known issues have been resolved!

---

## Resolved Issues

### 1. Startup Race Condition (Cosmetic) - P3 ✅ FIXED in v1.3.66

**Severity**: Low (Cosmetic)
**Impact**: Error logs during first 5 seconds of cluster startup
**Status**: ✅ FIXED - Implemented startup grace period

**Description**:
During cluster startup, nodes logged "replica not found" errors for ~5 seconds before the `__meta` partition replica was created.

**Evidence** (Before Fix):
```
ERROR chronik_raft::rpc: Step: replica not found: Configuration error:
Replica not found for topic __meta partition 0
```

**Root Cause**:
The Raft gRPC server starts accepting connections before replicas are registered. Early Raft messages from peers arrive before replicas exist, resulting in "replica not found" errors.

**Solution** (v1.3.66):
Added 10-second startup grace period to `RaftServiceImpl`:
- During first 10 seconds: "replica not found" logged as `debug!` (hidden unless RUST_LOG=debug)
- After 10 seconds: logged as `error!` (indicates actual problem)

**Files Changed**:
- `crates/chronik-raft/src/rpc.rs` - Added startup grace period tracking

**Verification**:
```bash
# Before fix: 45+ error messages during startup
grep -i "error.*replica not found" logs/*.log | wc -l
45

# After fix: 0 error messages (all logged as debug during grace period)
grep -i "error.*replica not found" logs/*.log | wc -l
0
```

**Fixed By**: v1.3.66
**See Also**: [fixes/STARTUP_RACE_CONDITION_FIX.md](./fixes/STARTUP_RACE_CONDITION_FIX.md)

---

## Issue Tracking Process

**Severity Levels**:
- **P0 (Critical)**: Data loss, crashes, security vulnerabilities
- **P1 (High)**: Performance regression, major client incompatibility
- **P2 (Medium)**: Minor client incompatibility, UX issues
- **P3 (Low)**: Cosmetic issues, minor bugs, documentation

**Response Times**:
- P0: Fix within 24 hours, hotfix release
- P1: Fix in next release (1 week)
- P2: Fix in 1-2 releases
- P3: Fix when capacity allows

**Reporting Issues**:
- GitHub Issues: https://github.com/lspecian/chronik-stream/issues
- Include: Version, reproduction steps, logs, expected vs. actual behavior

---

**Document Owner**: Development Team
**Review Frequency**: Weekly during RC phase, monthly after GA

