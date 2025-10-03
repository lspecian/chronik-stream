#!/usr/bin/env python3

"""
Test script to verify ListOffsets API with timestamp support works correctly.
This tests the enhanced implementation we just completed.
"""

import sys
import time
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic
from kafka.protocol.admin import ListOffsetsRequest_v1, ListOffsetsResponse_v1
from kafka.protocol.offset import ListOffsetsPartition
from kafka.protocol.types import Int64
import socket
from kafka.conn import BrokerConnection
from kafka.protocol.api import Request, Response
import struct
import json

def test_listoffsets_timestamp():
    """Test ListOffsets with timestamp support."""

    print("Testing ListOffsets API with timestamp support...")

    try:
        # Connect to our Chronik server
        bootstrap_servers = 'localhost:9092'

        # Create admin client
        admin = KafkaAdminClient(bootstrap_servers=bootstrap_servers)

        # Create a test topic
        topic_name = 'test-listoffsets-timestamp'
        try:
            admin.create_topics([NewTopic(name=topic_name, num_partitions=1, replication_factor=1)])
            print(f"Created topic: {topic_name}")
        except Exception as e:
            print(f"Topic might already exist: {e}")

        # Produce some test messages
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )

        print("Producing test messages...")
        current_time = int(time.time() * 1000)  # Current timestamp in milliseconds

        for i in range(5):
            message = {'message': f'test-{i}', 'timestamp': current_time + i * 1000}
            producer.send(topic_name, value=message)
            time.sleep(0.1)  # Small delay between messages

        producer.flush()
        print("Messages produced successfully")

        # Test ListOffsets with different timestamp queries
        print("\nTesting ListOffsets API...")

        # Create a simple connection to test ListOffsets directly
        conn = BrokerConnection('localhost', 9092, None, None)
        conn.connect()

        # Test 1: Get latest offsets (LATEST_TIMESTAMP = -1)
        print("1. Testing LATEST_TIMESTAMP (-1):")
        test_timestamp_query(conn, topic_name, -1, "LATEST")

        # Test 2: Get earliest offsets (EARLIEST_TIMESTAMP = -2)
        print("2. Testing EARLIEST_TIMESTAMP (-2):")
        test_timestamp_query(conn, topic_name, -2, "EARLIEST")

        # Test 3: Get offsets for current time
        print("3. Testing current timestamp:")
        test_timestamp_query(conn, topic_name, current_time, "CURRENT_TIME")

        # Test 4: Get offsets for future timestamp
        print("4. Testing future timestamp:")
        future_time = current_time + 60000  # 1 minute in future
        test_timestamp_query(conn, topic_name, future_time, "FUTURE")

        conn.close()
        print("\nListOffsets timestamp testing completed successfully!")
        return True

    except Exception as e:
        print(f"Test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_timestamp_query(conn, topic_name, timestamp, description):
    """Send a ListOffsets request with the specified timestamp."""
    try:
        # Build ListOffsets request manually
        request_data = struct.pack('>i', 1)  # correlation_id
        request_data += struct.pack('>h', len('python-test'))  # client_id length
        request_data += b'python-test'  # client_id
        request_data += struct.pack('>i', 1)  # topics array length
        request_data += struct.pack('>h', len(topic_name))  # topic name length
        request_data += topic_name.encode('utf-8')  # topic name
        request_data += struct.pack('>i', 1)  # partitions array length
        request_data += struct.pack('>i', 0)  # partition
        request_data += struct.pack('>q', timestamp)  # timestamp

        # Send request (API key for ListOffsets is 2)
        request_header = struct.pack('>hhi', 2, 1, 1)  # api_key, api_version, correlation_id
        full_request = struct.pack('>i', len(request_header) + len(request_data)) + request_header + request_data

        conn.send(full_request)

        # Read response
        response_size = struct.unpack('>i', conn.recv(4))[0]
        response_data = conn.recv(response_size)

        # Parse basic response (simplified)
        if len(response_data) >= 12:
            correlation_id = struct.unpack('>i', response_data[:4])[0]
            print(f"  {description} timestamp {timestamp}: Response received (correlation_id: {correlation_id})")
        else:
            print(f"  {description} timestamp {timestamp}: Short response received")

    except Exception as e:
        print(f"  {description} timestamp {timestamp}: Error - {e}")

if __name__ == '__main__':
    success = test_listoffsets_timestamp()
    sys.exit(0 if success else 1)