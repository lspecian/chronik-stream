#!/usr/bin/env python3
"""Regression test: Ensure multiple messages per partition are stored and retrievable"""

import sys
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_regression():
    """Test that 3 messages to same partition are all retrievable"""
    broker = 'localhost:9092'
    topic = f'test-regression-{uuid.uuid4().hex[:8]}'
    
    print("=" * 60)
    print("REGRESSION TEST: Multiple Messages Per Partition")
    print("=" * 60)
    print(f"Topic: {topic}")
    print()
    
    # PHASE 1: Produce 3 messages to partition 0
    try:
        print("PHASE 1: Producing 3 messages to partition 0")
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0)
        )
        
        expected_messages = []
        for i in range(3):
            message = f"Message-{i}".encode('utf-8')
            future = producer.send(topic, value=message, partition=0)
            metadata = future.get(timeout=10)
            expected_messages.append(message.decode('utf-8'))
            print(f"  ✓ Sent to partition {metadata.partition} offset {metadata.offset}: {message.decode('utf-8')}")
        
        producer.flush()
        producer.close()
        print(f"  ✓ Successfully produced {len(expected_messages)} messages")
        print()
        
    except Exception as e:
        print(f"  ✗ Failed to produce: {e}")
        return False
    
    # Wait for persistence
    time.sleep(2)
    
    # PHASE 2: Consume and validate all messages
    try:
        print("PHASE 2: Consuming messages from partition 0")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            group_id=f'test-group-{uuid.uuid4().hex[:8]}',
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            max_poll_records=10
        )
        
        received_messages = []
        poll_attempts = 0
        max_polls = 3
        
        while poll_attempts < max_polls:
            poll_attempts += 1
            print(f"  Poll attempt #{poll_attempts}...")
            
            msg_batch = consumer.poll(timeout_ms=2000, max_records=10)
            
            if msg_batch:
                for topic_partition, messages in msg_batch.items():
                    print(f"    Got {len(messages)} messages from {topic_partition}")
                    for msg in messages:
                        message = msg.value.decode('utf-8')
                        received_messages.append(message)
                        print(f"      ✓ Offset {msg.offset}: {message}")
                        
                        # Validate offset matches expected
                        expected_idx = int(message.split('-')[1])
                        if msg.offset != expected_idx:
                            print(f"      ✗ ERROR: Expected offset {expected_idx}, got {msg.offset}")
            else:
                print(f"    No messages in poll #{poll_attempts}")
                if len(received_messages) >= len(expected_messages):
                    break
        
        consumer.close()
        print()
        
        # PHASE 3: Validate results
        print("PHASE 3: Validation")
        print(f"  Expected messages: {expected_messages}")
        print(f"  Received messages: {received_messages}")
        print()
        
        if len(received_messages) != len(expected_messages):
            print(f"  ✗ FAIL: Expected {len(expected_messages)} messages, got {len(received_messages)}")
            missing = [msg for msg in expected_messages if msg not in received_messages]
            if missing:
                print(f"  Missing messages: {missing}")
            return False
        
        # Validate content matches
        for i, expected in enumerate(expected_messages):
            if i >= len(received_messages):
                print(f"  ✗ FAIL: Missing message at position {i}: {expected}")
                return False
            if received_messages[i] != expected:
                print(f"  ✗ FAIL: Message mismatch at position {i}")
                print(f"    Expected: {expected}")
                print(f"    Received: {received_messages[i]}")
                return False
        
        print(f"  ✓ SUCCESS: All {len(expected_messages)} messages received correctly!")
        return True
        
    except Exception as e:
        print(f"  ✗ Consumer failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    success = test_regression()
    
    print()
    print("=" * 60)
    if success:
        print("✓✓✓ REGRESSION TEST PASSED ✓✓✓")
        print("Multiple messages per partition are correctly stored and retrieved")
        sys.exit(0)
    else:
        print("✗✗✗ REGRESSION TEST FAILED ✗✗✗")
        print("BUG: Messages are being lost or overwritten")
        sys.exit(1)

if __name__ == "__main__":
    main()