#!/usr/bin/env python3
"""
Test script to verify v0.7.7 fix for segment metadata registration.
Sends 10 messages and verifies they can all be retrieved.
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import json
import time
import sys

def test_message_retrieval():
    """Test sending and retrieving 10 messages"""
    print("=" * 60)
    print("Testing Chronik Stream v0.7.7 Fix - 10 Message Test")
    print("=" * 60)
    
    topic = 'test-topic'
    messages_to_send = 10
    
    # Create producer
    try:
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            api_version=(0, 10, 0),  # Use v2 protocol
            acks='all',
            retries=3,
            linger_ms=10  # Small batching for testing
        )
        print(f"✓ Producer created successfully")
    except Exception as e:
        print(f"✗ Failed to create producer: {e}")
        return False
    
    # Send messages
    print(f"\nSending {messages_to_send} messages...")
    sent_messages = []
    
    for i in range(messages_to_send):
        message = {
            'id': i,
            'test': f'Message {i}',
            'timestamp': time.time()
        }
        
        try:
            future = producer.send(topic, value=message, partition=0)
            record_metadata = future.get(timeout=10)
            sent_messages.append({
                'id': i,
                'offset': record_metadata.offset,
                'partition': record_metadata.partition
            })
            print(f"  ✓ Message {i} sent - offset: {record_metadata.offset}")
        except Exception as e:
            print(f"  ✗ Failed to send message {i}: {e}")
            return False
    
    # Flush to ensure all messages are sent
    producer.flush()
    print(f"\n✓ All {messages_to_send} messages sent and flushed")
    
    # Small delay to ensure processing
    time.sleep(2)
    
    # Create consumer and retrieve messages
    print(f"\nRetrieving messages...")
    try:
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=['localhost:9092'],
            api_version=(0, 10, 0),
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=5000,
            value_deserializer=lambda m: json.loads(m.decode('utf-8'))
        )
        print(f"✓ Consumer created successfully")
    except Exception as e:
        print(f"✗ Failed to create consumer: {e}")
        return False
    
    # Retrieve and count messages
    retrieved_messages = []
    retrieved_ids = set()
    
    for message in consumer:
        if message.value and 'id' in message.value:
            msg_id = message.value['id']
            retrieved_messages.append(message)
            retrieved_ids.add(msg_id)
            print(f"  ✓ Retrieved message {msg_id} from offset {message.offset}")
    
    consumer.close()
    producer.close()
    
    # Verify results
    print(f"\n" + "=" * 60)
    print("RESULTS:")
    print(f"  Messages sent:     {messages_to_send}")
    print(f"  Messages retrieved: {len(retrieved_messages)}")
    print(f"  Success rate:      {len(retrieved_messages)/messages_to_send*100:.1f}%")
    
    # Check for missing messages
    missing_ids = set(range(messages_to_send)) - retrieved_ids
    if missing_ids:
        print(f"\n  ✗ MISSING MESSAGES: {sorted(missing_ids)}")
    else:
        print(f"\n  ✓ ALL MESSAGES RETRIEVED SUCCESSFULLY!")
    
    # Determine test status
    if len(retrieved_messages) == messages_to_send:
        print(f"\n✅ TEST PASSED: v0.7.7 fix working correctly!")
        return True
    else:
        print(f"\n❌ TEST FAILED: Message loss detected ({100 - len(retrieved_messages)/messages_to_send*100:.1f}% loss)")
        return False

if __name__ == "__main__":
    success = test_message_retrieval()
    sys.exit(0 if success else 1)