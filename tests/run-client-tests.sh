#!/bin/bash
# Script to run Kafka client compatibility tests

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "Building client test Docker image..."
docker build -f "$SCRIPT_DIR/Dockerfile.client-tests" -t chronik-client-tests "$SCRIPT_DIR"

echo "Starting Chronik Stream cluster..."
# Assuming docker-compose.yml is set up to run Chronik Stream
docker-compose -f "$PROJECT_ROOT/docker-compose.yml" up -d

# Wait for cluster to be ready
echo "Waiting for cluster to be ready..."
sleep 10

# Get bootstrap servers from docker-compose
BOOTSTRAP_SERVERS="localhost:9092"

echo "Running kafkactl tests..."
docker run --rm --network host chronik-client-tests bash -c "
    kafkactl get brokers --brokers $BOOTSTRAP_SERVERS
    kafkactl create topic test-topic --brokers $BOOTSTRAP_SERVERS
    echo 'test message' | kafkactl produce test-topic --brokers $BOOTSTRAP_SERVERS
    kafkactl consume test-topic --from-beginning --max-messages 1 --brokers $BOOTSTRAP_SERVERS
"

echo "Running Python client tests..."
docker run --rm --network host chronik-client-tests python3 -c "
from confluent_kafka import Producer, Consumer
import sys

# Producer test
p = Producer({'bootstrap.servers': '$BOOTSTRAP_SERVERS'})
p.produce('test-topic', key='test-key', value='test from python')
p.flush()
print('Produced message successfully')

# Consumer test
c = Consumer({
    'bootstrap.servers': '$BOOTSTRAP_SERVERS',
    'group.id': 'test-group',
    'auto.offset.reset': 'earliest'
})
c.subscribe(['test-topic'])

msg = c.poll(timeout=5.0)
if msg and not msg.error():
    print(f'Consumed: {msg.value().decode()}')
else:
    print('Failed to consume message')
    sys.exit(1)

c.close()
"

echo "Running Go/Sarama tests..."
docker run --rm --network host chronik-client-tests bash -c "
    echo 'test from sarama' | kafka-console-producer -brokers=$BOOTSTRAP_SERVERS -topic=test-topic
    kafka-console-consumer -brokers=$BOOTSTRAP_SERVERS -topic=test-topic -partition=0 -offset=0 -max-messages=1
"

echo "Running cross-client test..."
docker run --rm --network host chronik-client-tests bash -c "
    # Produce with kafkactl
    echo 'cross-client-test' | kafkactl produce cross-test --brokers $BOOTSTRAP_SERVERS
    
    # Consume with kcat
    timeout 5 kcat -b $BOOTSTRAP_SERVERS -t cross-test -C -c 1 -e
"

echo "All client tests completed successfully!"

# Cleanup
echo "Cleaning up..."
docker-compose -f "$PROJECT_ROOT/docker-compose.yml" down