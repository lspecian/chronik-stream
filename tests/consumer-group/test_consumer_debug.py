#!/usr/bin/env python3

from kafka import KafkaConsumer
import time

print("Testing consumer group coordination with detailed debugging...")

# Test with shorter timeout to see exactly where it fails
consumer = KafkaConsumer(
    "test-topic",
    group_id="test-group-debug",
    bootstrap_servers="localhost:9092",
    auto_offset_reset="earliest",
    consumer_timeout_ms=5000,  # 5 second timeout
    fetch_min_bytes=1,
    fetch_max_wait_ms=1000
)

print("✅ Consumer created successfully")
print("✅ Group coordination completed (JoinGroup + SyncGroup)")
print("🔍 Now attempting to consume messages...")

try:
    count = 0
    start_time = time.time()
    for msg in consumer:
        elapsed = time.time() - start_time
        print(f"✅ Received message after {elapsed:.2f}s: {msg.value.decode()}")
        print(f"   Topic: {msg.topic}, Partition: {msg.partition}, Offset: {msg.offset}")
        count += 1
        if count >= 3:  # Stop after receiving 3 messages
            break

    if count > 0:
        print(f"🎉 SUCCESS: Consumed {count} messages successfully!")
    else:
        print("⚠️  No messages consumed within timeout period")

except Exception as e:
    print(f"❌ Error during consumption: {e}")
finally:
    consumer.close()
    print("✅ Consumer closed")