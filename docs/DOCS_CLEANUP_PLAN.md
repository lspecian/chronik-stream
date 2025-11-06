# Documentation Cleanup Plan

**Date**: 2025-11-04
**Problem**: 100+ documentation files, many are debug/investigation docs that should be archived

---

## Current State

**Total docs**: ~100 files
**Categories**:
- ✅ **Canonical user docs** (15-20 files) - KEEP
- ❌ **Debug/investigation docs** (30+ files) - ARCHIVE
- ❌ **Status/analysis docs** (20+ files) - DELETE (info already in code/PRs)
- ⚠️ **Implementation plans** (10+ files) - ARCHIVE after completion

---

## Categorization

### ✅ KEEP - Canonical Documentation

**User-facing docs** (essential):
```
README.md
getting-started.md
DEPLOYMENT.md
DISASTER_RECOVERY.md
DOCKER.md
DOCKER_PUBLISH.md
HOW_TO_RUN_AND_TEST.md
TESTING_STANDARDS.md
TEST_CLEANUP_SUMMARY.md
```

**Technical reference** (essential):
```
ARCHITECTURE.md
RAFT_CONFIGURATION_REFERENCE.md
RAFT_DEPLOYMENT_GUIDE.md
ADMIN_API_SECURITY.md
kafka-client-compatibility.md
KSQL_INTEGRATION_GUIDE.md
```

**Feature docs** (keep):
```
COMPRESSION_SUPPORT.md
LAYERED_STORAGE_WITH_CLUSTERING.md
HYBRID_CLUSTERING_ARCHITECTURE.md
WAL_AUTO_TUNING.md
MIGRATION_v2.5.0.md
```

**Total to KEEP**: ~20 files

---

### 🗄️ ARCHIVE - Debug/Investigation Documents

**Consumer group debugging** (7 files):
```
CONSUMER_GROUP_BUG_INVESTIGATION.md
CONSUMER_GROUP_FINAL_BUG_ANALYSIS.md
CONSUMER_GROUP_PROTOCOL_BUG_STATUS.md
CONSUMER_GROUP_REBALANCE_FIX.md
CONSUMER_GROUP_REBALANCE_FIX_COMPLETE.md
CONSUMER_GROUP_SYNCGROUP_BUG_SUMMARY.md
CONSUMER_GROUP_BENCHMARK_RESULTS.md
```
**Action**: Move to `docs/archive/consumer-group-debug/`
**Reason**: Useful history but not needed in main docs

**Partition distribution debugging** (3 files):
```
PARTITION_DISTRIBUTION_BUG_ANALYSIS.md
PARTITION_DISTRIBUTION_BUG_ROOT_CAUSE.md
MURMUR2_PARTITIONING_ANALYSIS.md
```
**Action**: Move to `docs/archive/partition-debug/`

**WAL V2 debugging** (2 files):
```
WAL_V2_DESERIALIZATION_BUG.md
WAL_V2_FIX_PLAN.md
```
**Action**: Move to `docs/archive/wal-v2-debug/`

**Kafka Python rebalance bug**:
```
KAFKA_PYTHON_REBALANCE_BUG.md
```
**Action**: Move to `docs/archive/kafka-python-debug/`

**Total to ARCHIVE**: ~15 debug docs

---

### 🗑️ DELETE - Status/Analysis Documents

**Cluster plan status** (merged into code):
```
CLUSTER_PLAN_STATUS_ANALYSIS.md
CLUSTER_PLAN_STATUS_UPDATE.md
```
**Reason**: Status docs for completed work, info in git history

**Test results** (obsolete):
```
REPLICATION_TEST_RESULTS.md
PHASE_3_CLUSTER_TEST_RESULTS.md
PHASE3_PARTITION_ASSIGNMENT_ANALYSIS.md
PHASE_3_STATUS.md
```
**Reason**: Old test results, not relevant anymore

**Performance analysis** (obsolete):
```
V2.2_V2.3_PERFORMANCE_REGRESSION_ANALYSIS.md
```
**Reason**: Old version analysis, issue resolved

**Total to DELETE**: ~10 status docs

---

### 🗄️ ARCHIVE - Completed Implementation Plans

