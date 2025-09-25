#!/usr/bin/env python3
"""Test AdminClient functionality."""

import logging
from kafka.admin import KafkaAdminClient, NewTopic
from kafka.errors import KafkaError

# Enable debug logging
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)

def test_admin_client():
    """Test AdminClient operations."""

    print("="*60)
    print("Testing AdminClient")
    print("="*60)

    try:
        print("\n📋 Creating AdminClient...")
        admin_client = KafkaAdminClient(
            bootstrap_servers=['localhost:9094'],
            client_id='test-admin',
            request_timeout_ms=10000,
            metadata_max_age_ms=10000
        )

        print("✓ AdminClient created successfully")

        # List topics
        print("\n📂 Listing topics...")
        topics = admin_client.list_topics()
        print(f"✓ Found {len(topics)} topics: {topics}")

        # Try to create a topic
        print("\n🆕 Creating new topic 'admin-test-topic'...")
        new_topic = NewTopic(
            name='admin-test-topic',
            num_partitions=3,
            replication_factor=1
        )

        try:
            result = admin_client.create_topics([new_topic], validate_only=False)
            print(f"✓ Topic creation result: {result}")
        except Exception as e:
            print(f"❌ Topic creation failed: {e}")

        # List topics again
        print("\n📂 Listing topics after creation...")
        topics = admin_client.list_topics()
        print(f"✓ Found {len(topics)} topics: {topics}")

        # Get topic metadata
        print("\n📊 Getting topic metadata...")
        metadata = admin_client._client.cluster
        print(f"  Brokers: {metadata.brokers()}")
        print(f"  Topics: {metadata.topics()}")
        print(f"  Controller: {metadata.controller}")

        # Describe configs (if supported)
        print("\n⚙️ Attempting to describe configs...")
        try:
            # This will test DescribeConfigs API
            from kafka.admin import ConfigResource, ConfigResourceType
            resource = ConfigResource(ConfigResourceType.BROKER, "1")
            configs = admin_client.describe_configs(config_resources=[resource])
            print(f"✓ Config description: {configs}")
        except Exception as e:
            print(f"⚠️ Describe configs not supported: {e}")

        admin_client.close()
        print("\n✅ AdminClient test completed successfully!")

    except Exception as e:
        print(f"\n❌ AdminClient test failed:")
        print(f"  Error type: {type(e).__name__}")
        print(f"  Error: {e}")

        import traceback
        print("\n📋 Full traceback:")
        traceback.print_exc()

if __name__ == "__main__":
    test_admin_client()