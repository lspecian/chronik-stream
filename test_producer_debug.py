#!/usr/bin/env python3
"""Debug producer issues with detailed logging."""

import logging
import socket
from kafka import KafkaProducer
from kafka.errors import KafkaError

# Enable debug logging
logging.basicConfig(
    level=logging.DEBUG,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)

def test_producer():
    """Test producer with detailed logging."""

    print("="*60)
    print("Testing Producer with Debug Logging")
    print("="*60)

    # First check if server is reachable
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(1)
        result = sock.connect_ex(('localhost', 9094))
        sock.close()

        if result != 0:
            print(f"❌ Cannot connect to localhost:9094")
            return
        else:
            print(f"✓ Server is reachable on localhost:9094")
    except Exception as e:
        print(f"❌ Connection check failed: {e}")
        return

    # Try to create producer
    try:
        print("\n🔧 Creating KafkaProducer...")
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9094'],
            api_version='auto',
            request_timeout_ms=10000,
            metadata_max_age_ms=10000,
            max_block_ms=10000
        )

        print("✓ Producer created successfully")

        # Get metadata
        metadata = producer._metadata
        print(f"\n📊 Metadata:")
        print(f"  Brokers: {metadata.brokers()}")
        print(f"  Topics: {metadata.topics()}")
        print(f"  Controller: {metadata.controller}")

        # Try to send a message
        print("\n📤 Attempting to send message...")
        future = producer.send('test-topic', b'test message')

        # Wait for result with timeout
        try:
            record_metadata = future.get(timeout=10)
            print(f"✓ Message sent successfully!")
            print(f"  Topic: {record_metadata.topic}")
            print(f"  Partition: {record_metadata.partition}")
            print(f"  Offset: {record_metadata.offset}")
        except KafkaError as e:
            print(f"❌ Failed to send message: {e}")
            print(f"  Error type: {type(e).__name__}")
            print(f"  Error details: {e.args}")

        producer.close()

    except Exception as e:
        print(f"\n❌ Producer creation/operation failed:")
        print(f"  Error type: {type(e).__name__}")
        print(f"  Error: {e}")

        import traceback
        print("\n📋 Full traceback:")
        traceback.print_exc()

if __name__ == "__main__":
    test_producer()