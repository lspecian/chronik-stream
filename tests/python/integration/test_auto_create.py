#!/usr/bin/env python3
"""Test script to verify auto-topic creation in Chronik Stream"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import time
import sys

def test_auto_topic_creation():
    """Test that topics are auto-created when producing to non-existent topic"""
    
    bootstrap_servers = 'localhost:9092'
    test_topic = f'auto-test-topic-{int(time.time())}'
    
    print(f"Testing auto-topic creation with topic: {test_topic}")
    print(f"Connecting to: {bootstrap_servers}")
    
    try:
        # Create producer
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: v.encode('utf-8'),
            request_timeout_ms=10000,
            max_block_ms=10000
        )
        
        # Try to send message to non-existent topic
        print(f"Sending message to non-existent topic '{test_topic}'...")
        future = producer.send(test_topic, value='Test message for auto-created topic')
        
        # Wait for send to complete
        try:
            record_metadata = future.get(timeout=10)
            print(f"✅ Success! Message sent to auto-created topic '{test_topic}'")
            print(f"   Topic: {record_metadata.topic}")
            print(f"   Partition: {record_metadata.partition}")
            print(f"   Offset: {record_metadata.offset}")
            
            # Try to consume from the auto-created topic
            print(f"\nVerifying topic by consuming...")
            consumer = KafkaConsumer(
                test_topic,
                bootstrap_servers=bootstrap_servers,
                auto_offset_reset='earliest',
                consumer_timeout_ms=5000,
                value_deserializer=lambda m: m.decode('utf-8')
            )
            
            message_found = False
            for message in consumer:
                print(f"✅ Consumed message: {message.value}")
                message_found = True
                break
            
            consumer.close()
            
            if message_found:
                print("\n🎉 AUTO-TOPIC CREATION IS WORKING!")
                return True
            else:
                print("\n⚠️  Topic was created but no message was consumed")
                return False
                
        except KafkaError as e:
            print(f"❌ Failed to send message: {e}")
            print("   This likely means auto-topic creation is NOT working")
            return False
            
    except Exception as e:
        print(f"❌ Error during test: {e}")
        return False
    finally:
        producer.close()

if __name__ == '__main__':
    # Check if server is running
    print("=" * 60)
    print("AUTO-TOPIC CREATION TEST")
    print("=" * 60)
    
    success = test_auto_topic_creation()
    
    print("=" * 60)
    if success:
        print("TEST PASSED ✅")
        sys.exit(0)
    else:
        print("TEST FAILED ❌")
        sys.exit(1)