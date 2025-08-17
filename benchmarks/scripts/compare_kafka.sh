#!/bin/bash
#
# Compare performance between Chronik Stream and Apache Kafka
#
# This script:
# 1. Runs benchmarks against Chronik Stream
# 2. Runs the same benchmarks against Apache Kafka  
# 3. Generates comparison reports
#

set -e

SCRIPT_DIR=$(dirname "$0")
BENCHMARK_DIR=$(dirname "$SCRIPT_DIR")
PYTHON_BENCHMARK="${BENCHMARK_DIR}/python/benchmark_suite.py"
RESULTS_DIR="${BENCHMARK_DIR}/results/comparison_$(date +%Y%m%d_%H%M%S)"

# Configuration
NUM_MESSAGES=1000000
MESSAGE_SIZE=1024
NUM_PARTITIONS=10

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${BLUE}=== Chronik Stream vs Apache Kafka Performance Comparison ===${NC}"
echo "Configuration:"
echo "  Messages: ${NUM_MESSAGES}"
echo "  Message size: ${MESSAGE_SIZE} bytes"
echo "  Partitions: ${NUM_PARTITIONS}"
echo ""

# Create results directory
mkdir -p "$RESULTS_DIR"

# Function to check if service is running
check_service() {
    local name=$1
    local port=$2
    
    if nc -z localhost $port 2>/dev/null; then
        echo -e "${GREEN}✓ $name is running on port $port${NC}"
        return 0
    else
        echo -e "${RED}✗ $name is not running on port $port${NC}"
        return 1
    fi
}

# Run benchmarks against a Kafka-compatible service
run_benchmark() {
    local name=$1
    local bootstrap_servers=$2
    local output_dir="${RESULTS_DIR}/${name}"
    
    echo -e "\n${BLUE}Running benchmarks against $name...${NC}"
    
    mkdir -p "$output_dir"
    
    python3 "$PYTHON_BENCHMARK" \
        --bootstrap-servers "$bootstrap_servers" \
        --num-messages "$NUM_MESSAGES" \
        --message-size "$MESSAGE_SIZE" \
        --num-partitions "$NUM_PARTITIONS" \
        --results-dir "$output_dir" \
        2>&1 | tee "${output_dir}/benchmark_log.txt"
    
    echo -e "${GREEN}✓ Completed $name benchmarks${NC}"
}

# Check services
echo -e "\n${BLUE}Checking services...${NC}"

CHRONIK_AVAILABLE=false
KAFKA_AVAILABLE=false

if check_service "Chronik Stream" 9092; then
    CHRONIK_AVAILABLE=true
fi

if check_service "Apache Kafka" 9095; then
    KAFKA_AVAILABLE=true
fi

# Run benchmarks
if [ "$CHRONIK_AVAILABLE" = true ]; then
    run_benchmark "chronik-stream" "localhost:9092"
else
    echo -e "${RED}Skipping Chronik Stream benchmarks - service not available${NC}"
fi

if [ "$KAFKA_AVAILABLE" = true ]; then
    run_benchmark "apache-kafka" "localhost:9095"
else
    echo -e "${RED}Skipping Apache Kafka benchmarks - service not available${NC}"
fi

# Generate comparison report
echo -e "\n${BLUE}Generating comparison report...${NC}"

python3 - <<EOF
import json
import os
from tabulate import tabulate

results_dir = "$RESULTS_DIR"
comparison_data = []

# Load results from both systems
for system in ["chronik-stream", "apache-kafka"]:
    system_dir = os.path.join(results_dir, system)
    if not os.path.exists(system_dir):
        continue
    
    # Find the latest results file
    result_files = [f for f in os.listdir(system_dir) if f.startswith("benchmark_results_") and f.endswith(".json")]
    if not result_files:
        continue
    
    latest_file = sorted(result_files)[-1]
    with open(os.path.join(system_dir, latest_file), 'r') as f:
        data = json.load(f)
        
        for result in data['results']:
            comparison_data.append({
                'System': system,
                'Test': result['test_name'],
                'Throughput (msg/s)': f"{result['throughput_msgs_sec']:,.0f}",
                'Throughput (MB/s)': f"{result['throughput_mb_sec']:.2f}",
                'Avg Latency (ms)': f"{result['avg_latency_ms']:.2f}" if result['avg_latency_ms'] > 0 else "N/A",
                'P99 Latency (ms)': f"{result['p99_latency_ms']:.2f}" if result['p99_latency_ms'] > 0 else "N/A"
            })

# Create comparison table
if comparison_data:
    print("\n=== Performance Comparison ===\n")
    
    # Group by test name for side-by-side comparison
    tests = {}
    for row in comparison_data:
        test_name = row['Test']
        if test_name not in tests:
            tests[test_name] = {}
        tests[test_name][row['System']] = row
    
    # Create comparison rows
    comparison_rows = []
    for test_name, systems in tests.items():
        for system, data in systems.items():
            comparison_rows.append([
                system,
                test_name.replace('_', ' ').title(),
                data['Throughput (msg/s)'],
                data['Throughput (MB/s)'],
                data['Avg Latency (ms)'],
                data['P99 Latency (ms)']
            ])
    
    headers = ["System", "Test", "Throughput (msg/s)", "Throughput (MB/s)", "Avg Latency (ms)", "P99 Latency (ms)"]
    print(tabulate(comparison_rows, headers=headers, tablefmt="grid"))
    
    # Save comparison to file
    comparison_file = os.path.join(results_dir, "comparison_summary.txt")
    with open(comparison_file, 'w') as f:
        f.write("Performance Comparison: Chronik Stream vs Apache Kafka\n")
        f.write("=" * 80 + "\n\n")
        f.write(tabulate(comparison_rows, headers=headers, tablefmt="grid"))
        f.write("\n")
    
    print(f"\nComparison saved to: {comparison_file}")
else:
    print("No comparison data available")
EOF

echo -e "\n${GREEN}✓ Benchmark comparison complete!${NC}"
echo -e "Results saved to: ${RESULTS_DIR}"