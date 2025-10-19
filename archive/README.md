# Archive Directory

**Purpose**: Historical reference material from Chronik v2.0 Raft clustering implementation

---

## Why Keep This?

While these documents aren't user-facing documentation, they provide **valuable context** for:

### 1. Future Debugging
When similar issues arise, these troubleshooting notes provide:
- Root cause analysis of past bugs
- Solutions that worked (and didn't work)
- Performance bottlenecks and fixes

**Examples**:
- `analysis/RAFT_COMMIT_FIX_SUMMARY.md` - How we fixed Raft commit issues
- `analysis/RAFT_QUORUM_ANALYSIS.md` - Quorum problems and solutions
- `analysis/CLUSTER_STABILIZATION_COMPLETE.md` - Stability fixes

### 2. Design Decisions
Understanding **why** we made certain choices:
- Why raft-rs instead of openraft or other libraries?
- Why gossip protocol for bootstrap vs. deterministic?
- Why lease-based reads vs. read index?

**Examples**:
- `evaluations/RAFT_LIBRARY_COMPARISON.md` - Library evaluation matrix
- `design/GOSSIP_VS_DETERMINISTIC_ANALYSIS.md` - Bootstrap strategy analysis
- `evaluations/OPENRAFT_EVALUATION.md` - Why we didn't use openraft

### 3. Contributor Onboarding
New contributors can learn:
- How the system was built (phases 1-5)
- What challenges were encountered
- How problems were solved

**Examples**:
- `phases/PHASE1_COMPLETE.md` through `PHASE5_COMPLETE.md`
- Implementation journey from single-node to distributed cluster

### 4. Institutional Knowledge
Prevents "tribal knowledge" problem:
- Captures context that would otherwise be lost
- Explains non-obvious implementation details
- Documents failed experiments (so we don't repeat them)

---

## What's Inside

```
archive/
├── raft-implementation/           # Raft clustering implementation history
│   ├── phases/                    # Phase completion reports (Phase 1-5)
│   │   ├── PHASE1_COMPLETE.md
│   │   ├── PHASE2_COMPLETE.md
│   │   └── ... (28 files)
│   │
│   ├── analysis/                  # Troubleshooting & bug fixes
│   │   ├── RAFT_COMMIT_FIX_SUMMARY.md
│   │   ├── RAFT_QUORUM_ANALYSIS.md
│   │   ├── BROKER_REGISTRATION_FIX_SUMMARY.md
│   │   └── ... (29 files)
│   │
│   ├── features/                  # Feature implementation reports
│   │   ├── LEASE_IMPLEMENTATION_SUMMARY.md
│   │   ├── SNAPSHOT_IMPLEMENTATION_COMPLETE.md
│   │   ├── ISR_INTEGRATION_STATUS.md
│   │   └── ... (10 files)
│   │
│   ├── evaluations/               # Library & design evaluations
│   │   ├── RAFT_LIBRARY_COMPARISON.md
│   │   ├── OPENRAFT_EVALUATION.md
│   │   ├── CLUSTERING_EVALUATION.md
│   │   └── ... (10 files)
│   │
│   ├── design/                    # Design documents & proposals
│   │   ├── FETCH_HANDLER_RAFT_DESIGN.md
│   │   ├── GOSSIP_VS_DETERMINISTIC_ANALYSIS.md
│   │   └── ... (5 files)
│   │
│   ├── sessions/                  # Development session summaries
│   │   ├── FINAL_SESSION_SUMMARY.md
│   │   ├── INTEGRATION_SESSION_SUMMARY.md
│   │   └── ... (5 files)
│   │
│   └── *.md                       # Other implementation docs
│       ├── RAFT_IMPLEMENTATION_COMPLETE.md
│       ├── RAFT_CHAOS_TEST_REPORT.md
│       └── ... (rest)
│
├── test-logs/                     # Historical test logs (reference)
└── FILE_ORGANIZATION_PLAN.md      # How this directory was created
```

---

## When to Consult This Archive

### Debugging Scenarios
- **Raft commit hangs**: See `analysis/RAFT_COMMIT_FIX_SUMMARY.md`
- **Leader election issues**: See `analysis/RAFT_QUORUM_ANALYSIS.md`
- **Cluster instability**: See `analysis/CLUSTER_STABILIZATION_COMPLETE.md`
- **Prost/protobuf errors**: See `analysis/RAFT_PROST_BRIDGE_FIX.md`

### Design Questions
- **Why this Raft library?**: See `evaluations/RAFT_LIBRARY_COMPARISON.md`
- **Bootstrap strategy rationale**: See `design/GOSSIP_VS_DETERMINISTIC_ANALYSIS.md`
- **Fetch handler design**: See `design/FETCH_HANDLER_RAFT_DESIGN.md`

### Understanding Implementation
- **How was clustering added?**: Read phases in order (PHASE1 → PHASE5)
- **What features exist?**: Check `features/` directory
- **Test strategy**: See `*_TEST_PLAN.md` and `*_TEST_RESULTS.md` files

---

## Precedent in Open Source

Many successful projects maintain similar archives:

**Linux Kernel**:
- `/Documentation/historical/` - Old design docs and changelogs
- Provides context for decisions made 20+ years ago

**Rust Programming Language**:
- RFCs (Request for Comments) repository
- Historical proposals and discussions preserved

**PostgreSQL**:
- Mailing list archives going back decades
- Design discussions preserved for reference

**TiKV (uses raft-rs like us)**:
- Design proposals and implementation history
- Troubleshooting guides from production issues

---

## Maintenance Policy

### Keep
- ✅ All design decisions and evaluations
- ✅ Troubleshooting guides and bug fixes
- ✅ Performance analysis and optimizations
- ✅ Phase completion reports (implementation journey)

### Delete (if ever)
- ❌ Session summaries older than 2 years (low value)
- ❌ Duplicate information already in official docs

### Update
- 🔄 Add link in this README when referencing archived doc in current code
- 🔄 Create index of most-referenced documents (future)

---

## Current Documentation

**For users**: See `/docs/` directory
- `docs/RAFT_DEPLOYMENT_GUIDE.md` - How to deploy clusters
- `docs/RAFT_ARCHITECTURE.md` - Current architecture
- `docs/RAFT_TROUBLESHOOTING.md` - Common issues

**For contributors**: See `CONTRIBUTING.md` and `/docs/`

**Historical context**: You are here (`/archive/`)

---

## Final Thoughts

This archive represents **hundreds of hours** of implementation work, debugging, and design iteration. While not polished user documentation, it's **invaluable reference material** that:

1. **Saves time** when debugging similar issues
2. **Explains decisions** that might seem arbitrary otherwise
3. **Prevents rework** by documenting failed experiments
4. **Onboards contributors** faster by showing the journey

**Bottom line**: Keep this. It's organized, out of the way, and genuinely useful.

---

**Created**: 2025-10-19
**Maintained by**: Chronik development team
**Questions?**: See `/docs/` for current documentation
