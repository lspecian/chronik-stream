#!/usr/bin/env python3
"""Test raw MetadataRequest v0 to Chronik Stream"""

import socket
import struct
import sys

def encode_request_header(api_key, api_version, correlation_id, client_id):
    """Encode a Kafka request header"""
    header = struct.pack('>h', api_key)  # API key
    header += struct.pack('>h', api_version)  # API version
    header += struct.pack('>i', correlation_id)  # Correlation ID

    # Client ID (nullable string)
    if client_id:
        client_id_bytes = client_id.encode('utf-8')
        header += struct.pack('>h', len(client_id_bytes))
        header += client_id_bytes
    else:
        header += struct.pack('>h', -1)  # null string

    return header

def send_metadata_request_v0():
    """Send MetadataRequest v0 and parse response"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))

    # Build MetadataRequest v0
    # v0 structure: topic_array (null = all topics)
    header = encode_request_header(3, 0, 2, 'test-client')  # API 3 = Metadata, version 0

    # Request body for v0: just topic array
    # -1 = null array (all topics)
    body = struct.pack('>i', -1)

    request = header + body

    # Send with length prefix
    message = struct.pack('>i', len(request)) + request
    print(f"Sending MetadataRequest v0 ({len(message)} bytes)")
    print(f"Request hex: {message.hex()}")
    sock.send(message)

    # Read response
    response_len_bytes = sock.recv(4)
    if len(response_len_bytes) < 4:
        print(f"✗ Failed to read response length, got {len(response_len_bytes)} bytes")
        return False

    response_len = struct.unpack('>i', response_len_bytes)[0]
    print(f"Response length: {response_len}")

    response = sock.recv(response_len)
    print(f"Response bytes (first 100): {response[:100].hex()}")

    # Parse response
    offset = 0

    # Correlation ID
    correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id}")

    # For MetadataResponse v0:
    # - brokers array
    # - topics array

    # Brokers array
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Broker count: {broker_count}")

    for i in range(broker_count):
        node_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4

        # Host string
        host_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        host = response[offset:offset+host_len].decode('utf-8')
        offset += host_len

        # Port
        port = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4

        print(f"  Broker {node_id}: {host}:{port}")

    # Topics array
    topic_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Topic count: {topic_count}")

    for i in range(min(5, topic_count)):  # Print first 5 topics
        # Error code
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2

        # Topic name
        name_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        topic_name = response[offset:offset+name_len].decode('utf-8')
        offset += name_len

        # Partitions array
        partition_count = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4

        print(f"  Topic '{topic_name}': {partition_count} partitions, error={error_code}")

        # Skip partition details for brevity
        for p in range(partition_count):
            # Error code (2), partition_index (4), leader_id (4)
            offset += 10

            # Replica array
            replica_count = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            offset += replica_count * 4  # Skip replica IDs

            # ISR array
            isr_count = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            offset += isr_count * 4  # Skip ISR IDs

    sock.close()

    print("\n✓ MetadataRequest v0 succeeded!")
    return True

if __name__ == "__main__":
    try:
        success = send_metadata_request_v0()
        sys.exit(0 if success else 1)
    except Exception as e:
        print(f"✗ Error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)