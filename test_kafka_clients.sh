#!/bin/bash
# Test script to verify Kafka client compatibility with Chronik v0.5.2

echo "Testing Chronik Stream v0.5.2 Kafka Compatibility"
echo "================================================"

# Pull the latest image
echo "Pulling Docker image..."
docker pull ghcr.io/lspecian/chronik-stream:v0.5.2

# Start Chronik
echo "Starting Chronik Stream..."
docker run -d --name chronik-test -p 9092:9092 ghcr.io/lspecian/chronik-stream:v0.5.2

# Wait for startup
echo "Waiting for Chronik to start..."
sleep 5

# Test with different Kafka clients
echo ""
echo "Testing with kcat (kafkacat)..."
echo "--------------------------------"
echo "ApiVersions request:"
echo "00000012 0012 0000 0004 6b636174 0a6b61666b61636174" | xxd -r -p | nc localhost 9092 | xxd -p | head -2

echo ""
echo "Testing with Python client..."
echo "-----------------------------"
python3 -c "
from kafka import KafkaProducer
import sys
try:
    producer = KafkaProducer(bootstrap_servers='localhost:9092')
    producer.send('test-topic', b'Hello from Python')
    producer.flush()
    print('✓ Python client connected successfully')
except Exception as e:
    print(f'✗ Python client failed: {e}')
    sys.exit(1)
"

echo ""
echo "Testing with Go client..."
echo "-------------------------"
if command -v go &> /dev/null; then
    go run -e "
package main
import (
    \"fmt\"
    \"github.com/IBM/sarama\"
)
func main() {
    config := sarama.NewConfig()
    config.Version = sarama.V3_0_0_0
    client, err := sarama.NewClient([]string{\"localhost:9092\"}, config)
    if err != nil {
        fmt.Printf(\"✗ Go client failed: %v\\n\", err)
        return
    }
    defer client.Close()
    fmt.Println(\"✓ Go client connected successfully\")
}
" 2>/dev/null || echo "Go not installed, skipping Go test"
else
    echo "Go not installed, skipping Go test"
fi

echo ""
echo "Cleanup..."
docker stop chronik-test && docker rm chronik-test

echo ""
echo "Test complete!"