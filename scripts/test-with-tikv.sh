#!/bin/bash
# Script to run tests with TiKV using the test docker-compose configuration

set -e

echo "Starting TiKV test cluster..."
docker-compose -f docker-compose.test.yml up -d

# Wait for TiKV to be ready
echo "Waiting for TiKV to be ready..."
sleep 10

# Check if TiKV is healthy
docker-compose -f docker-compose.test.yml ps

echo "Running tests..."
cargo test -p chronik-common -- --test-threads=1

# Cleanup
echo "Cleaning up..."
docker-compose -f docker-compose.test.yml down -v

echo "Tests completed!"