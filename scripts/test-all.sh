#!/bin/bash
# Script to run all tests

set -e

echo "Running all tests..."

# Run tests for each crate that doesn't require TiKV
echo "Testing chronik-storage..."
cargo test -p chronik-storage -- --test-threads=1

echo "Testing chronik-backup..."
cargo test -p chronik-backup -- --test-threads=1

echo "Testing chronik-protocol..."
cargo test -p chronik-protocol -- --test-threads=1

echo "Testing chronik-auth..."
cargo test -p chronik-auth -- --test-threads=1

echo "Testing chronik-config..."
cargo test -p chronik-config -- --test-threads=1

# For chronik-common, note that TiKV tests require TiKV to be running
echo ""
echo "Note: chronik-common tests require TiKV to be running."
echo "To run them:"
echo "  1. Start TiKV: docker-compose -f docker-compose.test.yml up -d"
echo "  2. Wait ~60 seconds for cluster to bootstrap"
echo "  3. Run: cargo test -p chronik-common"
echo ""

echo "All non-TiKV tests completed successfully!"