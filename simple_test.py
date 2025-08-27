#!/usr/bin/env python3
"""Simple test to check basic Chronik Stream functionality"""

from kafka import KafkaProducer
import json
import time

print("Testing Chronik Stream connection...")

try:
    # Create a producer
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        value_serializer=lambda v: json.dumps(v).encode('utf-8'),
        request_timeout_ms=5000,
        max_block_ms=5000
    )
    
    print("✅ Connected to Chronik Stream")
    
    # Try to send a message
    print("Sending test message...")
    topic = f"test-topic-{int(time.time())}"
    
    future = producer.send(topic, value={'test': 'message'})
    
    # Wait for message to be sent
    try:
        metadata = future.get(timeout=10)
        print(f"✅ Message sent successfully!")
        print(f"   Topic: {metadata.topic}")
        print(f"   Partition: {metadata.partition}")  
        print(f"   Offset: {metadata.offset}")
    except Exception as e:
        print(f"❌ Failed to send message: {e}")
    
    producer.close()
    
except Exception as e:
    print(f"❌ Failed to connect: {e}")
    print("   Make sure Chronik Stream is running on localhost:9092")