#!/usr/bin/env python3
"""Test DeleteRecords API"""

from kafka.admin import KafkaAdminClient
import sys
import time

def test_delete_records():
    """Test DeleteRecords operation"""
    try:
        # Create admin client
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='test-delete-records-client'
        )

        print("Connected to Kafka admin client")

        # Test 1: Delete records from test-topic partition 0 before offset 100
        print("\n--- Test 1: Delete records before offset 100 ---")

        # kafka-python doesn't have a direct delete_records method,
        # but we can test with raw protocol if needed
        # For now, we'll test the API directly using a raw socket

        import socket
        import struct

        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))

        # Encode DeleteRecords request (API key 21, version 0)
        # Header
        api_key = 21  # DeleteRecords
        api_version = 0
        correlation_id = 1
        client_id = b'test-delete-client'

        header = struct.pack('>h', api_key)
        header += struct.pack('>h', api_version)
        header += struct.pack('>i', correlation_id)
        header += struct.pack('>h', len(client_id))
        header += client_id

        # Body - DeleteRecords request
        body = b''

        # Topics array (1 topic)
        body += struct.pack('>i', 1)

        # Topic name
        topic_name = b'test-topic'
        body += struct.pack('>h', len(topic_name))
        body += topic_name

        # Partitions array (1 partition)
        body += struct.pack('>i', 1)

        # Partition 0, offset 100
        body += struct.pack('>i', 0)  # partition_index
        body += struct.pack('>q', 100)  # offset

        # Timeout
        body += struct.pack('>i', 5000)  # timeout_ms

        # Send request
        message = header + body
        size_bytes = struct.pack('>i', len(message))
        sock.sendall(size_bytes + message)

        print("Sent DeleteRecords request for topic 'test-topic' partition 0 before offset 100")

        # Read response
        size_data = sock.recv(4)
        if len(size_data) < 4:
            print("Failed to read response size")
            return False

        response_size = struct.unpack('>i', size_data)[0]
        print(f"Response size: {response_size} bytes")

        response_data = sock.recv(response_size)

        # Parse response
        offset = 0

        # Correlation ID
        correlation_id = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Response correlation ID: {correlation_id}")

        # Throttle time
        throttle_time = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Throttle time: {throttle_time}ms")

        # Topics array
        topic_count = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Topics in response: {topic_count}")

        for i in range(topic_count):
            # Topic name
            name_len = struct.unpack('>h', response_data[offset:offset+2])[0]
            offset += 2
            topic_name = response_data[offset:offset+name_len].decode('utf-8')
            offset += name_len
            print(f"\nTopic: {topic_name}")

            # Partitions array
            partition_count = struct.unpack('>i', response_data[offset:offset+4])[0]
            offset += 4

            for j in range(partition_count):
                partition_index = struct.unpack('>i', response_data[offset:offset+4])[0]
                offset += 4
                low_watermark = struct.unpack('>q', response_data[offset:offset+8])[0]
                offset += 8
                error_code = struct.unpack('>h', response_data[offset:offset+2])[0]
                offset += 2

                print(f"  Partition {partition_index}:")
                print(f"    Low watermark: {low_watermark}")
                print(f"    Error code: {error_code}")

                if error_code == 0:
                    print(f"    ✅ Successfully set low watermark to {low_watermark}")
                else:
                    print(f"    ❌ Error code: {error_code}")

        sock.close()

        # Test 2: Test with v2 (with tagged fields)
        print("\n--- Test 2: Delete records with v2 protocol ---")

        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))

        # Header for v2
        api_version = 2
        correlation_id = 2

        header = struct.pack('>h', api_key)
        header += struct.pack('>h', api_version)
        header += struct.pack('>i', correlation_id)
        header += struct.pack('>h', len(client_id))
        header += client_id

        # Body - DeleteRecords request v2
        body = b''

        # Topics array
        body += struct.pack('>i', 1)

        # Topic name
        topic_name = b'test-topic-v2'
        body += struct.pack('>h', len(topic_name))
        body += topic_name

        # Partitions array
        body += struct.pack('>i', 2)  # 2 partitions

        # Partition 0, offset 50
        body += struct.pack('>i', 0)
        body += struct.pack('>q', 50)
        body += b'\x00'  # Tagged fields (none)

        # Partition 1, offset 75
        body += struct.pack('>i', 1)
        body += struct.pack('>q', 75)
        body += b'\x00'  # Tagged fields (none)

        # Topic tagged fields
        body += b'\x00'  # Tagged fields (none)

        # Timeout
        body += struct.pack('>i', 5000)

        # Request tagged fields
        body += b'\x00'  # Tagged fields (none)

        # Send request
        message = header + body
        size_bytes = struct.pack('>i', len(message))
        sock.sendall(size_bytes + message)

        print("Sent DeleteRecords v2 request for topic 'test-topic-v2'")

        # Read response
        size_data = sock.recv(4)
        response_size = struct.unpack('>i', size_data)[0]
        response_data = sock.recv(response_size)

        # Parse v2 response (similar structure but with nullable error messages)
        offset = 0
        correlation_id = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Response correlation ID: {correlation_id}")

        sock.close()

        print("\n✅ All DeleteRecords tests completed!")
        return True

    except Exception as e:
        print(f"❌ DeleteRecords test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    success = test_delete_records()
    sys.exit(0 if success else 1)