#!/usr/bin/env python3
"""Minimal test to debug GroupCommitWal - produce just 1 message."""

from kafka import KafkaProducer
import time

def test_minimal():
    print("🧪 Starting minimal produce test")

    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0),
        acks=1,  # Wait for leader ack (should trigger GroupCommitWal)
        request_timeout_ms=5000
    )

    print("✅ Producer created")

    # Send ONE message
    print("📤 Sending 1 message with acks=1...")
    future = producer.send('test-topic', b'Hello WAL!')

    # Wait for ack
    try:
        metadata = future.get(timeout=10)
        print(f"✅ Message sent successfully! Offset: {metadata.offset}")
    except Exception as e:
        print(f"❌ Failed to send message: {e}")
        return False

    producer.close()
    print("✅ Test completed - check logs for GroupCommitWal debug output")
    return True

if __name__ == '__main__':
    test_minimal()
