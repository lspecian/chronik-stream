#!/usr/bin/env python3
"""Simple cluster produce/consume test"""

from kafka import KafkaProducer, KafkaConsumer
import time

BOOTSTRAP_SERVERS = ['localhost:9092', 'localhost:9093', 'localhost:9094']
TOPIC = 'bench-test'  # Use existing topic

print("=" * 60)
print("Simple Cluster Test - v2.2.8")
print("=" * 60)

# Produce 30 messages (10 per partition)
print("\n1. Producing 30 messages to 3 partitions...")
producer = KafkaProducer(
    bootstrap_servers=BOOTSTRAP_SERVERS,
    acks='all'
)

for partition in range(3):
    for i in range(10):
        key = f"p{partition}-{i}".encode()
        value = f"Message {i} for partition {partition}".encode()
        future = producer.send(TOPIC, key=key, value=value, partition=partition)
        metadata = future.get(timeout=10)
        if i == 0:
            print(f"✓ Partition {partition}: offset {metadata.offset}")

producer.flush()
producer.close()
print(f"✓ Produced 30 messages total")

# Wait for replication
print("\n2. Waiting for replication...")
time.sleep(3)

# Consume messages
print("\n3. Consuming messages...")
consumer = KafkaConsumer(
    TOPIC,
    bootstrap_servers=BOOTSTRAP_SERVERS,
    auto_offset_reset='earliest',
    consumer_timeout_ms=10000,
    group_id='simple-test-group'
)

count = 0
start = time.time()
for msg in consumer:
    count += 1
    if count <= 3:  # Show first 3
        print(f"✓ Read: partition={msg.partition}, offset={msg.offset}, key={msg.key.decode()}")
    if count >= 30:
        break

consumer.close()
elapsed = time.time() - start

print(f"\n✓ Consumed {count} messages in {elapsed:.2f}s")
print("\n" + "=" * 60)
if count >= 30:
    print("✓ SUCCESS: Cluster working correctly!")
else:
    print(f"✗ PARTIAL: Only consumed {count}/30 messages")
print("=" * 60)
