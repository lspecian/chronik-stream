#!/bin/bash

echo "Testing Chronik Stream Docker v0.5.3"
echo "====================================="
echo ""

# Stop any existing container
docker stop chronik-test 2>/dev/null
docker rm chronik-test 2>/dev/null

echo "Starting Chronik Stream v0.5.3..."
docker run -d --name chronik-test \
  -p 9092:9092 \
  -p 9093:9093 \
  -p 8080:8080 \
  ghcr.io/lspecian/chronik-stream:0.5.3 \
  --enable-search

# Wait for startup
sleep 5

echo ""
echo "Testing endpoints..."
echo "-------------------"

# Test Kafka port
echo -n "Kafka port (9092): "
nc -zv localhost 9092 2>&1 | grep -q succeeded && echo "✓ OPEN" || echo "✗ CLOSED"

# Test metrics
echo -n "Metrics endpoint: "
curl -s http://localhost:9093/metrics | head -1 && echo "✓ HAS DATA" || echo "✗ EMPTY"

# Test search API
echo -n "Search API: "
curl -s -X GET http://localhost:8080/_cluster/health 2>&1 | head -20

echo ""
echo "Testing Go client..."
echo "-------------------"
if [ -f test_go_client.go ]; then
  timeout 10 go run test_go_client.go 2>&1 | head -20
else
  echo "test_go_client.go not found"
fi

echo ""
echo "Testing Python client..."
echo "------------------------"
cat > test_python.py << 'EOF'
from kafka import KafkaProducer, KafkaConsumer
import time

print("Testing Python client...")
try:
    producer = KafkaProducer(bootstrap_servers='localhost:9092')
    print("✓ Producer created")
    
    future = producer.send('test-topic', b'Hello from Python')
    result = future.get(timeout=5)
    print(f"✓ Message sent: {result}")
    
    producer.flush()
    print("✓ Flush successful")
    
except Exception as e:
    print(f"✗ Error: {e}")
EOF

timeout 10 python3 test_python.py

echo ""
echo "Container logs:"
echo "---------------"
docker logs --tail 20 chronik-test

echo ""
echo "Cleaning up..."
docker stop chronik-test
docker rm chronik-test

echo "Test complete."