#!/usr/bin/env python3

from kafka import KafkaProducer, KafkaConsumer
import json
import time

print("Testing Fetch Handler Fix")
print("=" * 50)

# Step 1: Produce some messages
print("\n1. Producing messages...")
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda v: json.dumps(v).encode()
)

for i in range(5):
    msg = {'id': i, 'message': f'Test message {i}'}
    future = producer.send('test-topic', value=msg)
    result = future.get(timeout=10)
    print(f"   ✅ Sent message {i}: {msg}")

producer.flush()
producer.close()
print("   ✅ All messages produced successfully")

# Step 2: Try to consume messages
print("\n2. Attempting to consume messages...")
consumer = KafkaConsumer(
    'test-topic',
    group_id='test-fetch-group',
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    enable_auto_commit=False,
    consumer_timeout_ms=5000,  # 5 second timeout
    value_deserializer=lambda v: json.loads(v.decode())
)

print("   ✅ Consumer created successfully")
print("   🔍 Waiting for messages...")

messages_received = []
try:
    for msg in consumer:
        messages_received.append(msg)
        print(f"   ✅ Received: offset={msg.offset}, value={msg.value}")
        if len(messages_received) >= 5:
            break
except Exception as e:
    print(f"   ❌ Error: {e}")

consumer.close()

# Step 3: Verify results
print("\n3. Results:")
print(f"   Messages sent: 5")
print(f"   Messages received: {len(messages_received)}")

if len(messages_received) == 5:
    print("\n🎉 SUCCESS: Fetch handler is working correctly!")
else:
    print(f"\n❌ FAILURE: Expected 5 messages, got {len(messages_received)}")
    print("   The fetch handler is not returning messages correctly.")