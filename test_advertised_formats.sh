#!/bin/bash

# Test script for various CHRONIK_ADVERTISED_ADDR formats

echo "Testing various CHRONIK_ADVERTISED_ADDR formats..."
echo "================================================"

test_format() {
    local format="$1"
    local expected="$2"
    
    echo -e "\nTest: CHRONIK_ADVERTISED_ADDR='$format'"
    echo "Expected: Advertised: $expected"
    
    pkill -f chronik-server 2>/dev/null
    sleep 1
    
    CHRONIK_ADVERTISED_ADDR="$format" timeout 2 ./target/release/chronik-server standalone 2>&1 | grep "Advertised:" | head -1
}

# Test cases
test_format "localhost" "localhost:9092"
test_format "localhost:9092" "localhost:9092" 
test_format "kafka.example.com" "kafka.example.com:9092"
test_format "kafka.example.com:9092" "kafka.example.com:9092"
test_format "192.168.1.100" "192.168.1.100:9092"
test_format "192.168.1.100:9092" "192.168.1.100:9092"
test_format "::1" "::1:9092"
test_format "[::1]" "[::1]:9092"
test_format "[::1]:9092" "[::1]:9092"
test_format "chronik-stream" "chronik-stream:9092"
test_format "chronik-stream:29092" "chronik-stream:29092"

echo -e "\n================================================"
echo "Test complete!"

pkill -f chronik-server 2>/dev/null