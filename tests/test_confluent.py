#!/usr/bin/env python3
"""Test with confluent-kafka library (what KSQLDB uses)"""

import sys

try:
    from confluent_kafka.admin import AdminClient
    from confluent_kafka import Producer
except ImportError:
    print("confluent-kafka not installed. Install with: pip install confluent-kafka")
    sys.exit(1)

def test_confluent_admin():
    """Test confluent-kafka AdminClient (used by KSQLDB)"""
    print("Testing confluent-kafka AdminClient...")

    try:
        # Configuration similar to what KSQLDB uses
        conf = {
            'bootstrap.servers': 'localhost:9092',
            'api.version.request': True,
            'api.version.fallback.ms': 0,
            'socket.timeout.ms': 5000,
            'metadata.max.age.ms': 5000,
        }

        admin = AdminClient(conf)

        # Try to get cluster metadata
        metadata = admin.list_topics(timeout=5)
        print(f"✓ AdminClient connected successfully")

        # Get broker info
        brokers = metadata.brokers
        print(f"✓ Brokers: {len(brokers)} broker(s) found")
        for broker_id, broker_metadata in brokers.items():
            print(f"  - Broker {broker_id}: {broker_metadata}")

        # List topics
        topics = metadata.topics
        print(f"✓ Topics: {len(topics)} topic(s) found")
        for topic_name in topics:
            print(f"  - {topic_name}")

        return True

    except Exception as e:
        print(f"✗ confluent-kafka AdminClient failed: {e}")
        return False

def test_confluent_producer():
    """Test confluent-kafka Producer"""
    print("\nTesting confluent-kafka Producer...")

    try:
        conf = {
            'bootstrap.servers': 'localhost:9092',
            'api.version.request': True,
        }

        producer = Producer(conf)

        # Get metadata
        metadata = producer.list_topics(timeout=5)
        print(f"✓ Producer connected")
        print(f"✓ Can retrieve metadata for {len(metadata.topics)} topic(s)")

        producer.flush()
        return True

    except Exception as e:
        print(f"✗ confluent-kafka Producer failed: {e}")
        return False

if __name__ == "__main__":
    admin_ok = test_confluent_admin()
    producer_ok = test_confluent_producer()

    if admin_ok and producer_ok:
        print("\n✓ confluent-kafka library works!")
        print("  This means KSQLDB should be able to connect")
        sys.exit(0)
    else:
        print("\n✗ confluent-kafka still has issues")
        sys.exit(1)