#!/usr/bin/env python3
"""Simple consume test with more timeout"""
from kafka import KafkaConsumer
import time

topic = 'batch-debug'

print("Consuming from batch-debug...")

consumer = KafkaConsumer(
    topic,
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=30000,  # 30 seconds timeout
    api_version=(0, 10, 0)
)

count = 0
for message in consumer:
    count += 1
    print(f"  [{count}] Partition {message.partition}, Offset {message.offset}: {message.value.decode('utf-8')}")

consumer.close()

print(f"\nTotal: {count}/10 messages")
if count == 10:
    print("✅ SUCCESS!")
else:
    print(f"❌ FAILED: Missing {10 - count} messages")
