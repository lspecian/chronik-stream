#!/bin/bash
set -e

# Quick validation script to test a single client
# This validates that the compatibility test framework is working

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo "========================================="
echo "Compatibility Test Framework Validation"
echo "========================================="
echo ""

# Determine docker-compose command
if docker compose version &> /dev/null 2>&1; then
    DOCKER_COMPOSE="docker compose"
else
    DOCKER_COMPOSE="docker-compose"
fi

echo -e "${YELLOW}[1/5]${NC} Cleaning up any previous test containers..."
$DOCKER_COMPOSE down -v 2>/dev/null || true

echo -e "${YELLOW}[2/5]${NC} Building Chronik container..."
$DOCKER_COMPOSE build chronik

echo -e "${YELLOW}[3/5]${NC} Starting Chronik server..."
$DOCKER_COMPOSE up -d chronik

echo -e "${YELLOW}[4/5]${NC} Waiting for Chronik to be healthy..."
for i in {1..30}; do
    if $DOCKER_COMPOSE ps chronik 2>/dev/null | grep -q "healthy"; then
        echo -e "${GREEN}✓${NC} Chronik is ready"
        break
    fi
    if [ $i -eq 30 ]; then
        echo -e "${RED}✗${NC} Chronik failed to become healthy"
        echo "Chronik logs:"
        $DOCKER_COMPOSE logs chronik
        exit 1
    fi
    sleep 2
    echo -n "."
done
echo ""

echo -e "${YELLOW}[5/5]${NC} Running kafka-python compatibility test..."
echo "----------------------------------------"

# Build and run the kafka-python test
$DOCKER_COMPOSE build kafka-python-client
if $DOCKER_COMPOSE run --rm kafka-python-client; then
    echo -e "${GREEN}✓${NC} kafka-python tests completed successfully"
else
    echo -e "${RED}✗${NC} kafka-python tests failed"
    echo ""
    echo "Chronik logs:"
    $DOCKER_COMPOSE logs --tail=50 chronik
    exit 1
fi

echo ""
echo "----------------------------------------"
echo -e "${GREEN}SUCCESS!${NC} Compatibility test framework is working correctly."
echo ""

# Check if results were generated
if [ -f "results/kafka-python-results.json" ]; then
    echo "Test results saved to: results/kafka-python-results.json"
    echo ""
    echo "Sample results:"
    cat results/kafka-python-results.json | python3 -m json.tool | head -20
else
    echo -e "${YELLOW}Note:${NC} Results file not found. Check volume mounting."
fi

echo ""
echo "To run all tests, use: ./scripts/run-tests.sh"
echo "To test a specific client: ./scripts/run-tests.sh --client confluent-kafka"

# Cleanup
echo ""
echo "Cleaning up..."
$DOCKER_COMPOSE down

echo "Validation complete!"