#!/usr/bin/env python3
"""Test Metadata request/response"""

import socket
import struct

def test_metadata():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build Metadata v12 request (flexible version)
    request = bytearray()
    request.extend((0).to_bytes(4, 'big'))  # Size placeholder
    request.extend((3).to_bytes(2, 'big'))   # API key = 3 (Metadata)
    request.extend((12).to_bytes(2, 'big'))  # API version = 12
    request.extend((100).to_bytes(4, 'big')) # Correlation ID = 100
    
    # Client ID (compact string for v12)
    request.append(11)  # length + 1 = 11
    request.extend(b'test-py-md')
    
    # Tagged fields for header (v12 is flexible)
    request.append(0)  # No tagged fields
    
    # Request body - topics array (null = all topics)
    request.append(0)  # Compact array with 0 = null
    
    # Allow auto topic creation (boolean)
    request.append(1)  # true
    
    # Include cluster authorized operations (v8+)
    request.append(0)  # false
    
    # Include topic authorized operations (v8+)
    request.append(0)  # false
    
    # Tagged fields for body
    request.append(0)  # No tagged fields
    
    # Update size
    size = len(request) - 4
    request[0:4] = size.to_bytes(4, 'big')
    
    print(f"Sending Metadata v12 request ({len(request)} bytes)")
    sock.send(request)
    
    # Read response
    response_size_bytes = sock.recv(4)
    if len(response_size_bytes) < 4:
        print(f"Failed to read size, got {len(response_size_bytes)} bytes")
        return
    
    response_size = int.from_bytes(response_size_bytes, 'big')
    print(f"Response size: {response_size} bytes")
    
    response = sock.recv(response_size)
    print(f"Actual received: {len(response)} bytes")
    
    if len(response) < 10:
        print(f"Response too short! Hex: {response.hex()}")
    else:
        print(f"First 50 bytes hex: {response[:50].hex()}")
    
    sock.close()

if __name__ == "__main__":
    test_metadata()
