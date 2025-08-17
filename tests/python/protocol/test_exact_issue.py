#!/usr/bin/env python3
"""Test to understand the exact issue."""

import socket
import struct

def test_metadata():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send metadata v1 request
    request = b''
    request += struct.pack('>h', 3)   # API Key (Metadata)
    request += struct.pack('>h', 1)   # API Version 1
    request += struct.pack('>i', 99)  # Correlation ID
    request += struct.pack('>h', 8)   # Client ID length
    request += b'test-cli'            # Client ID
    request += struct.pack('>i', -1)  # Topics: -1 for all
    
    # Send
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    print(f"Sent metadata v1 request")
    
    # Read response
    length_data = sock.recv(4)
    response_length = struct.unpack('>i', length_data)[0]
    print(f"Response length: {response_length}")
    
    response = b''
    while len(response) < response_length:
        chunk = sock.recv(min(4096, response_length - len(response)))
        if not chunk:
            break
        response += chunk
    
    print(f"Response hex: {response.hex()}")
    
    # Analyze
    offset = 0
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"\nOffset {offset}: Correlation ID = {corr_id}")
    offset += 4
    
    # Check what's at offset 4
    if offset + 4 <= len(response):
        val = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Offset {offset}: Next 4 bytes = {val}")
        if val == 0 or val == 1:
            print("  -> This could be throttle_time (0ms) OR broker count (1)")
        offset += 4
    
    # Check what's at offset 8
    if offset + 4 <= len(response):
        val = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Offset {offset}: Next 4 bytes = {val}")
        if val == 1:
            print("  -> If this is 1, then offset 4 was throttle_time!")
    
    sock.close()

if __name__ == "__main__":
    test_metadata()