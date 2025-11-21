#!/bin/bash
# Minimal Phase 7 Test

echo "================================================"
echo "Phase 7 Minimal Integration Test"
echo "================================================"
echo ""

# Test 1: Check cluster processes
echo "Test 1: Cluster Process Status"
echo "-------------------------------"
ps aux | grep chronik-server | grep -v grep | wc -l | xargs echo "Chronik processes running:"
echo ""

# Test 2: Check ports
echo "Test 2: Network Ports"
echo "--------------------"
ss -tuln 2>/dev/null | grep -E ":(9092|9093|9094)" | wc -l | xargs echo "Kafka ports listening:"
echo ""

# Test 3: Check recent log activity
echo "Test 3: Recent Log Activity"
echo "---------------------------"
echo "Node 1 last log line:"
tail -1 /home/ubuntu/Development/chronik-stream/tests/cluster/logs/node1.log
echo ""
echo "Node 2 last log line:"
tail -1 /home/ubuntu/Development/chronik-stream/tests/cluster/logs/node2.log
echo ""
echo "Node 3 last log line:"
tail -1 /home/ubuntu/Development/chronik-stream/tests/cluster/logs/node3.log
echo ""

# Test 4: Check for errors
echo "Test 4: Error Count in Logs"
echo "---------------------------"
grep -c "ERROR" /home/ubuntu/Development/chronik-stream/tests/cluster/logs/node*.log 2>/dev/null | xargs echo "Total ERROR lines:"
echo ""

echo "================================================"
echo "Minimal test complete"
echo "================================================"
