#!/bin/bash
#
# Chronik Stream File Organization Script
# Organizes root-level files into proper directories
# Uses regular 'mv' and 'rm' commands (NOT git commands)
# Files will be committed to git AFTER cleanup
#

set -e  # Exit on error

echo "======================================"
echo "Chronik File Organization Script"
echo "======================================"
echo ""
echo "This script will:"
echo "  - Move 92 historical docs to archive/"
echo "  - Move 48 test scripts to scripts/tests/raft/"
echo "  - Move 10 config files to config/examples/"
echo "  - Delete 8 obsolete/duplicate files"
echo ""
echo "⚠️  Using regular 'mv' commands (not git mv)"
echo "⚠️  You will commit changes to git AFTER cleanup"
echo ""
read -p "Continue? (yes/no): " confirm

if [ "$confirm" != "yes" ]; then
    echo "Aborted."
    exit 0
fi

echo ""
echo "Starting file organization..."
echo ""

# Navigate to project root
cd "$(dirname "$0")/.."

# Step 1: Create directories
echo "Step 1: Creating directory structure..."
mkdir -p archive/raft-implementation/{phases,analysis,features,evaluations,sessions,design}
mkdir -p config/examples/{cluster,stress,multi-dc}
mkdir -p scripts/tests/raft/{e2e,component,cluster,debug,benchmarks,utilities}
echo "✓ Directories created"
echo ""

# Step 2: Move Phase Completion Reports
echo "Step 2: Moving phase completion reports..."
mv PHASE*.md archive/raft-implementation/phases/ 2>/dev/null || true
echo "✓ Phase reports moved"
echo ""

# Step 3: Move Raft Implementation Docs
echo "Step 3: Moving Raft implementation docs..."
for file in RAFT_IMPLEMENTATION*.md RAFT_INTEGRATION*.md RAFT_NETWORK*.md \
            RAFT_TESTS*.md RAFT_CHAOS*.md; do
    [ -f "$file" ] && mv "$file" archive/raft-implementation/
done
echo "✓ Implementation docs moved"
echo ""

# Step 4: Move Analysis Documents
echo "Step 4: Moving analysis/troubleshooting docs..."
for file in RAFT_*ANALYSIS*.md RAFT_*FIX*.md RAFT_*ISSUE*.md \
            RAFT_ROOT_CAUSE*.md RAFT_BOOTSTRAP_DEADLOCK.md \
            RAFT_QUORUM*.md BROKER_REGISTRATION*.md \
            CLUSTER_STABILIZATION*.md COMPILATION_SUCCESS*.md \
            RAFT_PORT*.md RAFT_PROST*.md; do
    [ -f "$file" ] && mv "$file" archive/raft-implementation/analysis/
done
echo "✓ Analysis docs moved"
echo ""

# Step 5: Move Feature Reports
echo "Step 5: Moving feature implementation reports..."
for file in LEASE_*.md SNAPSHOT_*.md ISR_*.md \
            SEARCH_API*.md SELF_RELIANT*.md; do
    [ -f "$file" ] && mv "$file" archive/raft-implementation/features/
done
echo "✓ Feature reports moved"
echo ""

# Step 6: Move Session Summaries
echo "Step 6: Moving session summaries..."
for file in *SESSION*.md MIGRATION_TO_MAIN*.md \
            HONEST_TEST*.md PARALLEL_AGENTS*.md; do
    [ -f "$file" ] && mv "$file" archive/raft-implementation/sessions/
done
echo "✓ Session summaries moved"
echo ""

# Step 7: Move Evaluations
echo "Step 7: Moving evaluation documents..."
for file in CLUSTERING_EVALUATION*.md CLUSTERING_PROGRESS*.md \
            CLUSTERING_STATUS*.md OPENRAFT*.md raftify*.md \
            RAFT_LIBRARY*.md RAFT_DECISION*.md; do
    [ -f "$file" ] && mv "$file" archive/raft-implementation/evaluations/
done
echo "✓ Evaluation docs moved"
echo ""

