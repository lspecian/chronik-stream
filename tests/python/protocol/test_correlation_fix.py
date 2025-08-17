#!/usr/bin/env python3
"""Test if correlation ID issue is fixed"""

import socket
import struct

def test_api_versions():
    """Test API versions request"""
    # Connect
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.connect(('localhost', 9092))
    
    # API Versions request
    api_key = 18
    api_version = 0
    correlation_id = 42
    client_id = "test-client"
    
    # Build request
    request_body = struct.pack('>h', len(client_id))
    request_body += client_id.encode('utf-8')
    
    request_header = struct.pack('>hhi', api_key, api_version, correlation_id)
    request = request_header + request_body
    
    # Send with length prefix
    s.send(struct.pack('>i', len(request)) + request)
    
    # Read response
    length_bytes = s.recv(4)
    length = struct.unpack('>i', length_bytes)[0]
    response = s.recv(length)
    
    # Parse correlation ID from response
    response_correlation_id = struct.unpack('>i', response[:4])[0]
    
    print(f"Request correlation ID: {correlation_id}")
    print(f"Response correlation ID: {response_correlation_id}")
    print(f"Match: {correlation_id == response_correlation_id}")
    
    # Check if there's a double correlation ID
    if len(response) > 8:
        second_correlation_id = struct.unpack('>i', response[4:8])[0]
        print(f"Second correlation ID found: {second_correlation_id}")
        print(f"Response starts with: {response[:20].hex()}")
    
    s.close()
    
    return correlation_id == response_correlation_id

if __name__ == "__main__":
    success = test_api_versions()
    print(f"\nTest {'PASSED' if success else 'FAILED'}")