**Move to** `docs/archive/implementation-plans/`:
```
CLUSTER_CONFIG_IMPROVEMENT_PLAN.md     # Completed in v2.5.0
PHASE1_IMPLEMENTATION_PLAN.md          # Completed
PHASE_4_PLAN.md                        # Completed (node removal)
PRIORITY2_TEST_PLAN.md                 # Completed
SNAPSHOT_INTEGRATION_PLAN.md           # Completed
UNCOMMITTED_CHANGES_PLAN.md            # Completed or obsolete
WAL_REPLICATION_ROADMAP.md             # Partially completed
```

**KEEP active plans**:
```
ROADMAP_v2.x.md                        # Active roadmap
```

**Total to ARCHIVE**: ~8 plans

---

## Cleanup Actions

### 1. Create Archive Structure

```bash
mkdir -p docs/archive/{consumer-group-debug,partition-debug,wal-v2-debug,kafka-python-debug,implementation-plans}
```

### 2. Move Debug Docs to Archive

```bash
# Consumer group
mv docs/CONSUMER_GROUP*.md docs/archive/consumer-group-debug/

# Partition distribution
mv docs/PARTITION_DISTRIBUTION*.md docs/MURMUR2*.md docs/archive/partition-debug/

# WAL V2
mv docs/WAL_V2*.md docs/archive/wal-v2-debug/

# Kafka Python
mv docs/KAFKA_PYTHON_REBALANCE_BUG.md docs/archive/kafka-python-debug/

# Implementation plans
mv docs/CLUSTER_CONFIG_IMPROVEMENT_PLAN.md docs/archive/implementation-plans/
mv docs/PHASE1_IMPLEMENTATION_PLAN.md docs/archive/implementation-plans/
mv docs/PHASE_4_PLAN.md docs/archive/implementation-plans/
mv docs/PRIORITY2_TEST_PLAN.md docs/archive/implementation-plans/
mv docs/SNAPSHOT_INTEGRATION_PLAN.md docs/archive/implementation-plans/
mv docs/UNCOMMITTED_CHANGES_PLAN.md docs/archive/implementation-plans/
mv docs/WAL_REPLICATION_ROADMAP.md docs/archive/implementation-plans/
```

### 3. Delete Status Docs

```bash
rm -f docs/CLUSTER_PLAN_STATUS_ANALYSIS.md
rm -f docs/CLUSTER_PLAN_STATUS_UPDATE.md
rm -f docs/REPLICATION_TEST_RESULTS.md
rm -f docs/PHASE_3_CLUSTER_TEST_RESULTS.md
rm -f docs/PHASE3_PARTITION_ASSIGNMENT_ANALYSIS.md
rm -f docs/PHASE_3_STATUS.md
rm -f docs/V2.2_V2.3_PERFORMANCE_REGRESSION_ANALYSIS.md
```

### 4. Create Archive README

```bash
cat > docs/archive/README.md << 'EOF'
# Archived Documentation

This directory contains historical documentation from debugging sessions and completed implementation plans.

**Why archived?**
- Debug docs: Useful for understanding past issues but not needed in active docs
- Implementation plans: Completed and merged into code
- Status docs: Information captured in git history and code

**Organization**:
- `consumer-group-debug/` - Consumer group rebalancing bug investigation
- `partition-debug/` - Partition distribution bug investigation
- `wal-v2-debug/` - WAL V2 deserialization bug investigation
- `kafka-python-debug/` - kafka-python client compatibility issues
- `implementation-plans/` - Completed implementation plans

**Accessing**: Available in git history if needed for reference.
EOF
```

---

## Expected Result

### Before
```
docs/
├── 100+ files
├── Mix of canonical, debug, status, plans
└── Hard to find relevant documentation
```

### After
```
docs/
├── README.md (index of all docs)
├── getting-started.md
├── DEPLOYMENT.md
├── DISASTER_RECOVERY.md
├── ARCHITECTURE.md
├── TESTING_STANDARDS.md
├── ... (~20 canonical docs)
│
├── archive/              # Historical docs
│   ├── README.md
│   ├── consumer-group-debug/
│   ├── partition-debug/
│   ├── wal-v2-debug/
│   └── implementation-plans/
│
├── raft/                 # Raft-specific docs
├── releases/             # Release notes
└── ... (other directories)
```

---

## Automation Script

Create `docs/scripts/cleanup_docs.sh`:

