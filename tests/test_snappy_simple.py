#!/usr/bin/env python3
"""Simple test for Snappy compression"""
from kafka import KafkaProducer, KafkaConsumer
import time

# Test with Snappy
print("Testing Snappy compression...")

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    compression_type='snappy',
    api_version=(0, 10, 0)
)

topic = 'snappy-test'

# Send a few messages
for i in range(5):
    msg = f"Message {i} with Snappy compression"
    print(f"Sending: {msg}")
    future = producer.send(topic, msg.encode('utf-8'))
    try:
        metadata = future.get(timeout=10)
        print(f"  → Sent to partition {metadata.partition} at offset {metadata.offset}")
    except Exception as e:
        print(f"  → FAILED: {e}")

producer.flush()
producer.close()

time.sleep(2)

# Consume
print("\nConsuming messages...")
consumer = KafkaConsumer(
    topic,
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=10000,
    api_version=(0, 10, 0)
)

count = 0
for msg in consumer:
    count += 1
    print(f"  ← Received at offset {msg.offset}: {msg.value.decode('utf-8')}")

consumer.close()

print(f"\nConsumed {count} messages")
