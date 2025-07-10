#\!/usr/bin/env python3
"""Test API versions request."""

import socket
import struct

def test_api_versions():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send API versions v3 request
    request = b''
    request += struct.pack('>h', 18)  # API Key (ApiVersions)
    request += struct.pack('>h', 3)   # API Version 3
    request += struct.pack('>i', 100) # Correlation ID
    request += struct.pack('>h', 8)   # Client ID length
    request += b'test-cli'            # Client ID
    # Empty tagged fields
    request += b'\x00'
    
    # Send
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    print(f"Sent API versions v3 request")
    
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
    
    # Parse correlation ID
    corr_id = struct.unpack('>i', response[0:4])[0]
    print(f"\nCorrelation ID = {corr_id}")
    
    sock.close()

if __name__ == "__main__":
    test_api_versions()