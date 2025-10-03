#!/usr/bin/env python3
"""
Simple test for CreatePartitions API using raw protocol
"""

import socket
import struct
import time

def test_create_partitions():
    """Test CreatePartitions API with raw protocol"""

    print("Testing CreatePartitions API...")

    # First, create a topic with 1 partition using raw CreateTopics
    topic_name = f'expand-test-{int(time.time())}'

    # Connect to server
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    # CreateTopics request (API Key 19, Version 0)
    api_key = 19
    api_version = 0
    correlation_id = 1
    client_id = b'test-client'

    # Build request header
    header = struct.pack('>hhi', api_key, api_version, correlation_id)
    header += struct.pack('>h', len(client_id)) + client_id

    # Build request body
    topics_count = 1
    body = struct.pack('>i', topics_count)

    # Topic name
    topic_bytes = topic_name.encode('utf-8')
    body += struct.pack('>h', len(topic_bytes)) + topic_bytes

    # Partitions
    body += struct.pack('>i', 1)  # 1 partition initially

    # Replication factor
    body += struct.pack('>h', 1)

    # Replica assignments (null)
    body += struct.pack('>i', -1)

    # Config entries (null)
    body += struct.pack('>i', -1)

    # Timeout
    body += struct.pack('>i', 5000)

    # Send CreateTopics request
    request = header + body
    message = struct.pack('>i', len(request)) + request
    sock.sendall(message)

    # Read CreateTopics response
    size_bytes = sock.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    response = sock.recv(size)

    # Parse response to check if topic was created
    correlation_resp = struct.unpack('>i', response[:4])[0]
    print(f"CreateTopics correlation ID: {correlation_resp}")

    # Skip to error code (simplified parsing)
    # Response: correlation_id + topics_count + topic_name_len + topic_name + error_code
    offset = 4  # Skip correlation_id
    topics_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4

    if topics_count > 0:
        name_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2 + name_len  # Skip name
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        if error_code == 0:
            print(f"✅ Created topic '{topic_name}' with 1 partition")
        else:
            print(f"❌ Failed to create topic, error code: {error_code}")
            sock.close()
            return False

    sock.close()
    time.sleep(1)

    # Now test CreatePartitions to expand from 1 to 3 partitions
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    # CreatePartitions request (API Key 37, Version 0)
    api_key = 37
    api_version = 0
    correlation_id = 2

    # Build request header
    header = struct.pack('>hhi', api_key, api_version, correlation_id)
    header += struct.pack('>h', len(client_id)) + client_id

    # Build request body
    # topics array (1 topic)
    body = struct.pack('>i', 1)

    # Topic name
    body += struct.pack('>h', len(topic_bytes)) + topic_bytes

    # New partition count
    body += struct.pack('>i', 3)  # Expand to 3 partitions

    # Assignments (null)
    body += struct.pack('>i', -1)

    # Timeout
    body += struct.pack('>i', 5000)

    # Send CreatePartitions request
    request = header + body
    message = struct.pack('>i', len(request)) + request
    print(f"Sending CreatePartitions request to expand '{topic_name}' to 3 partitions...")
    sock.sendall(message)

    # Read CreatePartitions response
    size_bytes = sock.recv(4)
    if len(size_bytes) == 4:
        size = struct.unpack('>i', size_bytes)[0]
        print(f"Response size: {size} bytes")

        response = sock.recv(size)

        # Parse response
        correlation_resp = struct.unpack('>i', response[:4])[0]
        print(f"Correlation ID: {correlation_resp}")

        # Parse body
        offset = 4
        # Throttle time (v0 has it)
        throttle_time = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"Throttle time: {throttle_time}ms")

        # Results array
        results_count = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"Results count: {results_count}")

        if results_count > 0:
            # Topic name
            name_len = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            topic_name_resp = response[offset:offset+name_len].decode('utf-8')
            offset += name_len
            print(f"Topic: {topic_name_resp}")

            # Error code
            error_code = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2

            if error_code == 0:
                print(f"✅ CreatePartitions succeeded! Topic expanded to 3 partitions")
                sock.close()
                return True
            else:
                print(f"❌ CreatePartitions failed with error code: {error_code}")
                # Try to read error message if v1+
                if api_version >= 1 and offset < len(response):
                    msg_len = struct.unpack('>h', response[offset:offset+2])[0]
                    if msg_len > 0:
                        offset += 2
                        error_msg = response[offset:offset+msg_len].decode('utf-8')
                        print(f"   Error message: {error_msg}")
    else:
        print("Failed to read response")

    sock.close()
    return False

if __name__ == "__main__":
    result = test_create_partitions()

    print("\n" + "=" * 50)
    if result:
        print("🎉 CreatePartitions API is WORKING!")
    else:
        print("❌ CreatePartitions API test failed")