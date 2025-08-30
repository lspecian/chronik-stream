#!/usr/bin/env python3
"""Detailed test to debug Fetch API issues"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import time
import logging
import sys

# Enable detailed logging
logging.basicConfig(
    level=logging.DEBUG,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)

def test_fetch_detailed():
    # First produce some messages
    print("\n=== PRODUCING MESSAGES ===")
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        client_id='test-producer-detailed',
    )
    
    for i in range(3):
        msg = f"test-message-{i}".encode('utf-8')
        future = producer.send('chronik-default', value=msg, partition=0)
        record = future.get(timeout=10)
        print(f"Produced message {i} at offset {record.offset}")
    
    producer.flush()
    producer.close()
    print("Producer closed, messages written")
    
    # Now try to consume with detailed logging
    print("\n=== CONSUMING MESSAGES ===")
    consumer = KafkaConsumer(
        'chronik-default',
        bootstrap_servers=['localhost:9092'],
        client_id='test-consumer-detailed',
        group_id='test-group-detailed',
        auto_offset_reset='earliest',
        enable_auto_commit=False,  # Manual commit for testing
        consumer_timeout_ms=10000,  # 10 second timeout
        fetch_min_bytes=1,  # Fetch even 1 byte
        fetch_max_wait_ms=500,  # Don't wait long
        max_poll_records=10,
        session_timeout_ms=10000,
        heartbeat_interval_ms=3000,
    )
    
    print(f"Consumer created, subscribed to: {consumer.subscription()}")
    print(f"Consumer assignment: {consumer.assignment()}")
    
    # Poll for assignment
    print("\nPolling for partition assignment...")
    consumer.poll(timeout_ms=1000)
    print(f"After poll - assignment: {consumer.assignment()}")
    
    # Get partition info
    partitions = consumer.partitions_for_topic('chronik-default')
    print(f"Partitions for chronik-default: {partitions}")
    
    # Check position and committed offset
    from kafka import TopicPartition
    tp = TopicPartition('chronik-default', 0)
    
    if consumer.assignment():
        position = consumer.position(tp)
        print(f"Current position for partition 0: {position}")
        
        # Try to seek to beginning
        consumer.seek_to_beginning(tp)
        position = consumer.position(tp)
        print(f"Position after seek to beginning: {position}")
    
    # Try to consume messages with explicit polling
    print("\nAttempting to consume messages...")
    messages_consumed = 0
    
    for attempt in range(5):
        print(f"\nPoll attempt {attempt + 1}...")
        records = consumer.poll(timeout_ms=2000, max_records=10)
        
        if records:
            print(f"Got records: {records}")
            for topic_partition, messages in records.items():
                print(f"  Topic: {topic_partition.topic}, Partition: {topic_partition.partition}")
                for msg in messages:
                    print(f"    Offset: {msg.offset}, Value: {msg.value}")
                    messages_consumed += 1
        else:
            print("  No records returned")
            
            # Check if we're assigned
            if not consumer.assignment():
                print("  Consumer not assigned to any partitions!")
                # Try to manually assign
                consumer.assign([tp])
                print(f"  Manually assigned to: {consumer.assignment()}")
    
    print(f"\nTotal messages consumed: {messages_consumed}")
    
    # Check end offsets
    end_offsets = consumer.end_offsets([tp])
    print(f"End offsets: {end_offsets}")
    
    # Check beginning offsets  
    beginning_offsets = consumer.beginning_offsets([tp])
    print(f"Beginning offsets: {beginning_offsets}")
    
    consumer.close()
    return messages_consumed > 0

if __name__ == "__main__":
    try:
        success = test_fetch_detailed()
        sys.exit(0 if success else 1)
    except Exception as e:
        print(f"Error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)