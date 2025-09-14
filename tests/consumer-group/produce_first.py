#!/usr/bin/env python3

from kafka import KafkaProducer
import time

print("Producing messages to test-topic...")

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda x: x.encode('utf-8')
)

# Send 5 test messages
for i in range(5):
    message = f"Test message {i} from producer"
    future = producer.send('test-topic', value=message)
    result = future.get(timeout=10)  # Wait for message to be sent
    print(f"✅ Sent message {i}: {message}")

producer.flush()
producer.close()
print("✅ All messages produced successfully!")