#!/usr/bin/env python3
"""Test Kafka produce and fetch operations"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import time
import sys

def test_produce_fetch():
    # Test producing messages
    print("Creating producer...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            client_id='test-producer',
            value_serializer=lambda v: v.encode('utf-8') if isinstance(v, str) else v
        )
        
        # Send some test messages
        print("Sending test messages...")
        for i in range(5):
            message = f"test-message-{i}"
            future = producer.send('chronik-default', value=message, partition=0)
            # Wait for send to complete
            try:
                record_metadata = future.get(timeout=10)
                print(f"Sent: {message} to partition {record_metadata.partition} at offset {record_metadata.offset}")
            except KafkaError as e:
                print(f"Failed to send message: {e}")
                
        producer.flush()
        producer.close()
        print("Producer closed.")
        
    except Exception as e:
        print(f"Producer error: {e}")
        return False
    
    # Give time for messages to be written
    time.sleep(1)
    
    # Test consuming messages
    print("\nCreating consumer...")
    try:
        consumer = KafkaConsumer(
            'chronik-default',
            bootstrap_servers=['localhost:9092'],
            client_id='test-consumer',
            group_id='test-group',
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            consumer_timeout_ms=5000
        )
        
        print("Consuming messages...")
        message_count = 0
        for message in consumer:
            print(f"Received: {message.value.decode('utf-8')} from partition {message.partition} at offset {message.offset}")
            message_count += 1
            
        print(f"Total messages consumed: {message_count}")
        consumer.close()
        
        return message_count > 0
        
    except Exception as e:
        print(f"Consumer error: {e}")
        return False

if __name__ == "__main__":
    success = test_produce_fetch()
    sys.exit(0 if success else 1)