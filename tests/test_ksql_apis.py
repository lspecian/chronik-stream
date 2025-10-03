#!/usr/bin/env python3
"""Test script to identify which Kafka APIs KSQL needs after handshake"""

from kafka import KafkaAdminClient, KafkaProducer, KafkaConsumer
from kafka.admin import NewTopic
import time

print("Testing KSQL required APIs...")

# Test 1: AdminClient (what KSQL uses first)
try:
    print("\n1. Testing AdminClient connection...")
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092'],
        client_id='ksql-test-admin',
        request_timeout_ms=5000
    )
    print("   ✓ AdminClient connected successfully")

    # Try to list topics
    print("   Attempting to list topics...")
    topics = admin.list_topics()
    print(f"   ✓ Topics: {topics}")

    # Try to create a topic (what KSQL does for internal topics)
    print("   Attempting to create topic...")
    new_topics = [NewTopic(name="_ksql_test_topic", num_partitions=1, replication_factor=1)]
    try:
        result = admin.create_topics(new_topics=new_topics, validate_only=False)
        print(f"   ✓ Topic creation result: {result}")
    except Exception as e:
        print(f"   ✗ Topic creation failed: {e}")

    admin.close()
except Exception as e:
    print(f"   ✗ AdminClient failed: {e}")

# Test 2: Consumer Group operations
try:
    print("\n2. Testing Consumer operations...")
    consumer = KafkaConsumer(
        'chronik-default',
        bootstrap_servers=['localhost:9092'],
        client_id='ksql-test-consumer',
        group_id='ksql-test-group',
        auto_offset_reset='earliest',
        consumer_timeout_ms=1000
    )
    print("   ✓ Consumer connected")

    # Try to poll
    print("   Attempting to poll...")
    messages = consumer.poll(timeout_ms=1000)
    print(f"   ✓ Poll completed: {len(messages)} partitions")

    consumer.close()
except Exception as e:
    print(f"   ✗ Consumer failed: {e}")

# Test 3: Find coordinator (what KSQL needs for consumer groups)
try:
    print("\n3. Testing FindCoordinator...")
    from kafka.protocol.admin import FindCoordinatorRequest
    from kafka.conn import BrokerConnection

    conn = BrokerConnection('localhost', 9092, socket_timeout_ms=5000)
    conn.connect()
    if conn.connected():
        print("   ✓ Connected for FindCoordinator")
        # Would need to send FindCoordinator request here
        conn.close()
    else:
        print("   ✗ Could not connect for FindCoordinator")
except Exception as e:
    print(f"   ✗ FindCoordinator test failed: {e}")

print("\n✓ Test completed")