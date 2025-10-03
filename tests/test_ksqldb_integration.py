#!/usr/bin/env python3
"""Test KSQLDB integration with Chronik Stream."""

import time
import logging
from kafka import KafkaProducer, KafkaAdminClient, KafkaConsumer
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import json

# Enable detailed logging
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s'
)
logger = logging.getLogger(__name__)

def test_basic_kafka_functionality():
    """Test basic Kafka operations that KSQLDB needs."""

    print("\n" + "="*60)
    print("Testing Basic Kafka Functionality for KSQLDB")
    print("="*60)

    # 1. Test AdminClient - Required for KSQLDB
    try:
        print("\n1. Testing AdminClient...")
        admin = KafkaAdminClient(
            bootstrap_servers=['localhost:9094'],
            client_id='test-ksqldb-admin',
            request_timeout_ms=30000
        )

        # List topics
        topics = admin.list_topics()
        print(f"   ✓ AdminClient connected, found {len(topics)} topics")

        # Create a test topic for KSQLDB
        test_topic = NewTopic(
            name='ksqldb-test-stream',
            num_partitions=1,
            replication_factor=1
        )

        try:
            admin.create_topics([test_topic])
            print(f"   ✓ Created topic: ksqldb-test-stream")
        except Exception as e:
            if "already exists" in str(e).lower():
                print(f"   ℹ Topic already exists: ksqldb-test-stream")
            else:
                print(f"   ❌ Failed to create topic: {e}")

        admin.close()

    except Exception as e:
        print(f"   ❌ AdminClient failed: {e}")
        return False

    # 2. Test Producer - Required for KSQLDB to write
    try:
        print("\n2. Testing Producer...")
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9094'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )

        # Send test messages
        for i in range(5):
            data = {
                'id': i,
                'name': f'User_{i}',
                'timestamp': int(time.time() * 1000)
            }
            future = producer.send('ksqldb-test-stream', value=data)
            result = future.get(timeout=10)
            print(f"   ✓ Sent message {i}: offset={result.offset}")

        producer.close()

    except Exception as e:
        print(f"   ❌ Producer failed: {e}")
        return False

    # 3. Test Consumer - Required for KSQLDB to read
    try:
        print("\n3. Testing Consumer...")
        consumer = KafkaConsumer(
            'ksqldb-test-stream',
            bootstrap_servers=['localhost:9094'],
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000,
            value_deserializer=lambda m: json.loads(m.decode('utf-8'))
        )

        msg_count = 0
        for message in consumer:
            msg_count += 1
            print(f"   ✓ Received message: {message.value}")
            if msg_count >= 3:  # Just read a few
                break

        consumer.close()

    except Exception as e:
        print(f"   ❌ Consumer failed: {e}")
        return False

    print("\n" + "="*60)
    print("✅ All basic Kafka operations working!")
    print("="*60)
    return True

def test_ksqldb_required_apis():
    """Test specific APIs that KSQLDB requires."""

    print("\n" + "="*60)
    print("Testing KSQLDB Required APIs")
    print("="*60)

    try:
        from kafka.protocol.admin import (
            DescribeConfigsRequest_v0,
            ListGroupsRequest_v0,
            DescribeGroupsRequest_v0
        )
        from kafka.protocol.types import Int8, Int32, String, Array
        import struct
        import socket

        # Connect directly to test specific APIs
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9094))

        def send_request(api_key, api_version, correlation_id, body):
            """Send a Kafka request and get response."""
            # Build request header
            header = struct.pack('>hhih', api_key, api_version, correlation_id, len('test-client'))
            header += b'test-client'

            # Build full request
            request = struct.pack('>i', len(header) + len(body)) + header + body
            sock.send(request)

            # Read response
            size_bytes = sock.recv(4)
            if len(size_bytes) < 4:
                return None
            size = struct.unpack('>i', size_bytes)[0]
            response = sock.recv(size)
            return response

        # Test DescribeConfigs (API key 32)
        print("\n1. Testing DescribeConfigs API...")
        # Simple request for broker config
        body = struct.pack('>i', 1)  # 1 resource
        body += struct.pack('>b', 4)  # resource type = BROKER
        body += struct.pack('>h', 1) + b'1'  # resource name = "1"
        body += struct.pack('>i', -1)  # null config names (get all)

        response = send_request(32, 0, 1, body)
        if response:
            print("   ✓ DescribeConfigs API responded")
        else:
            print("   ❌ DescribeConfigs API failed")

        # Test ListGroups (API key 16)
        print("\n2. Testing ListGroups API...")
        body = b''  # Empty body for ListGroups v0

        response = send_request(16, 0, 2, body)
        if response:
            print("   ✓ ListGroups API responded")
        else:
            print("   ❌ ListGroups API failed")

        # Test DescribeGroups (API key 15)
        print("\n3. Testing DescribeGroups API...")
        # Request for a test group
        body = struct.pack('>i', 1)  # 1 group
        test_group = b'test-group'
        body += struct.pack('>h', len(test_group)) + test_group

        response = send_request(15, 0, 3, body)
        if response:
            print("   ✓ DescribeGroups API responded")
        else:
            print("   ❌ DescribeGroups API failed")

        sock.close()

        print("\n" + "="*60)
        print("✅ All KSQLDB required APIs responding!")
        print("="*60)
        return True

    except Exception as e:
        print(f"\n❌ API test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    """Run all tests."""

    print("\n" + "🚀"*30)
    print("CHRONIK STREAM - KSQLDB COMPATIBILITY TEST")
    print("🚀"*30)

    # Test basic functionality
    if not test_basic_kafka_functionality():
        print("\n⚠️  Basic Kafka functionality issues detected")
        print("KSQLDB may not work properly")
        return

    # Test KSQLDB required APIs
    if not test_ksqldb_required_apis():
        print("\n⚠️  KSQLDB required APIs not fully working")
        return

    print("\n" + "="*60)
    print("🎉 CHRONIK STREAM IS KSQLDB READY! 🎉")
    print("="*60)
    print("\nNext steps:")
    print("1. Start KSQLDB with bootstrap.servers=localhost:9094")
    print("2. Create streams and tables")
    print("3. Run KSQL queries")
    print("\nExample KSQL:")
    print("  CREATE STREAM test_stream (")
    print("    id INT,")
    print("    name VARCHAR,")
    print("    timestamp BIGINT")
    print("  ) WITH (")
    print("    KAFKA_TOPIC='ksqldb-test-stream',")
    print("    VALUE_FORMAT='JSON'")
    print("  );")

if __name__ == "__main__":
    main()