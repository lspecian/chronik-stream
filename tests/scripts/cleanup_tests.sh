#!/usr/bin/env bash
# Test cleanup script - Delete experimental Python debris
#
# This script removes ALL Python test files, debug directories,
# and test output directories, leaving only canonical Rust tests.
#
# Usage:
#   ./tests/scripts/cleanup_tests.sh --dry-run    # Show what would be deleted
#   ./tests/scripts/cleanup_tests.sh --execute    # Actually delete

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
DRY_RUN=true

if [ "$1" = "--execute" ]; then
    DRY_RUN=false
fi

echo "======================================="
echo "Chronik Stream Test Cleanup"
echo "======================================="
echo ""

if [ "$DRY_RUN" = true ]; then
    echo -e "${YELLOW}DRY RUN - No files will be deleted${NC}"
    echo "Run with --execute to actually delete files"
else
    echo -e "${RED}EXECUTING - Files will be permanently deleted${NC}"
    read -p "Are you sure? (yes/no): " confirm
    if [ "$confirm" != "yes" ]; then
        echo "Aborted"
        exit 1
    fi
fi
echo ""

deleted_count=0

# Function to delete file or show dry run
delete_file() {
    local file=$1
    if [ "$DRY_RUN" = true ]; then
        echo -e "  ${YELLOW}Would delete:${NC} ${file/${ROOT_DIR}\//}"
    else
        rm -f "$file"
        echo -e "  ${GREEN}Deleted:${NC} ${file/${ROOT_DIR}\//}"
    fi
    deleted_count=$((deleted_count + 1))
}

# Function to delete directory or show dry run
delete_dir() {
    local dir=$1
    if [ "$DRY_RUN" = true ]; then
        echo -e "  ${YELLOW}Would delete:${NC} ${dir/${ROOT_DIR}\//} (directory)"
    else
        rm -rf "$dir"
        echo -e "  ${GREEN}Deleted:${NC} ${dir/${ROOT_DIR}\//} (directory)"
    fi
    deleted_count=$((deleted_count + 1))
}

# 1. Delete ALL Python files
echo "Deleting Python files..."
while IFS= read -r -d '' file; do
    delete_file "$file"
done < <(find "${TESTS_DIR}" -name "*.py" -print0 2>/dev/null)
echo ""

# 2. Delete debug directories
echo "Deleting debug directories..."
for dir in $(find "${TESTS_DIR}" -type d -name "*debug*" -o -name "librdkafka_debug" 2>/dev/null); do
    delete_dir "$dir"
done
echo ""

# 3. Delete test output directories in root
echo "Deleting test output directories..."
for dir in $(find "${ROOT_DIR}" -maxdepth 1 -type d -name "test-*" 2>/dev/null); do
    delete_dir "$dir"
done
echo ""

# 4. Delete test output files in root (*.txt from test runs)
echo "Deleting test output files in root..."
for file in $(find "${ROOT_DIR}" -maxdepth 1 -name "*.txt" -o -name "consumer*.txt" -o -name "c[0-9]*.txt" 2>/dev/null); do
    # Only delete if it looks like test output
    if [[ "$file" =~ (consumer|node|test|c[0-9]) ]]; then
        delete_file "$file"
    fi
done
echo ""

# 5. Delete compat-tests directory (old compatibility framework)
if [ -d "${TESTS_DIR}/compat-tests" ]; then
    echo "Deleting compat-tests directory..."
    delete_dir "${TESTS_DIR}/compat-tests"
    echo ""
fi

# 6. Delete consumer-group directory (Python debris)
if [ -d "${TESTS_DIR}/consumer-group" ]; then
    echo "Deleting consumer-group directory..."
    delete_dir "${TESTS_DIR}/consumer-group"
    echo ""
fi

# 7. Delete python directory (all Python test debris)
if [ -d "${TESTS_DIR}/python" ]; then
    echo "Deleting python directory..."
    delete_dir "${TESTS_DIR}/python"
    echo ""
fi

# 8. Delete compatibility directory (Python debris)
if [ -d "${TESTS_DIR}/compatibility" ]; then
    echo "Deleting compatibility directory..."
    delete_dir "${TESTS_DIR}/compatibility"
    echo ""
fi

# 9. Clean up empty directories
echo "Cleaning empty directories..."
find "${TESTS_DIR}" -type d -empty -print0 2>/dev/null | while IFS= read -r -d '' dir; do
    if [ "$DRY_RUN" = false ]; then
        rmdir "$dir" 2>/dev/null || true
        echo -e "  ${GREEN}Removed empty:${NC} ${dir/${ROOT_DIR}\//}"
    fi
done
echo ""

# Summary
echo "======================================="
echo "Summary"
echo "======================================="
echo ""
echo "Total items processed: $deleted_count"
echo ""

if [ "$DRY_RUN" = true ]; then
    echo -e "${YELLOW}This was a dry run. No files were deleted.${NC}"
    echo ""
    echo "To actually delete these files:"
    echo "  ./tests/scripts/cleanup_tests.sh --execute"
else
    echo -e "${GREEN}Cleanup complete!${NC}"
    echo ""
    echo "Remaining canonical tests:"
    rust_count=$(find "${TESTS_DIR}/integration" -name "*.rs" 2>/dev/null | wc -l)
    echo "  ${rust_count} Rust integration tests in tests/integration/"
    echo ""
    echo "Next steps:"
    echo "  1. Run tests: cargo test --workspace --lib --bins"
    echo "  2. Verify nothing broke: cargo test --test integration"
    echo "  3. Commit cleanup: git add -u && git commit -m 'test: Remove Python test debris'"
fi
