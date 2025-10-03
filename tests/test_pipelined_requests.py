#!/usr/bin/env python3
"""Test pipelined requests to reproduce KSQLDB connection issue."""

import socket
import struct
import time

def create_api_versions_v3_request():
    """Create an ApiVersions v3 request as sent by KSQLDB/Confluent clients."""
    # Header: api_key=18, version=3, correlation_id=0, client_id="adminclient-2"
    header = b''
    header += struct.pack('>h', 18)  # api_key = ApiVersions
    header += struct.pack('>h', 3)   # api_version = 3
    header += struct.pack('>i', 0)   # correlation_id = 0

    # client_id as normal string (non-flexible header)
    client_id = b'adminclient-2'
    header += struct.pack('>h', len(client_id))
    header += client_id

    # Body for ApiVersions v3 (hybrid: uses compact strings despite non-flexible header)
    body = b''
    # client_software_name as compact string
    software_name = b'apache-kafka-java'
    body += bytes([len(software_name) + 1])  # Compact string length
    body += software_name

    # client_software_version as compact string
    software_version = b'7.5.0-140-ccs'
    body += bytes([len(software_version) + 1])  # Compact string length
    body += software_version

    # Tagged fields (empty)
    body += bytes([0])

    # Combine header and body
    message = header + body

    # Add length prefix
    return struct.pack('>i', len(message)) + message

def create_metadata_v12_request():
    """Create a Metadata v12 request as sent by KSQLDB."""
    # Header: api_key=3, version=12, correlation_id=1, client_id="adminclient-2"
    header = b''
    header += struct.pack('>h', 3)   # api_key = Metadata
    header += struct.pack('>h', 12)  # api_version = 12
    header += struct.pack('>i', 1)   # correlation_id = 1

    # client_id as normal string (non-flexible header)
    client_id = b'adminclient-2'
    header += struct.pack('>h', len(client_id))
    header += client_id

    # Body for Metadata v12 (flexible, but uses hybrid encoding)
    body = b''

    # topics array - null means all topics (compact array encoding)
    body += bytes([0])  # Compact array with 0 elements (null)

    # allow_auto_topic_creation (boolean)
    body += bytes([1])  # true

    # include_topic_authorized_operations (boolean) - v12+
    body += bytes([1])  # true

    # Tagged fields (empty)
    body += bytes([0])

    # Combine header and body
    message = header + body

    # Add length prefix
    return struct.pack('>i', len(message)) + message

def test_pipelined_requests():
    """Test sending pipelined requests like KSQLDB does."""
    print("Testing pipelined requests (KSQLDB pattern)...")
    print("-" * 60)

    # Create both requests
    api_versions_req = create_api_versions_v3_request()
    metadata_req = create_metadata_v12_request()

    print(f"ApiVersions request: {len(api_versions_req)} bytes")
    print(f"  Hex: {api_versions_req[:32].hex()}...")
    print(f"Metadata request: {len(metadata_req)} bytes")
    print(f"  Hex: {metadata_req[:32].hex()}...")

    # Connect to Chronik
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    print("\nConnected to Chronik on port 9092")

    # Send both requests pipelined (in one TCP packet)
    pipelined = api_versions_req + metadata_req
    print(f"\nSending {len(pipelined)} bytes (both requests pipelined)")
    sock.send(pipelined)

    # Read ApiVersions response
    print("\nReading ApiVersions response...")
    response_len_bytes = sock.recv(4)
    if len(response_len_bytes) == 4:
        response_len = struct.unpack('>i', response_len_bytes)[0]
        print(f"  Response size: {response_len} bytes")
        response = sock.recv(response_len)
        print(f"  Received {len(response)} bytes")

        # Parse correlation ID
        if len(response) >= 4:
            corr_id = struct.unpack('>i', response[:4])[0]
            print(f"  Correlation ID: {corr_id}")

    # Read Metadata response
    print("\nReading Metadata response...")
    response_len_bytes = sock.recv(4)
    if len(response_len_bytes) == 4:
        response_len = struct.unpack('>i', response_len_bytes)[0]
        print(f"  Response size: {response_len} bytes")
        response = sock.recv(response_len)
        print(f"  Received {len(response)} bytes")

        # Parse correlation ID
        if len(response) >= 4:
            corr_id = struct.unpack('>i', response[:4])[0]
            print(f"  Correlation ID: {corr_id}")
    else:
        print(f"  ERROR: Only received {len(response_len_bytes)} bytes for size header")

    sock.close()
    print("\nTest complete!")

def test_sequential_requests():
    """Test sending requests sequentially (not pipelined)."""
    print("\n\nTesting sequential requests (traditional pattern)...")
    print("-" * 60)

    # Connect to Chronik
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    print("Connected to Chronik on port 9092")

    # Send ApiVersions request
    api_versions_req = create_api_versions_v3_request()
    print(f"\nSending ApiVersions request: {len(api_versions_req)} bytes")
    sock.send(api_versions_req)

    # Read ApiVersions response
    response_len_bytes = sock.recv(4)
    response_len = struct.unpack('>i', response_len_bytes)[0]
    response = sock.recv(response_len)
    print(f"  Received response: {len(response)} bytes")

    # Send Metadata request
    metadata_req = create_metadata_v12_request()
    print(f"\nSending Metadata request: {len(metadata_req)} bytes")
    sock.send(metadata_req)

    # Read Metadata response
    response_len_bytes = sock.recv(4)
    response_len = struct.unpack('>i', response_len_bytes)[0]
    response = sock.recv(response_len)
    print(f"  Received response: {len(response)} bytes")

    sock.close()
    print("\nTest complete!")

if __name__ == '__main__':
    # Test both patterns
    test_pipelined_requests()
    test_sequential_requests()