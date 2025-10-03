#!/usr/bin/env python3
"""Test DescribeLogDirs API"""

import socket
import struct
import sys

def test_describe_log_dirs():
    """Test DescribeLogDirs operation"""
    try:
        # Create socket connection
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))

        print("Connected to Kafka server")

        # Test 1: Request log dirs for all topics (v0)
        print("\n--- Test 1: DescribeLogDirs v0 for all topics ---")

        # Encode DescribeLogDirs request (API key 35, version 0)
        api_key = 35  # DescribeLogDirs
        api_version = 0
        correlation_id = 1
        client_id = b'test-describe-log-dirs'

        # Build header
        header = struct.pack('>h', api_key)
        header += struct.pack('>h', api_version)
        header += struct.pack('>i', correlation_id)
        header += struct.pack('>h', len(client_id))
        header += client_id

        # Build body - null topics means all topics
        body = struct.pack('>i', -1)  # null array

        # Send request
        message = header + body
        size_bytes = struct.pack('>i', len(message))
        sock.sendall(size_bytes + message)

        print("Sent DescribeLogDirs request for all topics")

        # Read response
        size_data = sock.recv(4)
        if len(size_data) < 4:
            print("Failed to read response size")
            return False

        response_size = struct.unpack('>i', size_data)[0]
        print(f"Response size: {response_size} bytes")

        response_data = sock.recv(response_size)
        offset = 0

        # Parse response header
        correlation_id = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Correlation ID: {correlation_id}")

        # Parse response body
        # Throttle time
        throttle_time = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Throttle time: {throttle_time}ms")

        # Results array
        result_count = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Number of log directories: {result_count}")

        for i in range(result_count):
            # Error code
            error_code = struct.unpack('>h', response_data[offset:offset+2])[0]
            offset += 2

            # Log dir path
            path_len = struct.unpack('>h', response_data[offset:offset+2])[0]
            offset += 2
            log_dir = response_data[offset:offset+path_len].decode('utf-8')
            offset += path_len

            print(f"\nLog Directory: {log_dir}")
            print(f"  Error code: {error_code}")

            # Topics array
            topic_count = struct.unpack('>i', response_data[offset:offset+4])[0]
            offset += 4
            print(f"  Topics: {topic_count}")

            for j in range(topic_count):
                # Topic name
                name_len = struct.unpack('>h', response_data[offset:offset+2])[0]
                offset += 2
                topic_name = response_data[offset:offset+name_len].decode('utf-8')
                offset += name_len

                print(f"    Topic: {topic_name}")

                # Partitions array
                partition_count = struct.unpack('>i', response_data[offset:offset+4])[0]
                offset += 4

                for k in range(partition_count):
                    partition_id = struct.unpack('>i', response_data[offset:offset+4])[0]
                    offset += 4
                    size = struct.unpack('>q', response_data[offset:offset+8])[0]
                    offset += 8
                    offset_lag = struct.unpack('>q', response_data[offset:offset+8])[0]
                    offset += 8

                    print(f"      Partition {partition_id}: size={size} bytes, offset_lag={offset_lag}")

        sock.close()

        # Test 2: Request specific topics (v0)
        print("\n--- Test 2: DescribeLogDirs v0 for specific topics ---")

        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))

        correlation_id = 2

        # Build header
        header = struct.pack('>h', api_key)
        header += struct.pack('>h', api_version)
        header += struct.pack('>i', correlation_id)
        header += struct.pack('>h', len(client_id))
        header += client_id

        # Build body - specific topics
        body = b''

        # Topics array (1 topic)
        body += struct.pack('>i', 1)

        # Topic 1
        topic_name = b'test-topic'
        body += struct.pack('>h', len(topic_name))
        body += topic_name

        # Partitions array (2 partitions)
        body += struct.pack('>i', 2)
        body += struct.pack('>i', 0)  # partition 0
        body += struct.pack('>i', 1)  # partition 1

        # Send request
        message = header + body
        size_bytes = struct.pack('>i', len(message))
        sock.sendall(size_bytes + message)

        print("Sent DescribeLogDirs request for topic 'test-topic' partitions 0,1")

        # Read response
        size_data = sock.recv(4)
        response_size = struct.unpack('>i', size_data)[0]
        response_data = sock.recv(response_size)

        offset = 0
        correlation_id = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Correlation ID: {correlation_id}")

        sock.close()

        print("\n✅ All DescribeLogDirs tests completed!")
        return True

    except Exception as e:
        print(f"❌ DescribeLogDirs test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    success = test_describe_log_dirs()
    sys.exit(0 if success else 1)