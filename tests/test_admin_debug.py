#!/usr/bin/env python3
"""Debug AdminClient connection issue"""

import logging
from kafka.admin import KafkaAdminClient
from kafka import KafkaProducer
import sys

# Enable debug logging
logging.basicConfig(level=logging.DEBUG)

def test_admin_versions():
    """Test different API version configurations"""

    configs = [
        ("Default", {}),
        ("v0.10.0", {"api_version": (0, 10, 0)}),
        ("v0.9.0", {"api_version": (0, 9, 0)}),
        ("Auto", {"api_version": None}),
    ]

    for name, config in configs:
        print(f"\nTesting with {name} configuration...")
        try:
            admin = KafkaAdminClient(
                bootstrap_servers='localhost:9092',
                client_id=f'test-admin-{name}',
                **config
            )
            print(f"✓ {name}: Connected successfully")

            # Try to get metadata
            metadata = admin._client.cluster
            print(f"  Cluster ID: {metadata.cluster_id}")
            print(f"  Controller: {metadata.controller}")

            admin.close()

        except Exception as e:
            print(f"✗ {name}: Failed - {e}")

if __name__ == "__main__":
    test_admin_versions()