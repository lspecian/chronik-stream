#!/bin/bash
set -e

echo "================================================"
echo "Chronik Stream Integration Test Suite"
echo "================================================"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Check if Chronik is already running
if lsof -Pi :9092 -sTCP:LISTEN -t >/dev/null ; then
    echo -e "${YELLOW}Chronik already running on port 9092${NC}"
    CHRONIK_PID=""
else
    echo "Starting Chronik server..."
    cargo build --release
    ./target/release/chronik-server &
    CHRONIK_PID=$!
    echo "Chronik started with PID $CHRONIK_PID"

    # Wait for Chronik to be ready
    echo "Waiting for Chronik to be ready..."
    for i in {1..30}; do
        if nc -z localhost 9092 2>/dev/null; then
            echo -e "${GREEN}Chronik is ready${NC}"
            break
        fi
        sleep 1
    done
fi

# Run regression tests
echo ""
echo "Running regression tests..."
python3 tests/integration/test_regression.py
REGRESSION_EXIT=$?

# Run integration tests
echo ""
echo "Running integration tests..."
python3 tests/integration/test_kafka_clients.py
INTEGRATION_EXIT=$?

# Clean up
if [ ! -z "$CHRONIK_PID" ]; then
    echo ""
    echo "Stopping Chronik..."
    kill $CHRONIK_PID 2>/dev/null || true
    wait $CHRONIK_PID 2>/dev/null || true
fi

# Report results
echo ""
echo "================================================"
echo "Test Results:"
echo "================================================"

if [ $REGRESSION_EXIT -eq 0 ]; then
    echo -e "${GREEN}✓ Regression tests PASSED${NC}"
else
    echo -e "${RED}✗ Regression tests FAILED${NC}"
fi

if [ $INTEGRATION_EXIT -eq 0 ]; then
    echo -e "${GREEN}✓ Integration tests PASSED${NC}"
else
    echo -e "${RED}✗ Integration tests FAILED${NC}"
fi

# Exit with failure if any tests failed
if [ $REGRESSION_EXIT -ne 0 ] || [ $INTEGRATION_EXIT -ne 0 ]; then
    exit 1
fi

echo -e "${GREEN}All tests passed!${NC}"
exit 0