#!/usr/bin/env python3

from kafka import KafkaProducer, KafkaConsumer, TopicPartition
import json
import time

print("Testing Direct Fetch Without Groups")
print("=" * 50)

# Step 1: Produce a message
print("\n1. Producing a test message...")
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda v: json.dumps(v).encode()
)

msg = {'test': 'message', 'timestamp': time.time()}
future = producer.send('fetch-test-topic', value=msg)
result = future.get(timeout=10)
print(f"   ✅ Sent message to partition {result.partition} at offset {result.offset}")
producer.close()

# Step 2: Try to consume using assign (no groups)
print("\n2. Creating consumer with manual partition assignment...")
consumer = KafkaConsumer(
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    enable_auto_commit=False,
    value_deserializer=lambda v: json.loads(v.decode())
)

# Manually assign partition
tp = TopicPartition('fetch-test-topic', 0)
consumer.assign([tp])
consumer.seek_to_beginning(tp)

print(f"   ✅ Assigned to {tp}")
print(f"   🔍 Attempting to fetch messages...")

# Try to fetch
messages = []
timeout_time = time.time() + 5  # 5 second timeout
while time.time() < timeout_time:
    msg_batch = consumer.poll(timeout_ms=100)
    if msg_batch:
        for topic_partition, msgs in msg_batch.items():
            for msg in msgs:
                messages.append(msg)
                print(f"   ✅ Got message: offset={msg.offset}, value={msg.value}")
        if messages:
            break

consumer.close()

# Step 3: Results
print(f"\n3. Results: Got {len(messages)} messages")
if messages:
    print("🎉 SUCCESS: Fetch is working!")
else:
    print("❌ FAILURE: Fetch is not returning messages")