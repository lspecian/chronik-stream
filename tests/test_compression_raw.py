#!/usr/bin/env python3
"""Test compression by examining raw bytes"""
from kafka import KafkaProducer, KafkaConsumer
import time

# Test with gzip
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    compression_type='gzip',
    api_version=(0, 10, 0)
)

topic = 'test-raw-gzip'

# Send 5 messages
print("Sending 5 messages with gzip compression...")
for i in range(5):
    msg = f"Message {i}"
    producer.send(topic, msg.encode('utf-8'))
    print(f"  Sent: {msg}")

producer.flush()
producer.close()

time.sleep(2)

# Consume and check
print("\nConsuming messages...")
consumer = KafkaConsumer(
    topic,
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=5000,
    api_version=(0, 10, 0)
)

for msg in consumer:
    print(f"\nOffset {msg.offset}:")
    print(f"  Value length: {len(msg.value)}")
    print(f"  First 20 bytes (hex): {msg.value[:20].hex()}")
    print(f"  Decodable: {msg.value[:20]}")
    # Try to decode
    try:
        decoded = msg.value.decode('utf-8')
        print(f"  ✓ Decoded: {decoded}")
    except Exception as e:
        print(f"  ✗ Decode failed: {e}")

consumer.close()
