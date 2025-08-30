#!/usr/bin/env python3
"""Test ApiVersions v4 request with compact encoding"""

import struct
import socket
import sys

def write_compact_string(s):
    """Write a compact string (length+1 as varint, then bytes)"""
    if s is None:
        return bytes([0])  # Null string
    
    data = s.encode('utf-8') if isinstance(s, str) else s
    length = len(data) + 1
    
    result = b''
    # Write varint length
    while length >= 0x80:
        result += bytes([length & 0x7F | 0x80])
        length >>= 7
    result += bytes([length])
    
    # Write string data
    result += data
    return result

def send_api_versions_v4():
    """Send ApiVersions v4 request (with compact strings)"""
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    print("Connected to server")
    
    # Build ApiVersions v4 request (uses flexible versions)
    api_key = 18  # ApiVersions
    api_version = 4  # v4 uses compact encoding
    correlation_id = 888
    
    # For v4, we need compact encoding
    # Header format for flexible versions (v2+):
    # api_key (2), api_version (2), correlation_id (4), client_id (compact string), tagged_fields (0)
    
    request = struct.pack('!hhi', api_key, api_version, correlation_id)
    request += write_compact_string("test-v4")  # client_id as compact string
    request += bytes([0])  # Empty tagged fields
    
    # Body for v4: client_software_name and client_software_version as compact strings
    request += write_compact_string("kafka-python")
    request += write_compact_string("2.0.2")
    request += bytes([0])  # Empty tagged fields
    
    # Add size prefix
    full_request = struct.pack('!i', len(request)) + request
    
    print(f"Sending ApiVersions v4 request ({len(full_request)} bytes)")
    print(f"Request hex: {full_request.hex()}")
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
    
    print(f"Response hex (first 50 bytes): {response[:50].hex()}")
    
    # For v4 response parsing would be complex due to compact encoding
    # Just check correlation ID
    correlation_id_resp = struct.unpack('!i', response[0:4])[0]
    print(f"Correlation ID: {correlation_id_resp} (expected {correlation_id})")
    
    if correlation_id_resp != correlation_id:
        print("ERROR: Correlation ID mismatch!")
        # Maybe it's not where we expect?
        print(f"First 20 bytes: {response[:20].hex()}")
        return False
    
    sock.close()
    return True

if __name__ == "__main__":
    success = send_api_versions_v4()
    sys.exit(0 if success else 1)