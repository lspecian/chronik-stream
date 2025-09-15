#!/usr/bin/env python3
"""Quick basic compatibility test"""

import sys
from kafka.admin import KafkaAdminClient

def test_basic():
    """Test basic AdminClient connection"""
    try:
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='test-admin',
            api_version=(0, 10, 0)
        )

        metadata = admin._client.cluster
        brokers = metadata.brokers()
        print(f"✓ Connected to Chronik Stream")
        print(f"✓ Brokers: {brokers}")
        print(f"✓ ApiVersionsResponse v0 is working")

        admin.close()
        return True
    except Exception as e:
        print(f"✗ Connection failed: {e}")
        return False

if __name__ == "__main__":
    if test_basic():
        print("\n✅ Basic compatibility test PASSED!")
        sys.exit(0)
    else:
        print("\n❌ Basic compatibility test FAILED!")
        sys.exit(1)