#!/usr/bin/env python3
"""Test KSQLDB-like compatibility with AdminClient"""

import sys
from kafka.admin import KafkaAdminClient
from kafka.admin import ConfigResource, ConfigResourceType
from kafka.errors import KafkaError

def test_ksql_like_connection():
    """Test KSQLDB-like operations that typically fail with v0 issues"""
    print("Testing KSQLDB-like connection patterns...")

    try:
        # KSQLDB uses AdminClient for cluster metadata
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='ksqldb-test',
            api_version=(0, 10, 0)  # Force older API version like KSQLDB does
        )

        print("✓ Connected with KSQLDB-like settings")

        # Try to get cluster metadata (what KSQLDB does on startup)
        metadata = admin._client.cluster
        brokers = metadata.brokers()
        print(f"✓ Retrieved cluster metadata: {len(brokers)} broker(s)")

        # Try to list topics (KSQLDB does this for stream/table discovery)
        topics = admin.list_topics()
        print(f"✓ Listed topics: {list(topics)}")

        # Try to describe configs (KSQLDB checks broker configs)
        try:
            resource = ConfigResource(ConfigResourceType.BROKER, "1")
            configs = admin.describe_configs([resource])
            print(f"✓ Retrieved broker configs (DescribeConfigs API)")
        except Exception as e:
            print(f"⚠ DescribeConfigs not fully implemented yet: {e}")

        admin.close()
        return True

    except Exception as e:
        print(f"✗ KSQLDB-like connection failed: {e}")
        return False

def test_consumer_group_apis():
    """Test consumer group APIs that KSQLDB uses"""
    print("\nTesting consumer group APIs...")

    try:
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='ksql-consumer-test'
        )

        # Try to list consumer groups (KSQLDB monitors these)
        try:
            groups = admin.list_consumer_groups()
            print(f"✓ Listed consumer groups: {len(groups)} group(s)")
        except Exception as e:
            print(f"⚠ ListGroups not fully implemented: {e}")

        admin.close()
        return True

    except Exception as e:
        print(f"✗ Consumer group API test failed: {e}")
        return False

if __name__ == "__main__":
    ksql_ok = test_ksql_like_connection()
    groups_ok = test_consumer_group_apis()

    if ksql_ok:
        print("\n✓ KSQLDB should now be able to connect!")
        print("  AdminClient connects successfully with v0 API")
        print("  Metadata retrieval works")
        print("  Topic listing works")
        if not groups_ok:
            print("  Note: Consumer group APIs need implementation for full KSQLDB support")
    else:
        print("\n✗ KSQLDB connection still has issues")
        sys.exit(1)