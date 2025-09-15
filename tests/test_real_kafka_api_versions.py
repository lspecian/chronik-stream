#!/usr/bin/env python3
"""Test API versions response from real Kafka to compare"""

import socket
import struct
import sys

def test_real_kafka():
    """Test against a real Kafka broker (if available)"""
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(2)
        sock.connect(('localhost', 9093))  # Try port 9093 for real Kafka

        # Build ApiVersions request v0
        header = struct.pack('>h', 18)  # API key
        header += struct.pack('>h', 0)  # API version
        header += struct.pack('>i', 1)  # Correlation ID
        header += struct.pack('>h', -1)  # Null client ID

        request = header  # No body for ApiVersions v0

        # Send with length prefix
        message = struct.pack('>i', len(request)) + request
        sock.send(message)

        # Read response
        response_len_bytes = sock.recv(4)
        response_len = struct.unpack('>i', response_len_bytes)[0]
        print(f"Real Kafka response length: {response_len}")

        response = sock.recv(response_len)
        print(f"First 100 bytes hex: {response[:100].hex()}")

        # Parse to understand structure
        offset = 0

        # Correlation ID
        correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"Correlation ID: {correlation_id}")

        # Next 4 bytes
        next_val = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Next int32 value: {next_val} (0x{next_val:08x})")

        sock.close()
        return True

    except Exception as e:
        print(f"Could not connect to real Kafka on port 9093: {e}")
        return False

def decode_our_response():
    """Decode our response to understand structure"""
    # This is the response from our server as seen by Python client
    response = bytes.fromhex('000000010000003c00000000000900010000000d00020000000700030000000c')

    print("\nDecoding our response:")
    offset = 0

    # Correlation ID
    correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id}")

    # Next value (should be API count for v0)
    api_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"API count: {api_count}")

    # First API entry
    api_key = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    min_ver = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    max_ver = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    print(f"First API: key={api_key}, min={min_ver}, max={max_ver}")

if __name__ == "__main__":
    decode_our_response()
    test_real_kafka()