# Step 8: Move Design Documents
echo "Step 8: Moving design documents..."
for file in FETCH_HANDLER*.md GOSSIP_VS*.md OPTION_C*.md; do
    [ -f "$file" ] && mv "$file" archive/raft-implementation/design/
done
echo "✓ Design docs moved"
echo ""

# Step 9: Move Remaining Raft/Cluster Docs
echo "Step 9: Moving remaining Raft/cluster docs..."
for file in RAFT_CLUSTER*.md RAFT_CLUSTERING*.md RAFT_FINAL*.md \
            RAFT_E2E*.md RAFT_VERIFICATION*.md RAFT_MISSING*.md \
            AUTOMATIC_BOOTSTRAP*.md BOOTSTRAP_RECOMMENDATION*.md \
            CLI_EXAMPLES*.md; do
    [ -f "$file" ] && mv "$file" archive/raft-implementation/
done
echo "✓ Remaining docs moved"
echo ""

# Step 10: Move Test Scripts - E2E
echo "Step 10a: Moving E2E test scripts..."
for file in test_raft_phase*.py test_raft_e2e*.py \
            test_raft_cluster_lifecycle*.py test_raft_cluster_stress*.py \
            test_raft_cluster_basic*.py test_raft_multi_partition*.py \
            test_raft_single_partition*.py; do
    [ -f "$file" ] && mv "$file" scripts/tests/raft/e2e/
done
echo "✓ E2E tests moved"

# Step 10b: Move Component Tests
echo "Step 10b: Moving component test scripts..."
for file in test_raft_leader*.py test_raft_bridge*.py \
            test_raft_prost*.py test_snapshot*.py \
            test_single_node_raft*.py test_flush*.py \
            test_segment*.py test_topic*.py \
            test_metadata*.py test_acks*.py; do
    [ -f "$file" ] && mv "$file" scripts/tests/raft/component/
done
echo "✓ Component tests moved"

# Step 10c: Move Cluster Tests
echo "Step 10c: Moving cluster test scripts..."
for file in test_3node*.* test_cluster*.* \
            test_healthcheck*.sh test_network*.sh; do
    [ -f "$file" ] && mv "$file" scripts/tests/raft/cluster/
done
echo "✓ Cluster tests moved"

# Step 10d: Move Debug Tests
echo "Step 10d: Moving debug/diagnostic scripts..."
for file in test_raft_debug*.sh test_raft_diagnostic*.sh \
            test_raft_simple*.sh test_raft_with_feature*.sh \
            test_raft_messaging*.sh test_raft_conf_change*.sh \
            test_raft_ports*.sh test_single_node_fix*.sh \
            test_broker_registration*.sh; do
    [ -f "$file" ] && mv "$file" scripts/tests/raft/debug/
done
echo "✓ Debug tests moved"

# Step 10e: Move Test Runners
echo "Step 10e: Moving test runner scripts..."
for file in start_cluster*.sh run_raft*.sh test_cli*.sh; do
    [ -f "$file" ] && mv "$file" scripts/tests/raft/
done
echo "✓ Test runners moved"

# Step 10f: Move Benchmarks
echo "Step 10f: Moving benchmark scripts..."
for file in benchmark*.py; do
    [ -f "$file" ] && mv "$file" scripts/tests/raft/benchmarks/
done
echo "✓ Benchmarks moved"

# Step 10g: Move Utilities
echo "Step 10g: Moving utility scripts..."
for file in copy_snapshot*.sh test_object_store*.sh; do
    [ -f "$file" ] && mv "$file" scripts/tests/raft/utilities/
done
echo "✓ Utilities moved"
echo ""

# Step 11: Move Config Files
echo "Step 11: Moving configuration files..."
for file in chronik-cluster*.toml; do
    [ -f "$file" ] && mv "$file" config/examples/cluster/
done
for file in chronik-stress*.toml; do
    [ -f "$file" ] && mv "$file" config/examples/stress/
done
for file in multi-dc*.toml; do
    [ -f "$file" ] && mv "$file" config/examples/multi-dc/
done
echo "✓ Config files moved"
echo ""

# Step 12: Delete Duplicates & Obsolete Files
echo "Step 12: Deleting duplicate and obsolete files..."

