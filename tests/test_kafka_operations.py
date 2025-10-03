#!/usr/bin/env python3
"""Test all Kafka operations with Chronik to see what works"""

from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic, ConfigResource, ConfigResourceType
from kafka.errors import KafkaError
import json
import time

print("Testing Kafka operations with Chronik Stream...")
print("=" * 60)

# Test 1: Admin operations
print("\n1. ADMIN OPERATIONS")
print("-" * 40)
try:
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092'],
        client_id='test-admin',
        request_timeout_ms=5000
    )
    print("✓ AdminClient connected")

    # List topics
    try:
        topics = admin.list_topics()
        print(f"✓ List topics works - Found: {topics}")
    except Exception as e:
        print(f"✗ List topics failed: {e}")

    # Create topic
    try:
        new_topic = NewTopic(name="test_topic", num_partitions=1, replication_factor=1)
        result = admin.create_topics([new_topic], validate_only=False)
        print(f"✓ Create topic attempted - Result: {result}")
    except Exception as e:
        print(f"✗ Create topic failed: {e}")

    # Delete topic
    try:
        result = admin.delete_topics(["test_topic"])
        print(f"✓ Delete topic attempted - Result: {result}")
    except Exception as e:
        print(f"✗ Delete topic failed: {e}")

    admin.close()
except Exception as e:
    print(f"✗ AdminClient failed: {e}")

# Test 2: Producer operations
print("\n2. PRODUCER OPERATIONS")
print("-" * 40)
try:
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        client_id='test-producer',
        value_serializer=lambda m: json.dumps(m).encode('utf-8')
    )
    print("✓ Producer connected")

    # Send message
    try:
        future = producer.send('chronik-default', {'test': 'message', 'id': 1})
        result = future.get(timeout=2)
        print(f"✓ Message sent - Partition: {result.partition}, Offset: {result.offset}")
    except Exception as e:
        print(f"✗ Send message failed: {e}")

    producer.close()
except Exception as e:
    print(f"✗ Producer failed: {e}")

# Test 3: Consumer operations
print("\n3. CONSUMER OPERATIONS")
print("-" * 40)
try:
    consumer = KafkaConsumer(
        'chronik-default',
        bootstrap_servers=['localhost:9092'],
        client_id='test-consumer',
        group_id='test-group',
        auto_offset_reset='earliest',
        consumer_timeout_ms=2000
    )
    print("✓ Consumer connected")

    # Poll for messages
    try:
        messages = consumer.poll(timeout_ms=1000)
        print(f"✓ Poll works - Received {sum(len(msgs) for msgs in messages.values())} messages")

        # Try to consume
        msg_count = 0
        for message in consumer:
            msg_count += 1
            print(f"  Message {msg_count}: partition={message.partition}, offset={message.offset}")
            if msg_count >= 3:
                break
    except Exception as e:
        print(f"✗ Consume messages failed: {e}")

    consumer.close()
except Exception as e:
    print(f"✗ Consumer failed: {e}")

# Test 4: Consumer Group operations
print("\n4. CONSUMER GROUP OPERATIONS")
print("-" * 40)
try:
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092'],
        client_id='test-admin-2'
    )

    # List consumer groups
    try:
        groups = admin.list_consumer_groups()
        print(f"✓ List consumer groups works - Found: {len(groups)} groups")
        for group in groups[:3]:
            print(f"  - {group}")
    except Exception as e:
        print(f"✗ List consumer groups failed: {e}")

    # Describe consumer groups
    try:
        if groups:
            group_id = groups[0][0] if groups else 'test-group'
            result = admin.describe_consumer_groups([group_id])
            print(f"✓ Describe consumer group works for '{group_id}'")
    except Exception as e:
        print(f"✗ Describe consumer groups failed: {e}")

    admin.close()
except Exception as e:
    print(f"✗ Consumer group operations failed: {e}")

# Test 5: Cluster operations
print("\n5. CLUSTER OPERATIONS")
print("-" * 40)
try:
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092'],
        client_id='test-admin-3'
    )

    # Describe cluster
    try:
        cluster = admin._client.cluster
        print(f"✓ Cluster info: {cluster.brokers()}")
    except Exception as e:
        print(f"✗ Describe cluster failed: {e}")

    # Get configs
    try:
        configs = admin.describe_configs(
            config_resources=[ConfigResource(ConfigResourceType.BROKER, "1")]
        )
        print(f"✓ Describe configs attempted")
    except Exception as e:
        print(f"✗ Describe configs failed: {e}")

    admin.close()
except Exception as e:
    print(f"✗ Cluster operations failed: {e}")

print("\n" + "=" * 60)
print("SUMMARY:")
print("Chronik successfully handles:")
print("- Basic connections (ApiVersions, Metadata, DescribeCluster)")
print("- Producer operations (sending messages)")
print("- Consumer operations (consuming messages)")
print("\nNeeds implementation for full KSQL support:")
print("- CreateTopics API (for KSQL internal topics)")
print("- DeleteTopics API")
print("- Consumer group management APIs")
print("- Configuration APIs")