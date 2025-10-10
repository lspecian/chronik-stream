#!/usr/bin/env python3
"""Debug consumption to see what's being returned"""
from kafka import KafkaConsumer
import time

topic = 'batch-debug'

print("="*60)
print("CONSUME DEBUG TEST")
print("="*60)

consumer = KafkaConsumer(
    topic,
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=10000,
    api_version=(0, 10, 0),
    enable_auto_commit=False  # Manual offset control
)

print(f"\nConsuming from {topic}...")
print(f"Partitions assigned: {consumer.assignment()}")

# Get partition info
partitions = consumer.partitions_for_topic(topic)
print(f"Topic partitions: {partitions}")

# Get high watermark for each partition
for partition in partitions or []:
    from kafka import TopicPartition
    tp = TopicPartition(topic, partition)
    consumer.assign([tp])
    consumer.seek_to_beginning(tp)

    start_offset = consumer.position(tp)
    consumer.seek_to_end(tp)
    end_offset = consumer.position(tp)

    print(f"  Partition {partition}: start={start_offset}, end={end_offset}, messages={end_offset - start_offset}")

    # Reset to beginning
    consumer.seek_to_beginning(tp)

# Now consume all available messages
consumer.subscribe([topic])
consumer.seek_to_beginning()

count = 0
for message in consumer:
    count += 1
    print(f"  [{count}] Partition {message.partition}, Offset {message.offset}, Key={message.key}, Value={message.value.decode('utf-8')}")

consumer.close()

print(f"\nTotal consumed: {count} messages")
