#!/usr/bin/env python3
"""Simple Kafka client test for Chronik Stream"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import json
import time

def test_chronik_kafka():
    print("Testing Chronik Stream Kafka compatibility...")
    
    # Test 1: Create producer
    try:
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            # Use Kafka message format v2 (required by Chronik)
            api_version=(2, 0, 0)
        )
        print("✓ Producer created successfully")
    except Exception as e:
        print(f"✗ Failed to create producer: {e}")
        return
    
    # Test 2: Send message
    try:
        future = producer.send('demo-topic', {'test': 'message', 'timestamp': time.time()})
        result = future.get(timeout=10)
        print(f"✓ Message sent successfully: {result}")
    except KafkaError as e:
        print(f"✗ Failed to send message: {e}")
        return
    except Exception as e:
        print(f"✗ Unexpected error sending message: {e}")
        return
    finally:
        producer.close()
    
    # Test 3: Create consumer
    try:
        consumer = KafkaConsumer(
            'demo-topic',
            bootstrap_servers=['localhost:9092'],
            auto_offset_reset='earliest',
            value_deserializer=lambda m: json.loads(m.decode('utf-8')),
            consumer_timeout_ms=5000
        )
        print("✓ Consumer created successfully")
    except Exception as e:
        print(f"✗ Failed to create consumer: {e}")
        return
    
    # Test 4: Consume messages
    try:
        messages = []
        for message in consumer:
            messages.append(message.value)
            print(f"✓ Received message: {message.value}")
            
        if not messages:
            print("✗ No messages received")
        else:
            print(f"✓ Total messages consumed: {len(messages)}")
    except Exception as e:
        print(f"✗ Error consuming messages: {e}")
    finally:
        consumer.close()

if __name__ == "__main__":
    test_chronik_kafka()