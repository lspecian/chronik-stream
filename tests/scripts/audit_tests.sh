#!/usr/bin/env bash
# Test audit script - Find test debris to clean up
#
# Categorizes test files as:
# - KEEP: Canonical Rust tests
# - DELETE: Python debris, debug scripts, duplicates

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_DIR="$(cd "${TESTS_DIR}/.." && pwd)"

echo -e "${BLUE}======================================${NC}"
echo -e "${BLUE}Chronik Stream Test Audit${NC}"
echo -e "${BLUE}======================================${NC}"
echo ""

# Count files
rust_tests=$(find "${TESTS_DIR}/integration" -name "*.rs" 2>/dev/null | wc -l)
python_files=$(find "${TESTS_DIR}" -name "*.py" 2>/dev/null | wc -l)
shell_scripts=$(find "${TESTS_DIR}" -name "*.sh" 2>/dev/null | wc -l)

echo "File counts:"
echo "  Rust tests (canonical): ${rust_tests}"
echo "  Python files (debris):  ${python_files}"
echo "  Shell scripts:          ${shell_scripts}"
echo ""

# Canonical Rust tests (KEEP)
echo -e "${GREEN}KEEP - Canonical Rust Tests (${rust_tests} files)${NC}"
echo "--------------------------------------"
find "${TESTS_DIR}/integration" -name "*.rs" | sed 's|.*/tests/|  tests/|' | head -20
if [ $rust_tests -gt 20 ]; then
    echo "  ... and $((rust_tests - 20)) more"
fi
echo ""

# Python debris (DELETE)
echo -e "${RED}DELETE - Python Debris (${python_files} files)${NC}"
echo "--------------------------------------"
echo "All Python files should be deleted:"
find "${TESTS_DIR}" -name "*.py" | sed 's|.*/tests/|  tests/|' | head -30
if [ $python_files -gt 30 ]; then
    echo "  ... and $((python_files - 30)) more"
fi
echo ""

# Debug directories
debug_dirs=$(find "${TESTS_DIR}" -type d -name "*debug*" -o -name "librdkafka_debug" 2>/dev/null)
if [ -n "$debug_dirs" ]; then
    echo -e "${RED}DELETE - Debug Directories${NC}"
    echo "--------------------------------------"
    echo "$debug_dirs" | sed 's|.*/tests/|  tests/|'
    echo ""
fi

# Test output directories
test_dirs=$(find "${ROOT_DIR}" -maxdepth 1 -type d -name "test-*" 2>/dev/null)
if [ -n "$test_dirs" ]; then
    echo -e "${YELLOW}DELETE - Test Output Directories${NC}"
    echo "--------------------------------------"
    echo "$test_dirs" | sed "s|${ROOT_DIR}/|  |"
    echo ""
fi

# Summary
echo -e "${BLUE}======================================${NC}"
echo -e "${BLUE}Summary${NC}"
echo -e "${BLUE}======================================${NC}"
echo ""
echo -e "${GREEN}Canonical tests (KEEP):${NC} ${rust_tests} Rust test files in tests/integration/"
echo -e "${RED}Experimental debris (DELETE):${NC} ${python_files} Python files + debug directories"
echo ""
echo "Next steps:"
echo "  1. Review the list above"
echo "  2. Run: ./tests/scripts/cleanup_tests.sh --dry-run"
echo "  3. Run: ./tests/scripts/cleanup_tests.sh --execute"
