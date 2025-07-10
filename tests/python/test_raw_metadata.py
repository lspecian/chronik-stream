#!/usr/bin/env python3
"""Test raw metadata request/response."""

import socket
import struct

def test_raw():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build minimal metadata request for v1
    request = b''
    request += struct.pack('>h', 3)      # API key (Metadata)
    request += struct.pack('>h', 1)      # API version 1
    request += struct.pack('>i', 99)     # Correlation ID
    request += struct.pack('>h', -1)     # Client ID (null)
    request += struct.pack('>i', -1)     # Topics array (-1 = all)
    
    # Send
    full_request = struct.pack('>I', len(request)) + request
    print(f"Sending {len(full_request)} bytes:")
    print(f"Request hex: {full_request.hex()}")
    sock.send(full_request)
    
    # Receive
    resp_len = struct.unpack('>I', sock.recv(4))[0]
    print(f"\nResponse length field: {resp_len}")
    
    response = sock.recv(resp_len)
    print(f"Response hex: {response.hex()}")
    print(f"\nDecoding response:")
    
    # Expected for v1:
    # - Correlation ID (4 bytes)
    # - Brokers array
    # - Controller ID  
    # - Topics array
    
    offset = 0
    corr_id = struct.unpack('>I', response[offset:offset+4])[0]
    print(f"Correlation ID: {corr_id} (expected 99)")
    offset += 4
    
    # Check what's next
    print(f"\nNext 16 bytes as hex: {response[offset:offset+16].hex()}")
    print("If brokers array starts here, first 4 bytes should be broker count")
    
    sock.close()

if __name__ == "__main__":
    test_raw()