#!/usr/bin/env python3
"""Test MetadataRequest v0 compatibility with AdminClient"""

import sys
from kafka.admin import KafkaAdminClient
from kafka import KafkaProducer
from kafka.errors import KafkaError

def test_admin_client():
    """Test AdminClient connection with MetadataRequest v0"""
    print("Testing AdminClient connection...")

    try:
        # AdminClient typically uses MetadataRequest v0
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='test-admin',
            api_version=(0, 10, 0)  # Force older API version
        )

        print("✓ AdminClient connected successfully")

        # Try to get metadata
        metadata = admin._client.cluster
        if hasattr(metadata, 'cluster_id'):
            print(f"✓ Cluster ID: {metadata.cluster_id}")
        print(f"✓ Brokers: {metadata.brokers()}")

        admin.close()
        return True

    except Exception as e:
        print(f"✗ AdminClient failed: {e}")
        return False

def test_producer_metadata():
    """Test Producer metadata update"""
    print("\nTesting Producer metadata update...")

    try:
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            api_version=(0, 10, 0),
            request_timeout_ms=5000,
            metadata_max_age_ms=1000
        )

        print("✓ Producer connected")

        # Force metadata update
        partitions = producer.partitions_for('test-topic')
        print(f"✓ Got partitions for test-topic: {partitions}")

        producer.close()
        return True

    except Exception as e:
        print(f"✗ Producer metadata failed: {e}")
        return False

if __name__ == "__main__":
    admin_ok = test_admin_client()
    producer_ok = test_producer_metadata()

    if admin_ok and producer_ok:
        print("\n✓ All tests passed!")
        sys.exit(0)
    else:
        print("\n✗ Some tests failed")
        sys.exit(1)