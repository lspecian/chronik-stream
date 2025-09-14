#!/usr/bin/env python3
"""
Simple test to debug consumer not fetching messages
"""

from kafka import KafkaProducer, KafkaConsumer
import json
import time

# Produce a message
producer = KafkaProducer(
    bootstrap_servers=['localhost:9092'],
    value_serializer=lambda v: json.dumps(v).encode('utf-8')
)

msg = {'test': 'message', 'timestamp': time.time()}
future = producer.send('simple-test', value=msg)
result = future.get(timeout=10)
print(f"Produced message to offset {result.offset}")
producer.close()

# Try to consume immediately
print("\nCreating consumer...")
consumer = KafkaConsumer(
    'simple-test',
    bootstrap_servers=['localhost:9092'],
    auto_offset_reset='earliest',
    enable_auto_commit=False,
    max_poll_records=1,
    fetch_min_bytes=1,
    fetch_max_wait_ms=1000,
    consumer_timeout_ms=5000,
    value_deserializer=lambda m: json.loads(m.decode('utf-8')) if m else None
)

print("Polling for messages...")
messages = []
for message in consumer:
    print(f"Got message: offset={message.offset}, value={message.value}")
    messages.append(message)
    
consumer.close()

if messages:
    print(f"\n✅ SUCCESS: Consumed {len(messages)} messages")
else:
    print("\n❌ FAILURE: No messages consumed")