# Delete duplicate configs
for file in node1-cluster.toml node2-cluster.toml node3-cluster.toml; do
    if [ -f "$file" ]; then
        rm "$file"
        echo "  ✓ Deleted $file (duplicate)"
    fi
done

# Delete temporary/obsolete files
for file in TEST_IMPLEMENTATION_SUMMARY.txt single_partition_test_output.txt \
            fetch_handler_raft_integration.patch RAFT_PROTOC_FIX.patch \
            CLUSTERING_COMPLETION_PLAN.md; do
    if [ -f "$file" ]; then
        rm "$file"
        echo "  ✓ Deleted $file (obsolete)"
    fi
done
echo "✓ Obsolete files deleted"
echo ""

# Step 13: Create README Files
echo "Step 13: Creating README files..."

# Archive README
cat > archive/raft-implementation/README.md << 'EOF'
# Raft Implementation Archive

This directory contains historical documentation from the Raft clustering implementation (v2.0.0 development).

**Organization**:
- `phases/` - Phase completion reports (Phase 1-5)
- `analysis/` - Troubleshooting, bug fixes, root cause analysis
- `features/` - Feature implementation reports (ISR, snapshots, leases)
- `evaluations/` - Library comparisons, design decisions
- `sessions/` - Development session summaries
- `design/` - Design documents and architecture proposals

**Current Documentation**: See `/docs/RAFT_*.md` for official Raft documentation.

**Date Archived**: 2025-10-19
EOF

# Scripts README
cat > scripts/tests/raft/README.md << 'EOF'
# Raft Test Scripts

Python and Bash test scripts for Raft clustering functionality.

**Usage**:
```bash
# Run E2E tests
python3 scripts/tests/raft/e2e/test_raft_phase4_production_verified.py

# Run cluster tests
bash scripts/tests/raft/cluster/test_3node_cluster.sh

# Run benchmarks
python3 scripts/tests/raft/benchmarks/benchmark_raft_vs_standalone.py
```

**Organization**:
- `e2e/` - End-to-end tests (full cluster scenarios)
- `component/` - Component tests (individual features)
- `cluster/` - Multi-node cluster tests
- `debug/` - Debug and diagnostic utilities
- `benchmarks/` - Performance benchmarks
- `utilities/` - Helper scripts

**Note**: Most of these are historical test scripts. Modern tests are in `/tests/integration/raft_*.rs`.
EOF

# Config examples README
cat > config/examples/README.md << 'EOF'
# Chronik Configuration Examples

Example TOML configuration files for various deployment scenarios.

**Cluster Configs** (`cluster/`):
- `chronik-cluster.toml` - Basic 3-node cluster
- `chronik-cluster-node*.toml` - Per-node configurations

**Stress Test Configs** (`stress/`):
- Configurations for stress testing cluster deployments

**Multi-DC Configs** (`multi-dc/`):
- Multi-datacenter replication examples

**Usage**:
```bash
cargo run --features raft --bin chronik-server -- \
  --config config/examples/cluster/chronik-cluster.toml \
  standalone --raft
```

See `/docs/RAFT_CONFIGURATION_REFERENCE.md` for all configuration options.
EOF

echo "✓ README files created"
echo ""

# Final summary
echo "======================================"
echo "File Organization Complete!"
echo "======================================"
echo ""
echo "Summary of changes:"
echo "  - Created: archive/raft-implementation/ (with subdirs)"
echo "  - Created: config/examples/ (with subdirs)"
echo "  - Created: scripts/tests/raft/ (with subdirs)"
echo "  - Moved: ~140 files to organized locations"
echo "  - Deleted: 8 obsolete/duplicate files"
echo ""
echo "Root directory now contains only essential files."
echo ""
echo "Next steps:"
echo "  1. Verify root directory is clean: ls -la"
echo "  2. Check organized files: ls archive/ config/ scripts/tests/raft/"
echo "  3. Add new files to git: git add archive/ config/ scripts/"
echo "  4. Commit changes: git commit -m 'chore: Organize root directory files'"
echo ""
