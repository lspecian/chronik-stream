#!/usr/bin/env python3
"""Test to verify exact response bytes"""

import socket
import struct

def test_response():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build ApiVersions v3 request
    request = bytearray()
    request.extend((0).to_bytes(4, 'big'))  # Size placeholder
    request.extend((18).to_bytes(2, 'big'))  # API key = 18
    request.extend((3).to_bytes(2, 'big'))   # API version = 3
    request.extend((99).to_bytes(4, 'big'))  # Correlation ID = 99
    request.extend(b'\x00\x10')              # Client ID length = 16
    request.extend(b'test-python-cli\x00')   # Client ID
    
    # Update size
    size = len(request) - 4
    request[0:4] = size.to_bytes(4, 'big')
    
    sock.send(request)
    
    # Read response
    response_size_bytes = sock.recv(4)
    response_size = int.from_bytes(response_size_bytes, 'big')
    response = sock.recv(response_size)
    
    print(f"Response size field: {response_size}")
    print(f"Actual response bytes: {len(response)}")
    print(f"First 50 bytes hex: {response[:50].hex()}")
    print(f"Last 10 bytes hex: {response[-10:].hex()}")
    
    # Check structure
    correlation_id = int.from_bytes(response[0:4], 'big')
    print(f"\nCorrelation ID: {correlation_id}")
    
    # Check if there's an error code at position 4
    error_at_4 = int.from_bytes(response[4:6], 'big', signed=True)
    print(f"INT16 at position 4: {error_at_4}")
    
    # Check byte at position 6
    byte_at_6 = response[6]
    print(f"Byte at position 6: 0x{byte_at_6:02x} ({byte_at_6})")
    
    sock.close()

if __name__ == "__main__":
    test_response()