#!/bin/bash
# Copy Snapshot Implementation from Lahore to Main Workspace

set -e

LAHORE_DIR="/Users/lspecian/Development/chronik-stream/.conductor/lahore"
MAIN_DIR="/Users/lspecian/Development/chronik-stream"

echo "=========================================="
echo "Copying Snapshot Implementation to Main"
echo "=========================================="
echo ""
echo "Source: $LAHORE_DIR"
echo "Target: $MAIN_DIR"
echo ""

# Core Implementation File (THE MOST IMPORTANT)
echo "📝 Copying core implementation..."
cp -v "$LAHORE_DIR/crates/chronik-raft/src/replica.rs" "$MAIN_DIR/crates/chronik-raft/src/replica.rs"

# Test Files
echo ""
echo "📝 Copying test files..."
cp -v "$LAHORE_DIR/tests/integration/raft_snapshot_test.rs" "$MAIN_DIR/tests/integration/raft_snapshot_test.rs"
cp -v "$LAHORE_DIR/tests/integration/mod.rs" "$MAIN_DIR/tests/integration/mod.rs"

# Python Test Scripts
echo ""
echo "📝 Copying Python test scripts..."
cp -v "$LAHORE_DIR/test_snapshot_support.py" "$MAIN_DIR/test_snapshot_support.py"
cp -v "$LAHORE_DIR/test_raft_e2e_simple.py" "$MAIN_DIR/test_raft_e2e_simple.py"

# Documentation
echo ""
echo "📝 Copying documentation..."
cp -v "$LAHORE_DIR/SNAPSHOT_IMPLEMENTATION_COMPLETE.md" "$MAIN_DIR/SNAPSHOT_IMPLEMENTATION_COMPLETE.md"
cp -v "$LAHORE_DIR/SNAPSHOT_IMPLEMENTATION_PLAN.md" "$MAIN_DIR/SNAPSHOT_IMPLEMENTATION_PLAN.md"
cp -v "$LAHORE_DIR/SNAPSHOT_NEXT_STEPS.md" "$MAIN_DIR/SNAPSHOT_NEXT_STEPS.md"
cp -v "$LAHORE_DIR/SNAPSHOT_TEST_RESULTS.md" "$MAIN_DIR/SNAPSHOT_TEST_RESULTS.md"

echo ""
echo "=========================================="
echo "✅ Copy Complete!"
echo "=========================================="
echo ""
echo "Files copied to: $MAIN_DIR"
echo ""
echo "Next steps:"
echo "1. cd $MAIN_DIR"
echo "2. cargo build --release --bin chronik-server --features raft"
echo "3. Review: SNAPSHOT_IMPLEMENTATION_COMPLETE.md"
echo "4. Test: ./test_snapshot_support.py (requires cluster config)"
echo ""
