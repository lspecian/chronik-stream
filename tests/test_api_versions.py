#!/usr/bin/env python3
"""Test API versions response from Chronik Stream"""

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

def send_api_versions_request():
    """Send ApiVersions request and parse response"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))

    # Build ApiVersions request (v0)
    # ApiVersions request has no body in v0
    header = encode_request_header(18, 0, 1, 'test-client')
    request = header  # No body for ApiVersions v0

    # Send with length prefix
    message = struct.pack('>i', len(request)) + request
    sock.send(message)

    # Read response
    response_len_bytes = sock.recv(4)
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

    # For ApiVersions v0, the response structure is:
    # - correlation_id (int32)
    # - api_versions array (int32 count + entries)
    # - error_code (int16)

    # API versions array
    api_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"API count: {api_count}")

    apis = []
    for i in range(api_count):
        api_key = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        min_version = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        max_version = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        apis.append((api_key, min_version, max_version))
        if i < 10:  # Print first 10
            print(f"  API {api_key}: v{min_version}-{max_version}")

    # Error code
    error_code = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    print(f"Error code: {error_code}")

    # Check if Metadata (API 3) is properly advertised
    metadata_api = next((api for api in apis if api[0] == 3), None)
    if metadata_api:
        print(f"\n✓ Metadata API found: v{metadata_api[1]}-{metadata_api[2]}")
        if metadata_api[1] == 0:
            print("✓ Metadata v0 is advertised as supported")
            return True
        else:
            print(f"✗ Metadata v0 NOT supported (min version is {metadata_api[1]})")
            return False
    else:
        print("✗ Metadata API not found in response")
        return False

    sock.close()

if __name__ == "__main__":
    try:
        success = send_api_versions_request()
        sys.exit(0 if success else 1)
    except Exception as e:
        print(f"Error: {e}")
        sys.exit(1)