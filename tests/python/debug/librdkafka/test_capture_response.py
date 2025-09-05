#!/usr/bin/env python3
"""Capture the exact response from Chronik for ApiVersions v3"""

import socket
import struct

def send_api_versions_v3_request():
    """Send ApiVersions v3 request and capture response"""
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build ApiVersions v3 request
    request = bytearray()
    
    # Request size (will be filled later)
    request.extend((0).to_bytes(4, 'big'))
    
    # Request header (not flexible for ApiVersions)
    request.extend((18).to_bytes(2, 'big'))  # API key = 18 (ApiVersions)
    request.extend((3).to_bytes(2, 'big'))   # API version = 3
    request.extend((99).to_bytes(4, 'big'))  # Correlation ID = 99
    request.extend(b'\x00\x10')              # Client ID length = 16
    request.extend(b'test-python-cli\x00')   # Client ID (null-terminated)
    
    # No request body for ApiVersions
    
    # Update request size
    size = len(request) - 4
    request[0:4] = size.to_bytes(4, 'big')
    
    print(f"Sending ApiVersions v3 request ({len(request)} bytes):")
    print(f"  Hex: {request.hex()}")
    
    sock.send(request)
    
    # Read response
    response_size_bytes = sock.recv(4)
    response_size = int.from_bytes(response_size_bytes, 'big')
    print(f"\nResponse size: {response_size} bytes")
    
    response = sock.recv(response_size)
    print(f"Response received ({len(response)} bytes):")
    print(f"  Hex: {response.hex()}")
    
    # Parse response
    print("\nParsing response:")
    pos = 0
    
    # Response header
    correlation_id = int.from_bytes(response[pos:pos+4], 'big')
    pos += 4
    print(f"  Correlation ID: {correlation_id}")
    
    # Response body (flexible for v3)
    error_code = int.from_bytes(response[pos:pos+2], 'big')
    pos += 2
    print(f"  Error code: {error_code}")
    
    # Check what comes next
    print(f"\nRemaining bytes from position {pos}:")
    print(f"  Next 10 bytes: {response[pos:pos+10].hex()}")
    
    # Try to parse as compact array
    array_len = response[pos]
    if array_len > 0:
        actual_len = array_len - 1
        print(f"  Compact array length: {array_len} (means {actual_len} items)")
        pos += 1
        
        # First API - let's carefully check each field
        if pos + 7 < len(response):
            api_key = int.from_bytes(response[pos:pos+2], 'big')
            field2 = int.from_bytes(response[pos+2:pos+4], 'big')
            field3 = int.from_bytes(response[pos+4:pos+6], 'big')
            tagged = response[pos+6]
            print(f"  First API bytes: {response[pos:pos+7].hex()}")
            print(f"    api_key={api_key}, field2={field2}, field3={field3}, tagged={tagged}")
    
    sock.close()

if __name__ == "__main__":
    send_api_versions_v3_request()