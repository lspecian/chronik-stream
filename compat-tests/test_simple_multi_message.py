#!/usr/bin/env python3
"""Simple test to verify multiple messages can be consumed from a single partition"""

import sys
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer

def test_multi_message():
    """Test that multiple messages from one partition are consumed"""
    broker = 'localhost:9092'
    topic = f'test-multi-{uuid.uuid4().hex[:8]}'
    group = f'test-group-{uuid.uuid4().hex[:8]}'
    
    print(f"Testing with topic: {topic}, group: {group}")
    print("=" * 60)
    
    # Produce 5 messages to partition 0
    try:
        print("Producing 5 messages to partition 0...")
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0)
        )
        
        for i in range(5):
            message = f"Message-{i}"
            future = producer.send(topic, value=message.encode('utf-8'), partition=0)
            metadata = future.get(timeout=10)
            print(f"  Sent to partition {metadata.partition} offset {metadata.offset}: {message}")
        
        producer.flush()
        producer.close()
        print("✓ Produced 5 messages")
        print()
        
    except Exception as e:
        print(f"✗ Failed to produce: {e}")
        return False
    
    # Wait for messages to be persisted
    time.sleep(2)
    
    # Consume messages
    try:
        print("Consuming messages...")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            group_id=group,
            auto_offset_reset='earliest',
            enable_auto_commit=False,  # Manual commit for testing
            max_poll_records=10,
            fetch_min_bytes=1,
            fetch_max_wait_ms=500
        )
        
        messages_received = []
        
        # Poll once
        print("  Polling for messages...")
        msg_batch = consumer.poll(timeout_ms=5000, max_records=10)
        
        if msg_batch:
            for topic_partition, messages in msg_batch.items():
                print(f"    Got {len(messages)} messages from {topic_partition}")
                for msg in messages:
                    message = msg.value.decode('utf-8')
                    messages_received.append(message)
                    print(f"      Offset {msg.offset}: {message}")
        else:
            print("    No messages received")
        
        # Manually commit offsets
        consumer.commit()
        consumer.close()
        
        print()
        print(f"RESULTS:")
        print(f"  Messages sent: 5")
        print(f"  Messages received: {len(messages_received)}")
        
        if len(messages_received) != 5:
            print(f"✗ Only received {len(messages_received)}/5 messages!")
            for i in range(5):
                msg = f"Message-{i}"
                if msg not in messages_received:
                    print(f"    Missing: {msg}")
            return False
        else:
            print(f"✓ Successfully received all 5 messages")
            return True
            
    except Exception as e:
        print(f"✗ Consumer failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    print("=" * 60)
    print("SIMPLE MULTI-MESSAGE TEST")
    print("=" * 60)
    print()
    
    success = test_multi_message()
    
    print()
    print("=" * 60)
    if success:
        print("✓ TEST PASSED")
        sys.exit(0)
    else:
        print("✗ TEST FAILED")
        sys.exit(1)

if __name__ == "__main__":
    main()