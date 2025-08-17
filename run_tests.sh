#!/bin/bash
#
# Main test runner for Chronik Stream
#

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${BLUE}=== Chronik Stream Test Suite ===${NC}"

# Function to run a test category
run_test_category() {
    local category=$1
    local description=$2
    
    echo -e "\n${BLUE}Running ${description}...${NC}"
    
    if [ -d "tests/python/${category}" ]; then
        cd tests/python
        python -m pytest "${category}/" -v --tb=short
        cd ../..
    else
        echo -e "${RED}Directory tests/python/${category} not found${NC}"
        return 1
    fi
}

# Parse command line arguments
TEST_TYPE=${1:-all}

case $TEST_TYPE in
    all)
        echo "Running all tests..."
        
        # Run Rust tests
        echo -e "\n${BLUE}Running Rust tests...${NC}"
        cargo test
        
        # Run Python tests
        run_test_category "protocol" "Protocol Compatibility Tests"
        run_test_category "integration" "Integration Tests"
        run_test_category "performance" "Performance Tests"
        ;;
        
    rust)
        echo -e "\n${BLUE}Running Rust tests...${NC}"
        cargo test
        ;;
        
    protocol)
        run_test_category "protocol" "Protocol Compatibility Tests"
        ;;
        
    integration)
        run_test_category "integration" "Integration Tests"
        ;;
        
    performance)
        run_test_category "performance" "Performance Tests"
        ;;
        
    quick)
        echo "Running quick smoke tests..."
        
        # Run a subset of fast tests
        echo -e "\n${BLUE}Running Rust unit tests...${NC}"
        cargo test --lib
        
        echo -e "\n${BLUE}Running basic integration tests...${NC}"
        cd tests/python
        python integration/simple_kafka_test.py
        cd ../..
        ;;
        
    *)
        echo "Usage: $0 [all|rust|protocol|integration|performance|quick]"
        echo ""
        echo "  all         - Run all tests (default)"
        echo "  rust        - Run only Rust tests"
        echo "  protocol    - Run protocol compatibility tests"
        echo "  integration - Run integration tests"
        echo "  performance - Run performance tests"
        echo "  quick       - Run quick smoke tests"
        exit 1
        ;;
esac

echo -e "\n${GREEN}✓ Test suite completed${NC}"