```bash
#!/bin/bash
# Clean up documentation - archive debug docs, delete status docs

set -euo pipefail

echo "Creating archive directories..."
mkdir -p docs/archive/{consumer-group-debug,partition-debug,wal-v2-debug,kafka-python-debug,implementation-plans}

echo "Archiving consumer group debug docs..."
mv docs/CONSUMER_GROUP*.md docs/archive/consumer-group-debug/ 2>/dev/null || true

echo "Archiving partition debug docs..."
mv docs/PARTITION_DISTRIBUTION*.md docs/MURMUR2*.md docs/archive/partition-debug/ 2>/dev/null || true

echo "Archiving WAL V2 debug docs..."
mv docs/WAL_V2*.md docs/archive/wal-v2-debug/ 2>/dev/null || true

echo "Archiving Kafka Python debug docs..."
mv docs/KAFKA_PYTHON_REBALANCE_BUG.md docs/archive/kafka-python-debug/ 2>/dev/null || true

echo "Archiving implementation plans..."
for file in CLUSTER_CONFIG_IMPROVEMENT_PLAN PHASE1_IMPLEMENTATION_PLAN PHASE_4_PLAN \
            PRIORITY2_TEST_PLAN SNAPSHOT_INTEGRATION_PLAN UNCOMMITTED_CHANGES_PLAN \
            WAL_REPLICATION_ROADMAP; do
    mv "docs/${file}.md" docs/archive/implementation-plans/ 2>/dev/null || true
done

echo "Deleting status docs..."
rm -f docs/CLUSTER_PLAN_STATUS_ANALYSIS.md
rm -f docs/CLUSTER_PLAN_STATUS_UPDATE.md
rm -f docs/REPLICATION_TEST_RESULTS.md
rm -f docs/PHASE_3_CLUSTER_TEST_RESULTS.md
rm -f docs/PHASE3_PARTITION_ASSIGNMENT_ANALYSIS.md
rm -f docs/PHASE_3_STATUS.md
rm -f docs/V2.2_V2.3_PERFORMANCE_REGRESSION_ANALYSIS.md

echo "Creating archive README..."
cat > docs/archive/README.md << 'EOF'
# Archived Documentation

Historical documentation from debugging sessions and completed implementation plans.
See individual subdirectories for specific investigations.
EOF

echo "✓ Documentation cleanup complete"
echo ""
echo "Summary:"
echo "  - Archived: ~25 debug and plan docs"
echo "  - Deleted: ~10 status docs"
echo "  - Remaining: ~20 canonical docs"
```

---

## Commit Message

```
docs: Clean up documentation - archive debug docs

- Archive consumer group debug docs (7 files) → archive/consumer-group-debug/
- Archive partition distribution debug (3 files) → archive/partition-debug/
- Archive WAL V2 debug docs (2 files) → archive/wal-v2-debug/
- Archive completed implementation plans (8 files) → archive/implementation-plans/
- Delete obsolete status/analysis docs (10 files)

Remaining: ~20 canonical documentation files
Archived: ~25 historical investigation docs
Deleted: ~10 obsolete status docs

See docs/DOCS_CLEANUP_PLAN.md for rationale.
```

---

## Going Forward

### Documentation Standards

**DO create**:
- ✅ User-facing guides (getting started, deployment, etc.)
- ✅ Technical reference (architecture, configuration, APIs)
- ✅ Feature documentation (how to use specific features)

**DON'T commit**:
- ❌ Debug investigation docs (use issue comments instead)
- ❌ Status documents (info should be in PRs/commits)
- ❌ Analysis documents (summarize in issue/PR, don't create doc)

**Temporary docs OK** (but archive after):
- ⚠️ Implementation plans (archive when completed)
- ⚠️ Bug investigation (move to archive/ when fixed)

### Weekly Maintenance

```bash
# Check for debug docs
ls docs/*BUG*.md docs/*DEBUG*.md docs/*ANALYSIS*.md 2>/dev/null

# If found, move to archive
```

---

## See Also

- [TEST_CLEANUP_SUMMARY.md](./TEST_CLEANUP_SUMMARY.md) - Test suite cleanup (similar problem)
- [TESTING_STANDARDS.md](./TESTING_STANDARDS.md) - Testing standards going forward
