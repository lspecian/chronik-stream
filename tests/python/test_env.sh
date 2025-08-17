#!/bin/bash
# Environment setup for running integration tests

export RUST_LOG=info
export METADATA_STORE_TYPE=memory
export CHRONIK_INGEST_PORT=9092

# Start chronik-ingest with in-memory metadata store
echo "Starting Chronik Ingest with in-memory metadata store..."
cargo run --bin chronik-ingest &
INGEST_PID=$!

# Wait for server to start
echo "Waiting for server to start..."
sleep 5

# Check if server is running
if ! nc -z localhost 9092; then
    echo "Error: Chronik Ingest failed to start on port 9092"
    exit 1
fi

echo "Chronik Ingest started with PID: $INGEST_PID"
echo "Run your tests now. Press Ctrl+C to stop the server."

# Wait for user to stop
trap "kill $INGEST_PID; exit" INT TERM
wait $INGEST_PID