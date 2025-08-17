#!/bin/bash
# Validate Chronik Stream Deployment

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Configuration
KAFKA_ENDPOINT="${KAFKA_ENDPOINT:-localhost:9092}"
ADMIN_ENDPOINT="${ADMIN_ENDPOINT:-http://localhost:3000}"
TEST_TOPIC="validation-test-$(date +%s)"

echo -e "${GREEN}Chronik Stream Deployment Validation${NC}"
echo "====================================="
echo "Kafka Endpoint: $KAFKA_ENDPOINT"
echo "Admin Endpoint: $ADMIN_ENDPOINT"
echo ""

# Function to check service
check_service() {
    local name=$1
    local check_cmd=$2
    
    echo -n "Checking $name... "
    if eval "$check_cmd" &> /dev/null; then
        echo -e "${GREEN}✓${NC}"
        return 0
    else
        echo -e "${RED}✗${NC}"
        return 1
    fi
}

# Function to run test
run_test() {
    local name=$1
    local test_cmd=$2
    
    echo -n "Testing $name... "
    if eval "$test_cmd" &> /dev/null; then
        echo -e "${GREEN}✓${NC}"
        return 0
    else
        echo -e "${RED}✗${NC}"
        return 1
    fi
}

# Connectivity tests
echo -e "${YELLOW}1. Connectivity Tests${NC}"
echo "---------------------"
check_service "Kafka broker" "nc -zv ${KAFKA_ENDPOINT%:*} ${KAFKA_ENDPOINT#*:}"
check_service "Admin API" "curl -s -f $ADMIN_ENDPOINT/health"

# Kafka protocol tests
echo ""
echo -e "${YELLOW}2. Kafka Protocol Tests${NC}"
echo "-----------------------"

# Test with kafkactl if available
if command -v kafkactl &> /dev/null; then
    run_test "List brokers" "kafkactl --brokers $KAFKA_ENDPOINT get brokers"
    run_test "Create topic" "kafkactl --brokers $KAFKA_ENDPOINT create topic $TEST_TOPIC --partitions 3"
    run_test "List topics" "kafkactl --brokers $KAFKA_ENDPOINT get topics | grep $TEST_TOPIC"
    run_test "Delete topic" "kafkactl --brokers $KAFKA_ENDPOINT delete topic $TEST_TOPIC"
else
    echo -e "${YELLOW}kafkactl not installed, using Python client${NC}"
    
    # Test with Python client
    python3 << EOF
import sys
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
import json

try:
    # Connect to cluster
    admin = KafkaAdminClient(
        bootstrap_servers='$KAFKA_ENDPOINT',
        client_id='validation-test'
    )
    print("✓ Connected to cluster")
    
    # Create topic
    topic = NewTopic(name='$TEST_TOPIC', num_partitions=3, replication_factor=1)
    admin.create_topics([topic])
    print("✓ Created topic")
    
    # Produce message
    producer = KafkaProducer(
        bootstrap_servers='$KAFKA_ENDPOINT',
        value_serializer=lambda v: json.dumps(v).encode('utf-8')
    )
    future = producer.send('$TEST_TOPIC', {'test': 'message'})
    producer.flush()
    print("✓ Produced message")
    
    # Consume message
    consumer = KafkaConsumer(
        '$TEST_TOPIC',
        bootstrap_servers='$KAFKA_ENDPOINT',
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000,
        value_deserializer=lambda m: json.loads(m.decode('utf-8'))
    )
    
    for message in consumer:
        if message.value.get('test') == 'message':
            print("✓ Consumed message")
            break
    
    # Cleanup
    consumer.close()
    producer.close()
    admin.delete_topics(['$TEST_TOPIC'])
    admin.close()
    print("✓ Cleanup complete")
    
except Exception as e:
    print(f"✗ Test failed: {e}")
    sys.exit(1)
EOF
fi

# Performance test
echo ""
echo -e "${YELLOW}3. Performance Test${NC}"
echo "-------------------"

if command -v kafka-producer-perf-test &> /dev/null; then
    echo "Running performance test (10,000 messages)..."
    kafka-producer-perf-test \
        --topic perf-test \
        --num-records 10000 \
        --record-size 1024 \
        --throughput -1 \
        --producer-props bootstrap.servers=$KAFKA_ENDPOINT \
        2>&1 | grep -E "records/sec|MB/sec"
else
    echo -e "${YELLOW}kafka-producer-perf-test not available${NC}"
fi

# Admin API tests
echo ""
echo -e "${YELLOW}4. Admin API Tests${NC}"
echo "------------------"

if [ "$ADMIN_ENDPOINT" != "http://localhost:3000" ]; then
    run_test "Health check" "curl -s -f $ADMIN_ENDPOINT/health"
    run_test "Metrics endpoint" "curl -s -f $ADMIN_ENDPOINT/metrics | grep -q chronik"
    run_test "API version" "curl -s -f $ADMIN_ENDPOINT/api/version"
fi

# Summary
echo ""
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Validation Complete!${NC}"
echo -e "${GREEN}========================================${NC}"
echo ""
echo "Your Chronik Stream deployment is ready!"
echo "Connect with: kafkactl --brokers $KAFKA_ENDPOINT"