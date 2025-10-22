#!/usr/bin/env python3
"""
Simple test: Create topic and produce one message
This will help us debug why replica creation callback doesn't trigger
"""

from kafka import KafkaProducer
from kafka.admin import KafkaAdminClient, NewTopic
import time

# Create admin client
print("Creating admin client...")
admin = KafkaAdminClient(
    bootstrap_servers='localhost:9092',
    request_timeout_ms=30000
)

# Create topic
print("Creating topic 'debug-topic' with 1 partition, RF=1...")
topic = NewTopic(
    name='debug-topic',
    num_partitions=1,
    replication_factor=1
)

try:
    result = admin.create_topics([topic], timeout_ms=30000)
    print(f"create_topics() returned: {result}")
    print(f"Topic created! Waiting 5s for Raft replication...")
    time.sleep(5)
except Exception as e:
    print(f"Failed to create topic: {e}")

admin.close()

# Try to produce a message
print("\nProducing one message...")
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0)
)

future = producer.send('debug-topic', b'Test message')
try:
    record_metadata = future.get(timeout=10)
    print(f"Message sent! topic={record_metadata.topic} partition={record_metadata.partition} offset={record_metadata.offset}")
except Exception as e:
    print(f"Failed to send message: {e}")

producer.close()
print("\nDone. Check logs for 'CALLBACK' and 'CreateTopicWithAssignments applied' messages")
