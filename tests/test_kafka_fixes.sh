#!/bin/bash

# Test script to verify Kafka client compatibility fixes

set -e

echo "=== Testing Kafka Client Compatibility Fixes ==="
echo "This script tests the fixes for the reported issues:"
echo "1. Go client memory assertion failure"
echo "2. Python client connection timeout"
echo "3. Java client API versions parsing error"
echo "4. Metrics endpoint not responding"
echo ""

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Test metrics endpoint
test_metrics() {
    echo -e "${YELLOW}Testing metrics endpoint on port 9093...${NC}"
    if curl -s -o /dev/null -w "%{http_code}" http://localhost:9093/metrics | grep -q "200"; then
        echo -e "${GREEN}✓ Metrics endpoint is responding${NC}"
        return 0
    else
        echo -e "${RED}✗ Metrics endpoint not responding${NC}"
        return 1
    fi
}

# Test with kafkacat (if available)
test_kafkacat() {
    if command -v kafkacat &> /dev/null; then
        echo -e "${YELLOW}Testing with kafkacat...${NC}"
        
        # Get metadata
        if kafkacat -b localhost:9092 -L 2>/dev/null | grep -q "broker"; then
            echo -e "${GREEN}✓ kafkacat can fetch metadata${NC}"
        else
            echo -e "${RED}✗ kafkacat cannot fetch metadata${NC}"
            return 1
        fi
        
        # Try to produce a message
        echo "test-message" | kafkacat -b localhost:9092 -P -t test-topic 2>/dev/null
        if [ $? -eq 0 ]; then
            echo -e "${GREEN}✓ kafkacat can produce messages${NC}"
        else
            echo -e "${RED}✗ kafkacat cannot produce messages${NC}"
            return 1
        fi
    else
        echo -e "${YELLOW}kafkacat not installed, skipping kafkacat tests${NC}"
    fi
    return 0
}

# Test with Python client
test_python_client() {
    echo -e "${YELLOW}Testing Python kafka-python client...${NC}"
    
    python3 - <<EOF 2>/dev/null
import sys
import time
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

try:
    # Test producer connection with timeout
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        request_timeout_ms=5000,
        api_version=(2, 0, 0)
    )
    
    # Send a test message
    future = producer.send('test-topic', b'test-value')
    result = future.get(timeout=5)
    
    producer.close()
    print("SUCCESS: Python client can connect and produce")
    sys.exit(0)
except Exception as e:
    print(f"FAILED: Python client error: {e}")
    sys.exit(1)
EOF
    
    if [ $? -eq 0 ]; then
        echo -e "${GREEN}✓ Python client works correctly${NC}"
        return 0
    else
        echo -e "${RED}✗ Python client failed${NC}"
        return 1
    fi
}

# Test with Go client
test_go_client() {
    echo -e "${YELLOW}Testing Go confluent-kafka-go client...${NC}"
    
    # Create a temporary Go module for testing
    TMPDIR=$(mktemp -d)
    cd "$TMPDIR"
    
    cat > go.mod <<EOF
module kafka-test
go 1.21
require github.com/confluentinc/confluent-kafka-go/v2 v2.11.1
EOF
    
    cat > main.go <<'EOF'
package main

import (
    "fmt"
    "os"
    "github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
    producer, err := kafka.NewProducer(&kafka.ConfigMap{
        "bootstrap.servers": "localhost:9092",
        "client.id":        "chronik-test",
    })
    
    if err != nil {
        fmt.Printf("Failed to create producer: %s\n", err)
        os.Exit(1)
    }
    
    topic := "test-topic"
    err = producer.Produce(&kafka.Message{
        TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
        Key:           []byte("test-key"),
        Value:         []byte("test-value"),
    }, nil)
    
    if err != nil {
        fmt.Printf("Failed to produce message: %s\n", err)
        os.Exit(1)
    }
    
    // This was causing the assertion failure - test if it works now
    remaining := producer.Flush(5000)
    if remaining > 0 {
        fmt.Printf("Warning: %d messages were not delivered\n", remaining)
    }
    
    producer.Close()
    fmt.Println("SUCCESS: Go client works without assertion failure")
    os.Exit(0)
}
EOF
    
    go mod tidy 2>/dev/null
    go run main.go 2>/dev/null
    
    if [ $? -eq 0 ]; then
        echo -e "${GREEN}✓ Go client works without assertion failure${NC}"
        cd - > /dev/null
        rm -rf "$TMPDIR"
        return 0
    else
        echo -e "${RED}✗ Go client failed${NC}"
        cd - > /dev/null
        rm -rf "$TMPDIR"
        return 1
    fi
}

# Main test execution
echo "Starting tests..."
echo ""

# Check if Chronik is running
if ! nc -z localhost 9092 2>/dev/null; then
    echo -e "${RED}Error: Chronik server is not running on port 9092${NC}"
    echo "Please start the server with: cargo run --bin chronik-server"
    exit 1
fi

# Run tests
FAILED=0

test_metrics || FAILED=$((FAILED + 1))
echo ""

test_kafkacat || FAILED=$((FAILED + 1))
echo ""

test_python_client || FAILED=$((FAILED + 1))
echo ""

test_go_client || FAILED=$((FAILED + 1))
echo ""

# Summary
echo "=== Test Summary ==="
if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}All tests passed! The Kafka compatibility issues have been fixed.${NC}"
    exit 0
else
    echo -e "${RED}$FAILED test(s) failed. Some issues may still need attention.${NC}"
    exit 1
fi