#!/usr/bin/env python3
"""
Test WAL replication by sending messages to leader and verifying they appear on follower.
"""
from kafka import KafkaProducer, KafkaConsumer
import time
import sys

def test_replication():
    print("🧪 Testing WAL Replication")
    print("=" * 60)

    # Connect to leader (port 9092)
    print("\n1. Connecting to leader (localhost:9092)...")
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0)
    )
    print("   ✅ Connected to leader")

    # Send test messages
    topic = 'test-replication'
    num_messages = 10

    print(f"\n2. Sending {num_messages} messages to topic '{topic}'...")
    for i in range(num_messages):
        message = f"Test message {i+1}".encode('utf-8')
        producer.send(topic, value=message)
        print(f"   → Sent: Test message {i+1}")

    producer.flush()
    print(f"   ✅ All {num_messages} messages sent to leader")

    # Wait for replication
    print("\n3. Waiting for replication to follower (3 seconds)...")
    time.sleep(3)

    # Connect to follower and verify
    print("\n4. Connecting to follower (localhost:9093) to verify replication...")
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers='localhost:9093',
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000,  # 5 second timeout
        api_version=(0, 10, 0)
    )

    # Read messages from follower
    received_messages = []
    print(f"   Reading messages from follower...")
    for message in consumer:
        received_messages.append(message.value.decode('utf-8'))
        print(f"   ✓ Received: {message.value.decode('utf-8')}")

    consumer.close()

    # Verify count
    print(f"\n5. Verification:")
    print(f"   Messages sent to leader: {num_messages}")
    print(f"   Messages on follower: {len(received_messages)}")

    if len(received_messages) == num_messages:
        print("\n✅ SUCCESS: All messages replicated correctly!")
        print("=" * 60)
        return 0
    else:
        print(f"\n❌ FAILED: Expected {num_messages} messages, got {len(received_messages)}")
        print("=" * 60)
        return 1

if __name__ == '__main__':
    try:
        sys.exit(test_replication())
    except Exception as e:
        print(f"\n❌ ERROR: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)
