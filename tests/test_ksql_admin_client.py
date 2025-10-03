#!/usr/bin/env python3
"""Test KSQL AdminClient behavior with Chronik"""

from confluent_kafka.admin import AdminClient, ConfigResource
import time
import sys

def test_admin_client():
    """Test AdminClient operations that KSQL uses"""

    # Create AdminClient (this is what KSQL does)
    admin = AdminClient({
        'bootstrap.servers': 'localhost:9092',
        'client.id': 'test-admin-client',
        'request.timeout.ms': 5000,
        'socket.timeout.ms': 5000,
        'metadata.max.age.ms': 1000,
    })

    print("AdminClient created")

    # Try to get cluster metadata (listNodes equivalent)
    try:
        print("\nAttempting to get cluster metadata...")
        metadata = admin.list_topics(timeout=5)
        print(f"Success! Got metadata for {len(metadata.topics)} topics")

        # Print brokers (nodes)
        print(f"\nBrokers (nodes): {len(metadata.brokers)}")
        for broker_id, broker in metadata.brokers.items():
            print(f"  Broker {broker_id}: {broker.host}:{broker.port}")

        # Print cluster ID
        print(f"\nCluster ID: {metadata.cluster_id}")
        print(f"Controller ID: {metadata.controller_id}")

    except Exception as e:
        print(f"Error getting metadata: {e}")
        return False

    # Try to describe cluster configuration
    try:
        print("\nAttempting to describe cluster config...")
        # This uses DescribeConfigs API
        resource = ConfigResource('broker', '1')
        future = admin.describe_configs([resource])

        # Wait for result with timeout
        result = list(future.values())[0].result(timeout=5)
        print(f"Got config with {len(result)} entries")

    except Exception as e:
        print(f"Error describing config: {e}")
        # This is expected to fail if DescribeConfigs isn't implemented

    return True

if __name__ == "__main__":
    success = test_admin_client()
    sys.exit(0 if success else 1)