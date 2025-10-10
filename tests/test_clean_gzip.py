#!/usr/bin/env python3
"""Clean gzip test - single partition only"""
from kafka import KafkaProducer, KafkaConsumer, TopicPartition
import time

# Force all messages to partition 0 by using a key
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    compression_type='gzip',
    api_version=(0, 10, 0)
)

topic = 'test-clean-gzip'

print("Sending 10 messages to partition 0 with gzip compression...")
for i in range(10):
    msg = f"Message {i}"
    # Use same key to force same partition
    producer.send(topic, key=b'test-key', value=msg.encode('utf-8'))
    print(f"  Sent: {msg}")

producer.flush()
producer.close()

time.sleep(2)

print("\nConsuming from all partitions...")
consumer = KafkaConsumer(
    topic,
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=5000,
    api_version=(0, 10, 0)
)

count = 0
for msg in consumer:
    try:
        decoded = msg.value.decode('utf-8')
        print(f"  ✓ Offset {msg.offset}: {decoded}")
        count += 1
    except Exception as e:
        print(f"  ✗ Offset {msg.offset}: Decode failed - {e}")
        print(f"     Raw bytes (first 40): {msg.value[:40].hex()}")

consumer.close()

print(f"\n{'='*60}")
print(f"Result: {count}/10 messages consumed")
print(f"{'='*60}")
if count == 10:
    print("✅ SUCCESS")
else:
    print(f"❌ FAILED: Missing {10 - count} messages")
