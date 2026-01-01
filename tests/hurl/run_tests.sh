#!/bin/bash
# Hurl API Test Runner for Chronik Stream
#
# Usage:
#   ./run_tests.sh              # Run all tests
#   ./run_tests.sh health       # Run specific test file
#   ./run_tests.sh --verbose    # Run with verbose output
#   ./run_tests.sh --report     # Generate HTML report

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Default settings
VERBOSE=""
REPORT=""
API_KEY="${CHRONIK_ADMIN_API_KEY:-}"
SPECIFIC_TEST=""

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --verbose|-v)
            VERBOSE="--very-verbose"
            shift
            ;;
        --report|-r)
            REPORT="--report-html $SCRIPT_DIR/report.html"
            shift
            ;;
        --api-key)
            API_KEY="$2"
            shift 2
            ;;
        --help|-h)
            echo "Usage: $0 [options] [test_name]"
            echo ""
            echo "Options:"
            echo "  --verbose, -v    Enable verbose output"
            echo "  --report, -r     Generate HTML report"
            echo "  --api-key KEY    Set admin API key"
            echo "  --help, -h       Show this help"
            echo ""
            echo "Test names:"
            echo "  health           Health check tests"
            echo "  sql              SQL API tests"
            echo "  search           Search API tests"
            echo "  schema           Schema Registry tests"
            echo "  admin            Admin API tests"
            echo "  vector           Vector Search tests"
            echo "  integration      Integration workflow tests"
            echo "  all              Run all tests (default)"
            exit 0
            ;;
        *)
            SPECIFIC_TEST="$1"
            shift
            ;;
    esac
done

# Check if hurl is installed
if ! command -v hurl &> /dev/null; then
    echo -e "${RED}Error: hurl is not installed${NC}"
    echo "Install with: cargo install hurl"
    echo "Or see: https://hurl.dev/docs/installation.html"
    exit 1
fi

# Check if cluster is running
check_cluster() {
    echo -e "${BLUE}Checking cluster status...${NC}"
    if ! curl -s http://localhost:6092/health > /dev/null 2>&1; then
        echo -e "${RED}Error: Cluster does not appear to be running${NC}"
        echo "Start the cluster with: ./tests/cluster/start.sh"
        exit 1
    fi
    echo -e "${GREEN}✓ Cluster is running${NC}"
}

# Map test name to file
get_test_file() {
    case $1 in
        health)     echo "01_health.hurl" ;;
        sql)        echo "02_sql_api.hurl" ;;
        search)     echo "03_search_api.hurl" ;;
        schema)     echo "04_schema_registry.hurl" ;;
        admin)      echo "05_admin_api.hurl" ;;
        vector)     echo "06_vector_search.hurl" ;;
        integration) echo "07_integration_workflow.hurl" ;;
        *)          echo "" ;;
    esac
}

# Run a single test file
run_test() {
    local test_file="$1"
    local test_name=$(basename "$test_file" .hurl)

    echo -e "\n${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}Running: ${test_name}${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    local vars=""
    if [ -n "$API_KEY" ]; then
        vars="--variable api_key=$API_KEY"
    fi

    if hurl $VERBOSE $REPORT $vars --test "$test_file"; then
        echo -e "${GREEN}✓ $test_name passed${NC}"
        return 0
    else
        echo -e "${RED}✗ $test_name failed${NC}"
        return 1
    fi
}

# Main execution
echo -e "${GREEN}╔══════════════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║         Chronik Stream API Test Suite (Hurl)            ║${NC}"
echo -e "${GREEN}╚══════════════════════════════════════════════════════════╝${NC}"
echo ""

check_cluster

# Determine which tests to run
cd "$SCRIPT_DIR"

if [ -n "$SPECIFIC_TEST" ]; then
    test_file=$(get_test_file "$SPECIFIC_TEST")
    if [ -z "$test_file" ]; then
        # Check if it's a direct file reference
        if [ -f "$SPECIFIC_TEST" ]; then
            test_file="$SPECIFIC_TEST"
        elif [ -f "${SPECIFIC_TEST}.hurl" ]; then
            test_file="${SPECIFIC_TEST}.hurl"
        else
            echo -e "${RED}Unknown test: $SPECIFIC_TEST${NC}"
            exit 1
        fi
    fi
    run_test "$test_file"
else
    # Run all tests in order
    PASSED=0
    FAILED=0

    for test_file in *.hurl; do
        if run_test "$test_file"; then
            ((PASSED++))
        else
            ((FAILED++))
        fi
    done

    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}Test Summary${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "  ${GREEN}Passed: $PASSED${NC}"
    echo -e "  ${RED}Failed: $FAILED${NC}"

    if [ $FAILED -gt 0 ]; then
        exit 1
    fi
fi

if [ -n "$REPORT" ]; then
    echo -e "\n${GREEN}HTML report generated: $SCRIPT_DIR/report.html${NC}"
fi

echo ""
echo -e "${GREEN}Done!${NC}"
