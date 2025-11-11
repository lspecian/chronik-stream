#!/bin/bash
# Profile the chronik-server cluster to identify bottlenecks at 9K msg/s

set -e

echo "================================================================================"
echo "Chronik Cluster Profiling - Identifying Bottleneck at 9K msg/s"
echo "================================================================================"
echo

# Get PIDs of all 3 nodes
NODE1_PID=$(pgrep -f "chronik-server.*node1.toml" | head -1)
NODE2_PID=$(pgrep -f "chronik-server.*node2.toml" | head -1)
NODE3_PID=$(pgrep -f "chronik-server.*node3.toml" | head -1)

if [ -z "$NODE1_PID" ] || [ -z "$NODE2_PID" ] || [ -z "$NODE3_PID" ]; then
    echo "ERROR: Cluster not running!"
    exit 1
fi

echo "Found cluster nodes:"
echo "  Node 1 PID: $NODE1_PID"
echo "  Node 2 PID: $NODE2_PID"
echo "  Node 3 PID: $NODE3_PID"
echo

# Create output directory
mkdir -p profiling
cd profiling

echo "Starting perf profiling (30 seconds)..."
echo "Will profile all 3 nodes simultaneously while running benchmark"
echo

# Start perf for each node in background
perf record -F 999 -p $NODE1_PID -g -o perf-node1.data &
PERF1_PID=$!

perf record -F 999 -p $NODE2_PID -g -o perf-node2.data &
PERF2_PID=$!

perf record -F 999 -p $NODE3_PID -g -o perf-node3.data &
PERF3_PID=$!

echo "✓ Started perf recording for all 3 nodes"
echo

# Wait 2 seconds for perf to initialize
sleep 2

# Run benchmark for 30 seconds
echo "Running chronik-bench to generate load (30s, 128 concurrency)..."
../target/release/chronik-bench \
  --bootstrap-servers localhost:9092,localhost:9093,localhost:9094 \
  --topic profiling-test \
  --concurrency 128 \
  --message-size 256 \
  --duration 30s \
  --mode produce \
  --acks 1 2>&1 | tail -20

echo
echo "✓ Benchmark completed"
echo

# Stop perf recordings
echo "Stopping perf recordings..."
kill -INT $PERF1_PID $PERF2_PID $PERF3_PID 2>/dev/null || true
wait $PERF1_PID $PERF2_PID $PERF3_PID 2>/dev/null || true

sleep 2

echo "✓ Perf data collected"
echo

# Generate reports
echo "Generating perf reports..."
echo

echo "=== Node 1 (Leader) Top 20 Hot Functions ==="
perf report -i perf-node1.data --stdio --percent-limit 1 -n | head -80 > perf-node1-report.txt
head -80 perf-node1-report.txt
echo

echo "=== Node 2 Top 20 Hot Functions ==="
perf report -i perf-node2.data --stdio --percent-limit 1 -n | head -80 > perf-node2-report.txt
head -80 perf-node2-report.txt
echo

echo "=== Node 3 Top 20 Hot Functions ==="
perf report -i perf-node3.data --stdio --percent-limit 1 -n | head -80 > perf-node3-report.txt
head -80 perf-node3-report.txt
echo

echo "================================================================================"
echo "Profiling Complete!"
echo "================================================================================"
echo
echo "Data files:"
echo "  profiling/perf-node1.data - Node 1 perf data"
echo "  profiling/perf-node2.data - Node 2 perf data"
echo "  profiling/perf-node3.data - Node 3 perf data"
echo
echo "Reports:"
echo "  profiling/perf-node1-report.txt - Node 1 hot functions"
echo "  profiling/perf-node2-report.txt - Node 2 hot functions"
echo "  profiling/perf-node3-report.txt - Node 3 hot functions"
echo
echo "To view flame graph:"
echo "  perf script -i profiling/perf-node1.data | inferno-collapse-perf | inferno-flamegraph > flamegraph.svg"
echo
