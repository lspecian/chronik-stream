#!/usr/bin/env python3
"""Test consumer group and Fetch functionality"""

import sys
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_consumer_group_fetch():
    """Test with consumer group coordination"""
    broker = 'localhost:9092'
    topic = f'test-cg-{uuid.uuid4().hex[:8]}'
    group = f'test-group-{uuid.uuid4().hex[:8]}'
    
    print(f"Testing with topic: {topic}, group: {group}")
    
    # First produce messages
    try:
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0)
        )
        
        messages_sent = []
        for i in range(5):
            message = f"Message {i}".encode('utf-8')
            future = producer.send(topic, value=message)
            metadata = future.get(timeout=10)
            messages_sent.append(message)
            print(f"✓ Sent message {i} to partition {metadata.partition} at offset {metadata.offset}")
        
        producer.flush()
        producer.close()
        print(f"✓ Produced {len(messages_sent)} messages")
        
    except Exception as e:
        print(f"✗ Failed to produce: {e}")
        return False
    
    # Wait for messages to be persisted
    time.sleep(2)
    
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
            consumer_timeout_ms=10000,  # 10 second timeout
            max_poll_records=10,
            fetch_min_bytes=1,
            fetch_max_wait_ms=500
        )
        print("✓ Consumer created")
        
        # Poll for messages
        messages_received = []
        print("Polling for messages...")
        
        # Try multiple polls
        for poll_attempt in range(3):
            print(f"Poll attempt {poll_attempt + 1}...")
            msg_batch = consumer.poll(timeout_ms=2000)
            
            if msg_batch:
                for topic_partition, messages in msg_batch.items():
                    print(f"  Got {len(messages)} messages from {topic_partition}")
                    for msg in messages:
                        messages_received.append(msg.value)
                        print(f"    Offset {msg.offset}: {msg.value}")
            else:
                print("  No messages in this poll")
        
        # Also try iterator approach
        print("Trying iterator approach...")
        start_time = time.time()
        for message in consumer:
            messages_received.append(message.value)
            print(f"✓ Received via iterator: offset={message.offset}, value={message.value}")
            if time.time() - start_time > 5:
                break
        
        consumer.close()
        
        if messages_received:
            print(f"✓ Received {len(messages_received)} messages total")
            return True
        else:
            print("✗ No messages received")
            return False
            
    except Exception as e:
        print(f"✗ Consumer failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    print("=" * 60)
    print("Consumer Group & Fetch Test")
    print("=" * 60)
    
    success = test_consumer_group_fetch()
    
    print("=" * 60)
    if success:
        print("✓ Test PASSED")
        sys.exit(0)
    else:
        print("✗ Test FAILED")
        sys.exit(1)

if __name__ == "__main__":
    main()