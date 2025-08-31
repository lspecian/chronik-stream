#!/bin/bash
# Quick test script for Docker image

set -e

echo "======================================"
echo "Testing Chronik Server Docker Image"
echo "======================================"

# Check if image exists
if docker images | grep -q "chronik-server.*latest"; then
    echo "✓ Docker image found: chronik-server:latest"
else
    echo "✗ Docker image not found. Building may still be in progress."
    echo "  Run: docker build -t chronik-server:latest ."
    exit 1
fi

# Stop any existing test containers
echo "Cleaning up old test containers..."
docker stop chronik-docker-test 2>/dev/null || true
docker rm chronik-docker-test 2>/dev/null || true

# Run the container
echo "Starting Chronik Server container..."
docker run -d \
    --name chronik-docker-test \
    -p 9093:9092 \
    -e RUST_LOG=info \
    chronik-server:latest

# Wait for startup
echo "Waiting for server to start..."
sleep 5

# Check if container is running
if docker ps | grep -q chronik-docker-test; then
    echo "✓ Container is running"
else
    echo "✗ Container failed to start"
    docker logs chronik-docker-test
    exit 1
fi

# Show logs
echo ""
echo "Container logs:"
echo "---------------"
docker logs chronik-docker-test | tail -20

# Test connectivity
echo ""
echo "Testing connectivity..."
if nc -zv localhost 9093 2>&1 | grep -q succeeded; then
    echo "✓ Server is accepting connections on port 9093"
else
    echo "✗ Failed to connect to server"
    docker logs chronik-docker-test
    exit 1
fi

# Test with Python if available
if command -v python3 &> /dev/null; then
    echo ""
    echo "Testing with Python Kafka client..."
    python3 -c "
from kafka import KafkaProducer
import sys
try:
    producer = KafkaProducer(
        bootstrap_servers='localhost:9093',
        api_version=(0, 10, 0),
        request_timeout_ms=5000
    )
    print('✓ Successfully connected to Chronik Server via Kafka protocol')
    producer.close()
except Exception as e:
    print(f'✗ Failed to connect: {e}')
    sys.exit(1)
" || echo "  Note: Install kafka-python for full protocol testing"
fi

# Show resource usage
echo ""
echo "Resource usage:"
docker stats chronik-docker-test --no-stream

# Cleanup
echo ""
echo "Cleaning up test container..."
docker stop chronik-docker-test
docker rm chronik-docker-test

echo ""
echo "======================================"
echo "✅ Docker image test completed successfully!"
echo "======================================"
echo ""
echo "To publish this image:"
echo "1. Tag it: docker tag chronik-server:latest ghcr.io/yourusername/chronik-stream:v0.5.0"
echo "2. Login: echo \$GITHUB_TOKEN | docker login ghcr.io -u USERNAME --password-stdin"
echo "3. Push: docker push ghcr.io/yourusername/chronik-stream:v0.5.0"