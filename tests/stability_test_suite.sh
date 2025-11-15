#!/bin/bash
# Chronik Stability Test Suite - v2.2.7+
# Runs comprehensive stability tests to catch regressions

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "=========================================="
echo "Chronik Stability Test Suite"
echo "=========================================="
echo ""

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

TESTS_PASSED=0
TESTS_FAILED=0
TESTS_TOTAL=0

# Test result tracking
log_test() {
    TESTS_TOTAL=$((TESTS_TOTAL + 1))
    echo -e "${YELLOW}[TEST $TESTS_TOTAL]${NC} $1"
}

pass_test() {
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓ PASSED${NC}: $1"
    echo ""
}

fail_test() {
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗ FAILED${NC}: $1"
    echo "  Error: $2"
    echo ""
}

# Cleanup function
cleanup() {
    echo "Cleaning up..."
    ./tests/cluster/stop.sh 2>/dev/null || true
    sleep 2
}

trap cleanup EXIT

# Test 1: Cluster Startup Stability
log_test "Cluster starts cleanly without errors"
cleanup
./tests/cluster/start.sh > /tmp/startup.log 2>&1
if grep -q "ERROR\|PANIC\|fatal" /tmp/startup.log; then
    fail_test "Cluster startup" "Found errors in startup logs"
else
    sleep 15  # Wait for leader election and cluster stabilization
    if pgrep -f "chronik-server" > /dev/null; then
        pass_test "Cluster startup"
    else
        fail_test "Cluster startup" "Processes not running"
    fi
fi

# Test 2: Basic Produce-Consume (1K messages)
log_test "Produce and consume 1K messages"
python3 << 'EOF' > /tmp/test_basic.log 2>&1
from kafka import KafkaProducer, KafkaConsumer
import sys

producer = KafkaProducer(bootstrap_servers='localhost:9092', acks=1)
for i in range(1000):
    producer.send('stability-test-1k', f"msg-{i}".encode())
producer.flush()
producer.close()

consumer = KafkaConsumer('stability-test-1k',
                        bootstrap_servers='localhost:9092',
                        auto_offset_reset='earliest',
                        consumer_timeout_ms=10000,
                        group_id='stability-group-1k')
count = sum(1 for _ in consumer)
consumer.close()

if count == 1000:
    print("SUCCESS: 1000/1000")
    sys.exit(0)
else:
    print(f"FAILED: {count}/1000")
    sys.exit(1)
EOF

if [ $? -eq 0 ]; then
    pass_test "Basic produce-consume 1K"
else
    fail_test "Basic produce-consume 1K" "$(tail -1 /tmp/test_basic.log)"
fi

# Test 3: High Concurrency (64 producers)
log_test "High concurrency - 64 concurrent producers"
timeout 60s ./target/release/chronik-bench \
    --bootstrap-servers localhost:9092,localhost:9093,localhost:9094 \
    --topic stability-test-64 \
    --concurrency 64 \
    --message-size 256 \
    --duration 30s \
    --create-topic \
    --partitions 12 \
    --replication-factor 3 > /tmp/test_64.log 2>&1

if [ $? -eq 0 ] && grep -q "Messages sent:" /tmp/test_64.log && ! grep -q "Failed:" /tmp/test_64.log; then
    SENT=$(grep "Messages sent:" /tmp/test_64.log | awk '{print $3}')
    pass_test "High concurrency 64 producers ($SENT messages)"
else
    fail_test "High concurrency 64 producers" "$(tail -5 /tmp/test_64.log)"
fi

# Test 4: Cluster Remains Responsive After Load
log_test "Cluster remains responsive after heavy load"
sleep 5
python3 << 'EOF' > /tmp/test_responsive.log 2>&1
from kafka import KafkaProducer
import sys

producer = KafkaProducer(bootstrap_servers='localhost:9092', acks=1, request_timeout_ms=5000)
try:
    future = producer.send('responsiveness-test', b'test-message')
    future.get(timeout=5)
    producer.close()
    print("SUCCESS: Cluster responsive")
    sys.exit(0)
except Exception as e:
    print(f"FAILED: {e}")
    sys.exit(1)
EOF

if [ $? -eq 0 ]; then
    pass_test "Cluster responsiveness after load"
else
    fail_test "Cluster responsiveness" "$(cat /tmp/test_responsive.log)"
fi

