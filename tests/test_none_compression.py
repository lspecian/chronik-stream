#!/usr/bin/env python3
"""Test for 'none' compression specifically"""
from kafka import KafkaProducer, KafkaConsumer
import time

topic = 'test-none-only'

print("="*60)
print("Testing NO COMPRESSION")
print("="*60)

# Producer without compression
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0),
    linger_ms=0  # Send immediately, no batching
)

messages = [f"No compression message {i}" for i in range(10)]

print(f"\nSending {len(messages)} messages WITHOUT compression...")
for i, msg in enumerate(messages):
    future = producer.send(topic, msg.encode('utf-8'))
    metadata = future.get(timeout=10)
    print(f"  {i}: partition={metadata.partition}, offset={metadata.offset}")

producer.flush()
producer.close()

time.sleep(2)

print(f"\nConsuming from {topic}...")
consumer = KafkaConsumer(
    topic,
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=5000,
    api_version=(0, 10, 0)
)

consumed = []
for msg in consumer:
    consumed.append(msg.value.decode('utf-8'))
    print(f"  Offset {msg.offset} (partition {msg.partition}): {msg.value.decode('utf-8')}")

consumer.close()

print(f"\n{'='*60}")
print(f"Result: {len(consumed)}/{len(messages)} messages consumed")
print(f"{'='*60}")

if len(consumed) == len(messages):
    print("✅ SUCCESS: All messages consumed")
else:
    print(f"❌ FAILED: Only {len(consumed)}/{len(messages)} messages")
    print("\nMissing messages:")
    for i, msg in enumerate(messages):
        if msg not in consumed:
            print(f"  - Message {i}: {msg}")
