#!/usr/bin/env python3

from kafka import KafkaProducer, KafkaConsumer
import time

# First produce some messages
print("Producing messages...")
producer = KafkaProducer(bootstrap_servers='localhost:9092')

for i in range(5):
    msg = f"Test message {i}".encode('utf-8')
    producer.send('test-topic', msg)
    print(f"Sent: {msg}")

producer.flush()
producer.close()
print("Messages produced")

# Wait a bit for them to be available
time.sleep(1)

# Now try to consume
print("\nConsuming messages...")
consumer = KafkaConsumer(
    'test-topic',
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=5000,  # 5 second timeout
    group_id='test-group-fix'
)

count = 0
for message in consumer:
    print(f"Received: {message.value}")
    count += 1

print(f"\nTotal messages consumed: {count}")
consumer.close()