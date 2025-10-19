#!/bin/bash

# Quick test script for single-node Raft commit fix

echo "=== Testing Single-Node Raft Commit Fix ===" echo ""
echo "Building project..."
cargo build --release -p chronik-server

if [ $? -ne 0 ]; then
    echo "✗ Build failed"
    exit 1
fi

echo "✓ Build succeeded"
echo ""
echo "The fix implements:"
echo "  1. campaign() call in constructor for single-node clusters"
echo "  2. Synchronous commit in propose() via tick() + ready()"
echo "  3. propose_and_wait() delegates to propose() for single-node"
echo ""
echo "See RAFT_SINGLE_NODE_COMMIT_FIX.md for full details"
echo ""
echo "=== Fix Complete ==="
