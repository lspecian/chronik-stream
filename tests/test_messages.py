#!/usr/bin/env python3

import time
import sys
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic

def test_messages():
    print("Testing 10 messages with Chronik Server on port 9092...")

    try:
        # Test 1: Producer - Send 10 messages
        print("\n1. Testing Producer API - Sending 10 messages...")
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            client_id='test_producer',
            value_serializer=lambda x: x.encode('utf-8'),
            api_version=(0, 10, 0)  # Use specific API version
        )

        for i in range(10):
            message = f'Test message {i+1}'
            print(f"Sending: {message}")
            future = producer.send('test-messages', message)
            result = future.get(timeout=10)
            print(f"✅ Sent message {i+1}: offset {result.offset}")

        producer.flush()
        producer.close()
        print("✅ All 10 messages sent successfully")

        # Wait a moment for messages to be available
        time.sleep(2)

        # Test 2: Consumer - Read messages
        print("\n2. Testing Consumer API - Reading messages...")
        consumer = KafkaConsumer(
            'test-messages',
            bootstrap_servers=['localhost:9092'],
            client_id='test_consumer',
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            group_id='test-group',
            value_deserializer=lambda x: x.decode('utf-8'),
            consumer_timeout_ms=10000,  # 10 second timeout
            api_version=(0, 10, 0)  # Use specific API version
        )

        message_count = 0
        received_messages = []

        print("Waiting for messages...")
        for message in consumer:
            message_count += 1
            received_messages.append(message.value)
            print(f"✅ Received message {message_count}: {message.value} (offset: {message.offset})")
            if message_count >= 10:
                break

        consumer.close()

        print(f"\n🎉 Test completed! Sent 10 messages, received {message_count} messages")

        if message_count == 10:
            print("✅ All messages received successfully!")
            print("✅ Protocol implementation working correctly!")
            return True
        else:
            print(f"❌ Expected 10 messages but received {message_count}")
            return False

    except Exception as e:
        print(f"❌ Test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    success = test_messages()
    sys.exit(0 if success else 1)