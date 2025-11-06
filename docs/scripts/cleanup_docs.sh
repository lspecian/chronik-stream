#!/usr/bin/env bash
# Documentation cleanup script - archive debug docs, delete status docs
#
# This script cleans up documentation by:
# - Archiving debug/investigation docs to docs/archive/
# - Deleting obsolete status/analysis docs
# - Keeping canonical user-facing documentation
#
# Usage:
#   ./docs/scripts/cleanup_docs.sh --dry-run    # Show what would be done
#   ./docs/scripts/cleanup_docs.sh --execute    # Actually execute

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

DOCS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DRY_RUN=true

if [ "${1:-}" = "--execute" ]; then
    DRY_RUN=false
fi

echo "======================================="
echo "Chronik Docs Cleanup"
echo "======================================="
echo ""

if [ "$DRY_RUN" = true ]; then
    echo -e "${YELLOW}DRY RUN - No files will be moved/deleted${NC}"
    echo "Run with --execute to actually clean up docs"
else
    echo -e "${RED}EXECUTING - Files will be moved/deleted${NC}"
    read -p "Are you sure? (yes/no): " confirm
    if [ "$confirm" != "yes" ]; then
        echo "Aborted"
        exit 1
    fi
fi
echo ""

moved_count=0
deleted_count=0

# Function to move file or show dry run
move_file() {
    local src=$1
    local dst_dir=$2
    local filename=$(basename "$src")

    if [ "$DRY_RUN" = true ]; then
        echo -e "  ${YELLOW}Would move:${NC} ${src/${DOCS_DIR}\//} → archive/${dst_dir}/"
    else
        mkdir -p "${DOCS_DIR}/archive/${dst_dir}"
        mv "$src" "${DOCS_DIR}/archive/${dst_dir}/"
        echo -e "  ${GREEN}Moved:${NC} ${filename} → archive/${dst_dir}/"
    fi
    moved_count=$((moved_count + 1))
}

# Function to delete file or show dry run
delete_file() {
    local file=$1
    local filename=$(basename "$file")

    if [ "$DRY_RUN" = true ]; then
        echo -e "  ${YELLOW}Would delete:${NC} ${file/${DOCS_DIR}\//}"
    else
        rm -f "$file"
        echo -e "  ${GREEN}Deleted:${NC} ${filename}"
    fi
    deleted_count=$((deleted_count + 1))
}

# 1. Archive consumer group debug docs
echo "Archiving consumer group debug docs..."
for file in "${DOCS_DIR}"/CONSUMER_GROUP*.md; do
    if [ -f "$file" ]; then
        move_file "$file" "consumer-group-debug"
    fi
done
echo ""

# 2. Archive partition distribution debug docs
echo "Archiving partition distribution debug docs..."
for file in "${DOCS_DIR}"/PARTITION_DISTRIBUTION*.md "${DOCS_DIR}"/MURMUR2*.md; do
    if [ -f "$file" ]; then
        move_file "$file" "partition-debug"
    fi
done
echo ""

# 3. Archive WAL V2 debug docs
echo "Archiving WAL V2 debug docs..."
for file in "${DOCS_DIR}"/WAL_V2*.md; do
    if [ -f "$file" ]; then
        move_file "$file" "wal-v2-debug"
    fi
done
echo ""

# 4. Archive Kafka Python debug docs
echo "Archiving Kafka Python debug docs..."
if [ -f "${DOCS_DIR}/KAFKA_PYTHON_REBALANCE_BUG.md" ]; then
    move_file "${DOCS_DIR}/KAFKA_PYTHON_REBALANCE_BUG.md" "kafka-python-debug"
fi
echo ""

# 5. Archive completed implementation plans
echo "Archiving completed implementation plans..."
for plan in CLUSTER_CONFIG_IMPROVEMENT_PLAN PHASE1_IMPLEMENTATION_PLAN PHASE_4_PLAN \
            PRIORITY2_TEST_PLAN SNAPSHOT_INTEGRATION_PLAN UNCOMMITTED_CHANGES_PLAN \
            WAL_REPLICATION_ROADMAP; do
    if [ -f "${DOCS_DIR}/${plan}.md" ]; then
        move_file "${DOCS_DIR}/${plan}.md" "implementation-plans"
    fi
done
echo ""

# 6. Delete obsolete status docs
echo "Deleting obsolete status/analysis docs..."
for status in CLUSTER_PLAN_STATUS_ANALYSIS CLUSTER_PLAN_STATUS_UPDATE \
              REPLICATION_TEST_RESULTS PHASE_3_CLUSTER_TEST_RESULTS \
              PHASE3_PARTITION_ASSIGNMENT_ANALYSIS PHASE_3_STATUS \
              V2.2_V2.3_PERFORMANCE_REGRESSION_ANALYSIS; do
    if [ -f "${DOCS_DIR}/${status}.md" ]; then
        delete_file "${DOCS_DIR}/${status}.md"
    fi
done
echo ""

# 7. Create archive README
if [ "$DRY_RUN" = false ]; then
    echo "Creating archive README..."
    cat > "${DOCS_DIR}/archive/README.md" << 'EOF'
# Archived Documentation

This directory contains historical documentation from debugging sessions and completed implementation plans.

**Why archived?**
- Debug docs: Useful for understanding past issues but not needed in active docs
- Implementation plans: Completed and merged into code
- Status docs: Information captured in git history and code

**Organization**:
- `consumer-group-debug/` - Consumer group rebalancing bug investigation (7 files)
- `partition-debug/` - Partition distribution bug investigation (3 files)
- `wal-v2-debug/` - WAL V2 deserialization bug investigation (2 files)
- `kafka-python-debug/` - kafka-python client compatibility issues (1 file)
- `implementation-plans/` - Completed implementation plans (7 files)

**Accessing**: Available in git history if needed for reference.

See [../DOCS_CLEANUP_PLAN.md](../DOCS_CLEANUP_PLAN.md) for cleanup rationale.
EOF
    echo -e "  ${GREEN}Created:${NC} archive/README.md"
    echo ""
fi

# Summary
echo "======================================="
echo "Summary"
echo "======================================="
echo ""
echo "Moved to archive: ${moved_count} files"
echo "Deleted: ${deleted_count} files"
echo ""

if [ "$DRY_RUN" = true ]; then
    echo -e "${YELLOW}This was a dry run. No files were changed.${NC}"
    echo ""
    echo "To actually clean up docs:"
    echo "  ./docs/scripts/cleanup_docs.sh --execute"
else
    echo -e "${GREEN}Cleanup complete!${NC}"
    echo ""
    canonical_count=$(ls -1 "${DOCS_DIR}"/*.md 2>/dev/null | wc -l)
    echo "Remaining canonical docs: ~${canonical_count} files"
    echo ""
    echo "Archive structure:"
    find "${DOCS_DIR}/archive" -name "*.md" | head -10 | sed 's|.*/docs/archive/|  archive/|'
    echo ""
    echo "Next steps:"
    echo "  1. Verify docs are organized: ls docs/*.md"
    echo "  2. Review archive: ls docs/archive/*/"
    echo "  3. Commit: git add docs/ && git commit -m 'docs: Clean up - archive debug docs'"
fi
