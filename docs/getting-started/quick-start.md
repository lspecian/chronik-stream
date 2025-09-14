# Quick Start Guide

Get Chronik Stream up and running in 5 minutes using Docker Compose.

## 1. Clone the Repository

```bash
git clone https://github.com/chronik-stream/chronik-stream.git
cd chronik-stream
```

## 2. Start Chronik Stream

```bash
docker-compose up -d
```

This will start:
- Chronik Stream server on port 9092 (Kafka protocol)
- Search API on port 8080
- Admin UI on port 8081

## 3. Verify Installation

Check that all services are running:

```bash
docker-compose ps
```

You should see all containers in "Up" state.

## 4. Create Your First Topic

Using kafka-python:

```python
from kafka.admin import KafkaAdminClient, NewTopic

admin = KafkaAdminClient(bootstrap_servers='localhost:9092')

topic = NewTopic(
    name='my-first-topic',
    num_partitions=3,
    replication_factor=1
)

admin.create_topics([topic])
print("Topic created successfully!")
```

## 5. Produce Messages

```python
from kafka import KafkaProducer
import json

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda v: json.dumps(v).encode('utf-8')
)

# Send some messages
for i in range(10):
    message = {
        'id': i,
        'user': f'user_{i}',
        'action': 'click' if i % 2 == 0 else 'view',
        'timestamp': '2024-01-15T10:00:00Z'
    }
    producer.send('my-first-topic', message)

producer.flush()
print("Messages sent!")
```

## 6. Search Messages

Chronik Stream automatically indexes your messages. Search them using the REST API:

```bash
# Search for all 'click' actions
curl -X POST http://localhost:8080/api/v1/search \
  -H "Content-Type: application/json" \
  -d '{
    "topic": "my-first-topic",
    "query": {
      "match": {
        "action": "click"
      }
    }
  }'
```

## 7. Consume Messages

```python
from kafka import KafkaConsumer
import json

consumer = KafkaConsumer(
    'my-first-topic',
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    value_deserializer=lambda m: json.loads(m.decode('utf-8'))
)

for message in consumer:
    print(f"Received: {message.value}")
    # Press Ctrl+C to stop
```

## Next Steps

Congratulations! You've successfully:
- ✅ Started Chronik Stream
- ✅ Created a topic
- ✅ Produced messages
- ✅ Searched messages in real-time
- ✅ Consumed messages

Now you can:
- Build your [First Application](first-application.md) with more advanced features
- Learn about [Configuration](configuration.md) options
- Explore the [Search API](../api-reference/search-api.md) capabilities
- Check out more [Examples](../examples/index.md)

## Stopping Chronik Stream

To stop all services:

```bash
docker-compose down
```

To stop and remove all data:

```bash
docker-compose down -v
```