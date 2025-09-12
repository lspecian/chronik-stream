#!/usr/bin/env python3
"""Test that ALL messages are consumed, not just offset 0"""

import sys
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_full_consumption():
    """Test that all messages across multiple partitions are consumed"""
    broker = 'localhost:9092'
    topic = f'test-full-{uuid.uuid4().hex[:8]}'
    group = f'test-group-{uuid.uuid4().hex[:8]}'
    
    print(f"Testing with topic: {topic}, group: {group}")
    print("=" * 60)
    
    # Produce 10 messages per partition (3 partitions = 30 messages total)
    try:
        print("PHASE 1: Producing messages")
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0)
        )
        
        messages_sent = []
        for partition in range(3):
            for i in range(10):
                message = f"Partition-{partition}-Message-{i}"
                future = producer.send(topic, value=message.encode('utf-8'), partition=partition)
                metadata = future.get(timeout=10)
                messages_sent.append(message)
                print(f"  Sent to partition {metadata.partition} offset {metadata.offset}: {message}")
        
        producer.flush()
        producer.close()
        print(f"✓ Produced {len(messages_sent)} messages total")
        print()
        
    except Exception as e:
        print(f"✗ Failed to produce: {e}")
        return False
    
    # Wait for messages to be persisted
    time.sleep(2)
    
    # Consume all messages
    try:
        print("PHASE 2: Consuming messages (first poll)")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            group_id=group,
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            consumer_timeout_ms=10000,  # 10 second timeout
            max_poll_records=100,  # Get more records per poll
            fetch_min_bytes=1,
            fetch_max_wait_ms=500
        )
        
        messages_received = []
        poll_count = 0
        
        # Poll multiple times to ensure we get all messages
        while poll_count < 5:  # Try up to 5 polls
            poll_count += 1
            print(f"  Poll attempt #{poll_count}...")
            
            msg_batch = consumer.poll(timeout_ms=2000, max_records=100)
            
            if msg_batch:
                for topic_partition, messages in msg_batch.items():
                    print(f"    Got {len(messages)} messages from {topic_partition}")
                    for msg in messages:
                        message = msg.value.decode('utf-8')
                        messages_received.append(message)
                        print(f"      Offset {msg.offset}: {message}")
            else:
                print(f"    No messages in poll #{poll_count}")
                if len(messages_received) >= len(messages_sent):
                    break  # Got all messages
        
        consumer.close()
        
        print()
        print(f"RESULTS after first consumer:")
        print(f"  Messages sent: {len(messages_sent)}")
        print(f"  Messages received: {len(messages_received)}")
        
        if len(messages_received) != len(messages_sent):
            print(f"✗ Only received {len(messages_received)}/{len(messages_sent)} messages!")
            print(f"  Missing messages:")
            for msg in messages_sent:
                if msg not in messages_received:
                    print(f"    - {msg}")
            return False
        else:
            print(f"✓ Successfully received all {len(messages_received)} messages")
            
    except Exception as e:
        print(f"✗ Consumer failed: {e}")
        import traceback
        traceback.print_exc()
        return False
    
    # Test 2: Create a NEW consumer and verify it starts from committed offsets
    print()
    print("PHASE 3: Testing offset persistence with new consumer")
    time.sleep(2)
    
    try:
        consumer2 = KafkaConsumer(
            topic,
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            group_id=group,  # Same group - should use committed offsets
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            consumer_timeout_ms=3000,
            max_poll_records=100
        )
        
        messages_from_second = []
        poll_count = 0
        
        while poll_count < 3:
            poll_count += 1
            print(f"  Second consumer poll #{poll_count}...")
            
            msg_batch = consumer2.poll(timeout_ms=1000, max_records=100)
            
            if msg_batch:
                for topic_partition, messages in msg_batch.items():
                    print(f"    Got {len(messages)} messages from {topic_partition}")
                    for msg in messages:
                        message = msg.value.decode('utf-8')
                        messages_from_second.append(message)
                        print(f"      Offset {msg.offset}: {message}")
            else:
                print(f"    No messages (expected - offsets should be committed)")
                break
        
        consumer2.close()
        
        if len(messages_from_second) == 0:
            print(f"✓ Second consumer correctly started from committed offsets (no duplicate messages)")
        else:
            print(f"✗ Second consumer received {len(messages_from_second)} messages - offsets not persisted!")
            return False
            
    except Exception as e:
        print(f"✗ Second consumer failed: {e}")
        import traceback
        traceback.print_exc()
        return False
    
    # Test 3: Produce MORE messages and ensure new consumer gets only new ones
    print()
    print("PHASE 4: Testing incremental consumption")
    
    try:
        # Produce 5 more messages per partition
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0)
        )
        
        new_messages = []
        for partition in range(3):
            for i in range(5):
                message = f"New-Partition-{partition}-Message-{i}"
                future = producer.send(topic, value=message.encode('utf-8'), partition=partition)
                metadata = future.get(timeout=10)
                new_messages.append(message)
                print(f"  Sent new message to partition {metadata.partition}: {message}")
        
        producer.flush()
        producer.close()
        print(f"✓ Produced {len(new_messages)} new messages")
        
    except Exception as e:
        print(f"✗ Failed to produce new messages: {e}")
        return False
    
    time.sleep(2)
    
    # Consume only the NEW messages
    try:
        print()
        print("  Creating third consumer to get only new messages...")
        consumer3 = KafkaConsumer(
            topic,
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            group_id=group,  # Same group - should get only new messages
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            consumer_timeout_ms=5000,
            max_poll_records=100
        )
        
        new_messages_received = []
        poll_count = 0
        
        while poll_count < 3:
            poll_count += 1
            print(f"  Third consumer poll #{poll_count}...")
            
            msg_batch = consumer3.poll(timeout_ms=2000, max_records=100)
            
            if msg_batch:
                for topic_partition, messages in msg_batch.items():
                    print(f"    Got {len(messages)} messages from {topic_partition}")
                    for msg in messages:
                        message = msg.value.decode('utf-8')
                        new_messages_received.append(message)
                        print(f"      Offset {msg.offset}: {message}")
            else:
                print(f"    No more messages")
                if len(new_messages_received) >= len(new_messages):
                    break
        
        consumer3.close()
        
        print()
        print(f"FINAL RESULTS:")
        print(f"  New messages sent: {len(new_messages)}")
        print(f"  New messages received: {len(new_messages_received)}")
        
        # Check we got exactly the new messages
        if len(new_messages_received) != len(new_messages):
            print(f"✗ Received {len(new_messages_received)}/{len(new_messages)} new messages")
            return False
        
        # Verify they are the NEW messages, not old ones
        for msg in new_messages_received:
            if not msg.startswith("New-"):
                print(f"✗ Received old message that should have been committed: {msg}")
                return False
        
        print(f"✓ Successfully received exactly the {len(new_messages)} new messages")
        return True
        
    except Exception as e:
        print(f"✗ Third consumer failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    print("=" * 60)
    print("COMPREHENSIVE MESSAGE CONSUMPTION TEST")
    print("=" * 60)
    print()
    print("This test verifies:")
    print("1. ALL messages are consumed (not just offset 0)")
    print("2. Offset commits are persisted correctly")
    print("3. Subsequent consumers start from committed offsets")
    print("4. Incremental consumption works properly")
    print()
    
    success = test_full_consumption()
    
    print()
    print("=" * 60)
    if success:
        print("✓✓✓ ALL TESTS PASSED ✓✓✓")
        sys.exit(0)
    else:
        print("✗✗✗ TEST FAILED ✗✗✗")
        sys.exit(1)

if __name__ == "__main__":
    main()