#!/usr/bin/env python3

from kafka import KafkaProducer, KafkaConsumer
import time
import sys

# First produce a message to trigger topic creation
print("Creating producer...")
producer = KafkaProducer(bootstrap_servers='localhost:9092')

topic = "test-topic"
message = b"Hello from metadata fix test"

print(f"Sending message to topic '{topic}'...")
future = producer.send(topic, message)
result = future.get(timeout=10)
print(f"Message sent: {result}")

producer.flush()
producer.close()

# Wait a bit for propagation
time.sleep(1)

# Now try to consume
print(f"\nCreating consumer for topic '{topic}'...")
consumer = KafkaConsumer(
    topic,
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=5000,
    group_id='test-metadata-fix'
)

print("Consuming messages...")
count = 0
for msg in consumer:
    print(f"Received: {msg.value}")
    count += 1

print(f"Total messages consumed: {count}")
consumer.close()

if count > 0:
    print("SUCCESS: Consumer received messages!")
    sys.exit(0)
else:
    print("FAILURE: No messages received")
    sys.exit(1)