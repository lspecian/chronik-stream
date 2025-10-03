#!/bin/bash
# Test KSQL operations to discover remaining API issues

echo "Testing KSQL operations..."

# Test 1: Show topics
echo "1. Listing topics..."
curl -s -X POST http://localhost:8088/ksql \
  -H "Content-Type: application/vnd.ksql.v1+json; charset=utf-8" \
  -d '{"ksql":"SHOW TOPICS;"}' | jq '.'

echo -e "\n2. Creating a test topic with kafka-python..."
python3 - << 'EOF'
from kafka import KafkaProducer
import json

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda v: json.dumps(v).encode('utf-8')
)

# Send a test message to create the topic
producer.send('test_events', {'id': 1, 'message': 'hello'})
producer.flush()
print("✓ Sent test message to topic 'test_events'")
producer.close()
EOF

echo -e "\n3. Creating a KSQL stream..."
curl -s -X POST http://localhost:8088/ksql \
  -H "Content-Type: application/vnd.ksql.v1+json; charset=utf-8" \
  -d '{
    "ksql":"CREATE STREAM test_stream (id INT, message VARCHAR) WITH (kafka_topic='\''test_events'\'', value_format='\''JSON'\'');",
    "streamsProperties": {}
  }' | jq '.'

echo -e "\n4. Listing streams..."
curl -s -X POST http://localhost:8088/ksql \
  -H "Content-Type: application/vnd.ksql.v1+json; charset=utf-8" \
  -d '{"ksql":"SHOW STREAMS;"}' | jq '.'

echo -e "\nTest completed. Check /tmp/chronik-test.log for any new API requests."
