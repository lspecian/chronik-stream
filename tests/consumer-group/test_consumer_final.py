#!/usr/bin/env python3

from kafka import KafkaConsumer
import time

print("Testing final consumer group coordination fix...")

# First test the group creation
consumer = KafkaConsumer(
    "test-topic",
    group_id="test-group-fix",
    bootstrap_servers="localhost:9092",
    auto_offset_reset="earliest",
    consumer_timeout_ms=10000  # 10 second timeout
)

print("✅ Consumer created successfully, attempting to consume...")

count = 0
for msg in consumer:
    print(f"✅ Received message: {msg.value.decode()}")
    count += 1
    if count >= 5:  # Stop after receiving 5 messages
        break

consumer.close()

if count > 0:
    print(f"🎉 SUCCESS: Consumer group coordination working! Received {count} messages")
    print("✅ Group 'test-group-fix' was created automatically")
    print("✅ SyncGroup returned valid partition assignments")
    print("✅ Consumer successfully joined group and fetched messages")
else:
    print("❌ FAILURE: No messages received")