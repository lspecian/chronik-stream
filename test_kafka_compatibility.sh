#!/bin/bash
# Test Kafka wire protocol compatibility with the fixed ApiVersionsResponse

set -e

echo "Building Docker image with fix..."
docker build -f Dockerfile.all-in-one -t chronik-test:fixed .

echo "Starting Chronik container..."
docker run -d --name chronik-test \
  -p 9092:9092 -p 3000:3000 \
  chronik-test:fixed

echo "Waiting for Chronik to start..."
sleep 5

echo "Testing with kafka-topics (Confluent Platform)..."
docker run --rm --network host confluentinc/cp-kafka:7.5.0 \
  kafka-topics --bootstrap-server localhost:9092 \
  --create --if-not-exists --topic test-topic \
  --partitions 3 --replication-factor 1

echo "Listing topics..."
docker run --rm --network host confluentinc/cp-kafka:7.5.0 \
  kafka-topics --bootstrap-server localhost:9092 --list

echo "Testing with kafka-console-producer..."
echo "test message 1" | docker run -i --rm --network host confluentinc/cp-kafka:7.5.0 \
  kafka-console-producer --bootstrap-server localhost:9092 --topic test-topic

echo "Testing with kafka-console-consumer..."
docker run --rm --network host confluentinc/cp-kafka:7.5.0 \
  kafka-console-consumer --bootstrap-server localhost:9092 \
  --topic test-topic --from-beginning --max-messages 1 --timeout-ms 5000

echo "Cleaning up..."
docker stop chronik-test
docker rm chronik-test

echo "✅ All tests passed! Kafka wire protocol is working correctly."