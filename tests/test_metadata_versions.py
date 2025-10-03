#!/usr/bin/env python3
"""Test MetadataResponse for all versions to understand the field ordering."""

import socket
import struct
from kafka.protocol.metadata import MetadataResponse

def send_metadata_request(version, correlation_id=123):
    """Send a metadata request and return the raw response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    request = bytearray()
    request.extend(struct.pack('>h', 3))      # API Key (Metadata)
    request.extend(struct.pack('>h', version)) # API Version
    request.extend(struct.pack('>i', correlation_id))
    request.extend(struct.pack('>h', 4))      # Client ID length
    request.extend(b'test')                   # Client ID

    if version == 0:
        # v0: just topics array
        request.extend(struct.pack('>i', -1))  # Topics: -1 = null
    elif version >= 1 and version < 4:
        # v1-v3: topics array
        request.extend(struct.pack('>i', -1))  # Topics: -1 = null
    elif version >= 4:
        # v4+: topics array + allow_auto_topic_creation
        request.extend(struct.pack('>i', -1))  # Topics: -1 = null
        request.extend(struct.pack('>b', 1))   # allow_auto_topic_creation: true

    # Send with size prefix
    size = len(request)
    sock.send(struct.pack('>i', size))
    sock.send(request)

    # Read response
    size_bytes = sock.recv(4)
    response_size = struct.unpack('>i', size_bytes)[0]

    response = bytearray()
    while len(response) < response_size:
        chunk = sock.recv(min(4096, response_size - len(response)))
        if not chunk:
            break
        response.extend(chunk)

    sock.close()
    return response

def analyze_response(version, response):
    """Analyze the response structure."""
    print(f"\n=== Version {version} ===")
    print(f"Response size: {len(response)} bytes")
    print(f"First 100 bytes hex: {response[:100].hex()}")

    offset = 0

    # Correlation ID (always first)
    correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id}")

    # Field order varies by version
    if version == 0:
        # v0: brokers, topics
        print("Expected: brokers, topics")
    elif version == 1:
        # v1: brokers, controller_id, topics
        print("Expected: brokers, controller_id, topics")
    elif version == 2:
        # v2: brokers, cluster_id, controller_id, topics
        print("Expected: brokers, cluster_id, controller_id, topics")
    elif version >= 3 and version < 9:
        # v3-v8: throttle_time, brokers, cluster_id, controller_id, topics
        print("Expected: throttle_time, brokers, cluster_id, controller_id, topics")

        # Read throttle_time
        throttle_time = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"Throttle time: {throttle_time}ms")

    # Try to decode with kafka-python
    try:
        if version <= 5:
            decoder = MetadataResponse[version]
            # Create a mock BytesIO-like object
            import io
            buf = io.BytesIO(response)
            decoded = decoder.decode(buf)
            print(f"✓ kafka-python decoded successfully")
            print(f"  Topics: {[t.topic for t in decoded.topics]}")
        else:
            print(f"  kafka-python doesn't support v{version}")
    except Exception as e:
        print(f"✗ kafka-python decode failed: {e}")
        print(f"  Failed at offset ~{offset}")

# Test all versions
for version in [0, 1, 2, 3, 4, 5]:
    try:
        response = send_metadata_request(version, correlation_id=100 + version)
        analyze_response(version, response)
    except Exception as e:
        print(f"\nVersion {version} failed: {e}")