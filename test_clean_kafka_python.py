#!/usr/bin/env python3
"""Test with kafka-python client on clean topic."""

from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import json
import time

def test_clean_topic():
    """Test producing and consuming on a fresh topic."""
    print("=== Testing Clean Topic with kafka-python ===")
    
    topic_name = "clean-test-topic"
    
    # Create admin client
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=['localhost:9092'],
            api_version=(2, 0, 0)
        )
        
        # Create new topic
        new_topic = NewTopic(
            name=topic_name,
            num_partitions=1,
            replication_factor=1
        )
        
        try:
            admin.create_topics([new_topic], validate_only=False)
            print(f"✓ Created topic: {topic_name}")
        except Exception as e:
            print(f"Topic may already exist: {e}")
        
        admin.close()
    except Exception as e:
        print(f"Admin setup failed: {e}")
        return False
    
    # Test producer
    print(f"\n=== Producing to {topic_name} ===")
    try:
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            api_version=(2, 0, 0),
            acks=1,  # Wait for leader ack
            batch_size=16384,  # Batch messages
            linger_ms=100,  # Wait up to 100ms to batch
        )
        
        # Send 20 messages
        for i in range(20):
            data = {'number': i, 'message': f'Clean test message {i}'}
            future = producer.send(topic_name, value=data, partition=0)
            
            if i % 5 == 0:
                # Force flush every 5 messages to see batching
                producer.flush()
                print(f"  Flushed after message {i}")
        
        # Final flush
        producer.flush()
        producer.close()
        print("✓ Produced 20 messages")
        
    except Exception as e:
        print(f"✗ Producer failed: {e}")
        return False
    
    # Wait a bit for segments to be written
    print("\nWaiting 2 seconds for segments to persist...")
    time.sleep(2)
    
    # Test consumer
    print(f"\n=== Consuming from {topic_name} ===")
    try:
        consumer = KafkaConsumer(
            topic_name,
            bootstrap_servers=['localhost:9092'],
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            group_id='clean-test-group',
            value_deserializer=lambda m: json.loads(m.decode('utf-8')) if m else None,
            api_version=(2, 0, 0),
            consumer_timeout_ms=5000
        )
        
        message_count = 0
        for message in consumer:
            if message_count < 5:  # Print first 5
                print(f"  Message {message.offset}: {message.value['message']}")
            message_count += 1
            
            if message_count >= 20:
                break
        
        consumer.close()
        
        if message_count == 20:
            print(f"✓ Consumed all 20 messages successfully!")
            return True
        else:
            print(f"⚠ Only consumed {message_count} messages")
            return False
            
    except Exception as e:
        print(f"✗ Consumer failed: {e}")
        return False

def main():
    if test_clean_topic():
        print("\n✓ Clean topic test PASSED - Chronik-Stream is working correctly!")
    else:
        print("\n✗ Clean topic test FAILED")

if __name__ == "__main__":
    main()