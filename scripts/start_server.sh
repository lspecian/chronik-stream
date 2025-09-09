#!/bin/bash
# Start Chronik Stream server for testing

echo "Starting Chronik Stream server..."
echo "================================"

# Build the server
echo "Building Chronik Stream..."
cargo build --release --bin chronik

if [ $? -ne 0 ]; then
    echo "❌ Build failed!"
    exit 1
fi

echo "✅ Build successful"

# Start the server
echo "Starting server on port 9092..."
echo "Press Ctrl+C to stop"
echo ""

RUST_LOG=info cargo run --release --bin chronik