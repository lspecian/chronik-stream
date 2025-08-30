#!/usr/bin/env python3
"""Test ApiVersions request/response to debug version detection"""

import struct
import socket
import sys

def send_api_versions_v0():
    """Send ApiVersions v0 request and parse response"""
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    print("Connected to server")
    
    # Build ApiVersions v0 request
    api_key = 18  # ApiVersions
    api_version = 0
    correlation_id = 999
    client_id = b"test-versions"
    
    # Request header
    request = struct.pack('!hhih', api_key, api_version, correlation_id, len(client_id))
    request += client_id
    
    # No body for ApiVersions v0
    
    # Add size prefix
    full_request = struct.pack('!i', len(request)) + request
    
    print(f"Sending ApiVersions v0 request ({len(full_request)} bytes)")
    sock.send(full_request)
    
    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) != 4:
        print(f"Failed to read response size: got {len(size_bytes)} bytes")
        return False
        
    response_size = struct.unpack('!i', size_bytes)[0]
    print(f"Response size: {response_size} bytes")
    
    response = sock.recv(response_size)
    if len(response) < response_size:
        print(f"Incomplete response: got {len(response)} of {response_size} bytes")
        return False
    
    # Parse response
    offset = 0
    
    # Correlation ID
    correlation_id_resp = struct.unpack('!i', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id_resp} (expected {correlation_id})")
    
    if correlation_id_resp != correlation_id:
        print("ERROR: Correlation ID mismatch!")
        return False
    
    # For v0, the response format is:
    # correlation_id (4 bytes)
    # api_versions array size (4 bytes)
    # array of [api_key(2), min_version(2), max_version(2)]
    # error_code (2 bytes)
    
    # Array size
    array_size = struct.unpack('!i', response[offset:offset+4])[0]
    offset += 4
    print(f"Number of supported APIs: {array_size}")
    
    # Parse each API
    for i in range(array_size):
        if offset + 6 > len(response):
            print(f"ERROR: Response too short at API {i}")
            return False
            
        api_key, min_ver, max_ver = struct.unpack('!hhh', response[offset:offset+6])
        offset += 6
        print(f"  API {api_key}: versions {min_ver}-{max_ver}")
    
    # Error code
    if offset + 2 > len(response):
        print(f"ERROR: Response too short for error code (offset={offset}, len={len(response)})")
        print(f"Remaining bytes: {response[offset:].hex()}")
        return False
        
    error_code = struct.unpack('!h', response[offset:offset+2])[0]
    offset += 2
    print(f"Error code: {error_code}")
    
    if error_code != 0:
        print(f"ERROR: Non-zero error code: {error_code}")
        return False
    
    print(f"Successfully parsed ApiVersions response!")
    print(f"Total bytes parsed: {offset} of {len(response)}")
    
    if offset < len(response):
        print(f"WARNING: Extra bytes at end: {response[offset:].hex()}")
    
    sock.close()
    return True

if __name__ == "__main__":
    success = send_api_versions_v0()
    sys.exit(0 if success else 1)