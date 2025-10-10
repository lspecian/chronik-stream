#!/usr/bin/env python3
"""Debug test to understand batching behavior"""
from kafka import KafkaProducer, KafkaConsumer
import time

topic = 'batch-debug'

print("="*60)
print("BATCH DEBUG TEST - Sending to single partition")
print("="*60)

# Force all messages to partition 0 by using a key
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    api_version=(0, 10, 0),
    linger_ms=0  # No batching delay
)

messages = [
    (f"msg-{i}", f"Message {i}")
    for i in range(10)
]

print(f"\nSending {len(messages)} messages to partition 0 (using key)...")
for key, value in messages:
    # Use same key to force same partition
    future = producer.send(
        topic,
        value=value.encode('utf-8'),
        key=b'same-key'  # Force all to same partition
    )
    metadata = future.get(timeout=10)
    print(f"  {key}: partition={metadata.partition}, offset={metadata.offset}")

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
    print(f"  Partition {msg.partition}, Offset {msg.offset}: {msg.value.decode('utf-8')}")

consumer.close()

print(f"\n{'='*60}")
print(f"Result: {len(consumed)}/{len(messages)} messages consumed")
print(f"{'='*60}")

if len(consumed) == len(messages):
    print("✅ SUCCESS")
else:
    print(f"❌ FAILED: Missing {len(messages) - len(consumed)} messages")
