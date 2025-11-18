#!/usr/bin/env python3
"""Quick test to verify cluster accepts produce/consume requests"""

from kafka import KafkaProducer, KafkaConsumer
import time
import sys

def test_produce_consume():
    print("Testing Chronik cluster...")

    # Create producer
    print("Creating producer...")
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
        acks='all',
        retries=3,
        request_timeout_ms=10000
    )

    # Produce a message
    topic = 'quick-test'
    message = b'Hello from quick test!'

    print(f"Producing message to topic '{topic}'...")
    try:
        future = producer.send(topic, message)
        record_metadata = future.get(timeout=10)
        print(f"✓ Message sent successfully!")
        print(f"  Topic: {record_metadata.topic}")
        print(f"  Partition: {record_metadata.partition}")
        print(f"  Offset: {record_metadata.offset}")
    except Exception as e:
        print(f"✗ Failed to produce message: {e}")
        sys.exit(1)
    finally:
        producer.close()

    # Give cluster time to replicate
    time.sleep(2)

    # Create consumer
    print("\nCreating consumer...")
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000,
        group_id='quick-test-group'
    )

    # Consume message
    print(f"Consuming messages from topic '{topic}'...")
    try:
        messages_consumed = 0
        for msg in consumer:
            print(f"✓ Received message:")
            print(f"  Topic: {msg.topic}")
            print(f"  Partition: {msg.partition}")
            print(f"  Offset: {msg.offset}")
            print(f"  Value: {msg.value.decode('utf-8')}")
            messages_consumed += 1
            break  # Just consume one message

        if messages_consumed > 0:
            print(f"\n✓ SUCCESS: Cluster is working correctly!")
            return True
        else:
            print(f"\n✗ FAILED: No messages consumed")
            return False
    except Exception as e:
        print(f"✗ Failed to consume: {e}")
        return False
    finally:
        consumer.close()

if __name__ == '__main__':
    success = test_produce_consume()
    sys.exit(0 if success else 1)
