#!/usr/bin/env python3
"""Debug kafkactl connection issue."""

import socket
import struct
import time

def test_api_versions():
    """Test API versions request (kafkactl likely starts with this)."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send API versions request (API key 18)
    request = b''
    request += struct.pack('>h', 18)  # API Key (ApiVersions)
    request += struct.pack('>h', 0)   # API Version 0
    request += struct.pack('>i', 1)   # Correlation ID
    request += struct.pack('>h', -1)  # Client ID length (-1 = null)
    
    # Send
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read response
    length_data = sock.recv(4)
    response_length = struct.unpack('>i', length_data)[0]
    print(f"ApiVersions response length: {response_length}")
    response = sock.recv(response_length)
    
    # Parse response
    offset = 0
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {corr_id}")
    
    error_code = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    print(f"Error code: {error_code}")
    
    if error_code == 0:
        api_count = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"API count: {api_count}")
        
        for i in range(min(5, api_count)):  # Show first 5 APIs
            api_key = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            min_ver = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            max_ver = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            print(f"  API {api_key}: v{min_ver}-{max_ver}")
    
    sock.close()
    return error_code == 0

def test_metadata_after_versions():
    """Test metadata after API versions (like kafkactl would)."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # First send API versions
    request = b''
    request += struct.pack('>h', 18)  # API Key (ApiVersions)
    request += struct.pack('>h', 0)   # API Version 0
    request += struct.pack('>i', 1)   # Correlation ID
    request += struct.pack('>h', -1)  # Client ID length (-1 = null)
    
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read API versions response
    length_data = sock.recv(4)
    response_length = struct.unpack('>i', length_data)[0]
    response = sock.recv(response_length)
    
    # Now send metadata request
    request = b''
    request += struct.pack('>h', 3)   # API Key (Metadata)
    request += struct.pack('>h', 1)   # API Version 1 (kafkactl uses v1 by default)
    request += struct.pack('>i', 2)   # Correlation ID
    request += struct.pack('>h', 9)   # Client ID length
    request += b'kafkactl1'           # Client ID
    request += struct.pack('>i', -1)  # Topics array: -1 means all topics
    
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read metadata response
    length_data = sock.recv(4)
    response_length = struct.unpack('>i', length_data)[0]
    print(f"\nMetadata response length: {response_length}")
    
    response = sock.recv(response_length)
    
    # Debug first bytes
    print(f"First 20 bytes: {response[:20].hex()}")
    
    sock.close()

if __name__ == "__main__":
    print("Testing Kafka protocol flow...")
    if test_api_versions():
        print("\n✓ API versions succeeded")
        test_metadata_after_versions()
    else:
        print("\n✗ API versions failed")