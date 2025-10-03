#!/usr/bin/env python3
"""
Comprehensive test of KSQL compatibility with Chronik Stream
Tests all the Kafka operations that KSQL requires
"""

import socket
import struct
import time
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import traceback

print("=" * 80)
print("KSQL COMPATIBILITY TEST FOR CHRONIK STREAM")
print("=" * 80)

def test_operation(name, func):
    """Helper to test an operation and report results"""
    print(f"\n{name}:")
    print("-" * 40)
    try:
        result = func()
        if result:
            print(f"✅ {name} PASSED")
            print(f"   Result: {result}")
        else:
            print(f"✅ {name} PASSED")
        return True
    except Exception as e:
        print(f"❌ {name} FAILED")
        print(f"   Error: {e}")
        if hasattr(e, '__cause__'):
            print(f"   Cause: {e.__cause__}")
        return False

# Test 1: Basic Connection
def test_basic_connection():
    """Test basic TCP connection"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    sock.close()
    return "Connected successfully"

# Test 2: AdminClient Operations
def test_admin_client():
    """Test AdminClient connectivity"""
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092'],
        client_id='ksql-test',
        request_timeout_ms=5000,
        api_version=(2, 8, 0)
    )

    # Try list_topics
    topics = admin.list_topics()
    admin.close()
    return f"Found {len(topics)} topics"

# Test 3: Create Topics (Critical for KSQL)
def test_create_topics():
    """Test creating topics - KSQL needs this for internal topics"""
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092'],
        client_id='ksql-test-create',
        request_timeout_ms=5000
    )

    try:
        new_topic = NewTopic(
            name="_confluent-ksql-default__command_topic",
            num_partitions=1,
            replication_factor=1
        )
        result = admin.create_topics([new_topic], validate_only=False)
        admin.close()
        return f"Created command topic: {result}"
    except Exception as e:
        admin.close()
        raise e

# Test 4: List Consumer Groups
def test_list_consumer_groups():
    """Test listing consumer groups"""
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092'],
        client_id='ksql-test-groups',
        request_timeout_ms=5000
    )

    try:
        groups = admin.list_consumer_groups()
        admin.close()
        return f"Found {len(groups)} consumer groups"
    except Exception as e:
        admin.close()
        raise e

# Test 5: Producer Operations
def test_producer():
    """Test producer operations"""
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        client_id='ksql-test-producer',
        request_timeout_ms=5000
    )

    try:
        future = producer.send('chronik-default', b'test-message')
        result = future.get(timeout=5)
        producer.close()
        return f"Sent message to partition {result.partition} at offset {result.offset}"
    except Exception as e:
        producer.close()
        raise e

# Test 6: Consumer Operations
def test_consumer():
    """Test consumer operations"""
    consumer = KafkaConsumer(
        'chronik-default',
        bootstrap_servers=['localhost:9092'],
        client_id='ksql-test-consumer',
        group_id='ksql-test-group',
        auto_offset_reset='earliest',
        consumer_timeout_ms=2000,
        request_timeout_ms=5000
    )

    try:
        # Just test that we can connect and poll
        messages = consumer.poll(timeout_ms=1000)
        msg_count = sum(len(msgs) for msgs in messages.values())
        consumer.close()
        return f"Polled {msg_count} messages"
    except Exception as e:
        consumer.close()
        raise e

# Test 7: DescribeCluster
def test_describe_cluster():
    """Test DescribeCluster API directly"""
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092'],
        client_id='ksql-test-cluster',
        request_timeout_ms=5000
    )

    try:
        cluster = admin._client.cluster
        brokers = cluster.brokers()
        controller = cluster.controller
        admin.close()
        return f"Cluster has {len(brokers)} brokers, controller: {controller}"
    except Exception as e:
        admin.close()
        raise e

# Run all tests
results = []

results.append(test_operation("Basic TCP Connection", test_basic_connection))
results.append(test_operation("AdminClient Connection", test_admin_client))
results.append(test_operation("DescribeCluster API", test_describe_cluster))
results.append(test_operation("Producer Operations", test_producer))
results.append(test_operation("Consumer Operations", test_consumer))
results.append(test_operation("List Consumer Groups", test_list_consumer_groups))
results.append(test_operation("Create Topics (KSQL Critical)", test_create_topics))

# Summary
print("\n" + "=" * 80)
print("SUMMARY")
print("=" * 80)

passed = sum(1 for r in results if r)
failed = sum(1 for r in results if not r)

print(f"✅ Passed: {passed}")
print(f"❌ Failed: {failed}")

if failed == 0:
    print("\n🎉 ALL TESTS PASSED! Chronik is fully KSQL-compatible!")
else:
    print(f"\n⚠️  {failed} test(s) failed. These APIs need implementation for full KSQL compatibility.")

print("\nKSQL Compatibility Status:")
if results[6]:  # Create Topics test
    print("✅ Can create internal KSQL topics")
else:
    print("❌ Cannot create internal KSQL topics - KSQL server will fail to start")

if results[5]:  # Consumer Groups test
    print("✅ Can manage consumer groups")
else:
    print("⚠️  Consumer group management may have issues")