# Test 5: No Deadlocks (Check CPU usage)
log_test "No deadlocks detected (CPU > 0)"
sleep 2
for node in 1 2 3; do
    PID=$(pgrep -f "chronik-server.*node$node" | head -1)
    if [ -n "$PID" ]; then
        CPU=$(ps -p $PID -o %cpu= | awk '{print $1}')
        if [ -z "$CPU" ] || [ "$CPU" = "0.0" ]; then
            fail_test "No deadlocks - Node $node" "CPU usage is 0% (possible deadlock)"
            break
        fi
    fi
done
if [ $TESTS_FAILED -eq 0 ] || [ $TESTS_FAILED -eq $((TESTS_TOTAL - 1)) ]; then
    pass_test "No deadlocks detected (all nodes active)"
fi

# Test 6: Log Analysis - No Critical Errors
log_test "Log analysis - no critical errors"
CRITICAL_ERRORS=0
for log in tests/cluster/logs/node*.log; do
    if grep -q "PANIC\|fatal\|deadlock\|SEGFAULT" "$log"; then
        CRITICAL_ERRORS=$((CRITICAL_ERRORS + 1))
    fi
done

if [ $CRITICAL_ERRORS -eq 0 ]; then
    pass_test "Log analysis clean"
else
    fail_test "Log analysis" "Found $CRITICAL_ERRORS critical errors in logs"
fi

# Test 7: Memory Leak Check (Basic)
log_test "Memory leak check (RSS stable)"
for node in 1 2 3; do
    PID=$(pgrep -f "chronik-server.*node$node" | head -1)
    if [ -n "$PID" ]; then
        RSS1=$(ps -p $PID -o rss= | awk '{print $1}')
        sleep 5
        RSS2=$(ps -p $PID -o rss= | awk '{print $1}')
        DIFF=$((RSS2 - RSS1))
        # Allow 50MB growth (normal for caching)
        if [ $DIFF -gt 51200 ]; then
            fail_test "Memory leak check - Node $node" "RSS grew ${DIFF}KB in 5s"
            break
        fi
    fi
done
if [ $TESTS_FAILED -eq 0 ] || [ $TESTS_FAILED -lt $TESTS_TOTAL ]; then
    pass_test "No obvious memory leaks"
fi

# Test 8: Consumer Group Coordination
log_test "Consumer group coordination"
python3 << 'EOF' > /tmp/test_consumer_group.log 2>&1
from kafka import KafkaProducer, KafkaConsumer
import sys

# Produce 100 messages
producer = KafkaProducer(bootstrap_servers='localhost:9092', acks=1)
for i in range(100):
    producer.send('consumer-group-test', f"msg-{i}".encode())
producer.flush()
producer.close()

# Consume with group
consumer = KafkaConsumer('consumer-group-test',
                        bootstrap_servers='localhost:9092',
                        auto_offset_reset='earliest',
                        consumer_timeout_ms=10000,
                        group_id='test-cg-stability',
                        enable_auto_commit=True)
count = sum(1 for _ in consumer)
consumer.close()

# Consume again (should be 0 if offsets committed)
consumer2 = KafkaConsumer('consumer-group-test',
                         bootstrap_servers='localhost:9092',
                         auto_offset_reset='earliest',
                         consumer_timeout_ms=5000,
                         group_id='test-cg-stability',
                         enable_auto_commit=True)
count2 = sum(1 for _ in consumer2)
consumer2.close()

if count == 100 and count2 == 0:
    print(f"SUCCESS: Consumed {count}, reread {count2}")
    sys.exit(0)
else:
    print(f"FAILED: Consumed {count}/100, reread {count2}/0")
    sys.exit(1)
EOF

if [ $? -eq 0 ]; then
    pass_test "Consumer group coordination"
else
    fail_test "Consumer group coordination" "$(cat /tmp/test_consumer_group.log)"
fi

# Final Summary
echo "=========================================="
echo "Test Summary"
echo "=========================================="
echo -e "Total tests:  $TESTS_TOTAL"
echo -e "${GREEN}Passed:       $TESTS_PASSED${NC}"
echo -e "${RED}Failed:       $TESTS_FAILED${NC}"
echo ""

if [ $TESTS_FAILED -eq 0 ]; then
    echo -e "${GREEN}✓ ALL TESTS PASSED${NC}"
    echo ""
    exit 0
else
    echo -e "${RED}✗ SOME TESTS FAILED${NC}"
    echo ""
    echo "Check logs in /tmp/test_*.log for details"
    exit 1
fi
