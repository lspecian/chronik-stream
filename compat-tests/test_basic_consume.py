#!/usr/bin/env python3
"""Test basic consumer functionality with single message per partition"""

import sys
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_basic_consume():
    """Test basic produce and consume with consumer groups"""
    broker = 'localhost:9092'
    topic = f'test-basic-{uuid.uuid4().hex[:8]}'
    group = f'test-group-{uuid.uuid4().hex[:8]}'
    
    print(f"Testing with topic: {topic}, group: {group}")
    
    # First produce a single message per partition
    try:
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0)
        )
        
        # Send one message to each partition (3 partitions)
        for i in range(3):
            message = f"Message {i}".encode('utf-8')
            future = producer.send(topic, value=message, partition=i)
            metadata = future.get(timeout=10)
            print(f"✓ Sent message to partition {metadata.partition} at offset {metadata.offset}")
        
        producer.flush()
        producer.close()
        print("✓ Produced 3 messages (one per partition)")
        
    except Exception as e:
        print(f"✗ Failed to produce: {e}")
        return False
    
    # Wait for messages to be persisted
    time.sleep(1)
    
    # Try to consume with consumer group
    try:
        print(f"Creating consumer in group '{group}'...")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            group_id=group,
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            consumer_timeout_ms=5000,  # 5 second timeout
            max_poll_records=10,
            fetch_min_bytes=1,
            fetch_max_wait_ms=500
        )
        print("✓ Consumer created")
        
        # Poll for messages once
        messages_received = []
        print("Polling for messages...")
        
        msg_batch = consumer.poll(timeout_ms=2000, max_records=10)
        
        if msg_batch:
            for topic_partition, messages in msg_batch.items():
                print(f"  Got {len(messages)} messages from {topic_partition}")
                for msg in messages:
                    messages_received.append(msg.value)
                    print(f"    Offset {msg.offset}: {msg.value}")
        else:
            print("  No messages in poll")
        
        consumer.close()
        
        if len(messages_received) == 3:
            print(f"✓ Successfully received all 3 messages")
            return True
        else:
            print(f"✗ Expected 3 messages, got {len(messages_received)}")
            return False
            
    except Exception as e:
        print(f"✗ Consumer failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    print("=" * 60)
    print("Basic Consumer Group Test")
    print("=" * 60)
    
    success = test_basic_consume()
    
    print("=" * 60)
    if success:
        print("✓ Test PASSED")
        sys.exit(0)
    else:
        print("✗ Test FAILED")
        sys.exit(1)

if __name__ == "__main__":
    main()