#!/usr/bin/env python3
"""Test the exact wire protocol response from both servers."""

import socket
import struct
import sys

def send_api_versions_v3(host, port):
    """Send ApiVersions v3 request and capture response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build ApiVersions v3 request
    api_key = 18  # ApiVersions
    api_version = 3
    correlation_id = 123
    client_id = "test-client"
    
    # v3 uses flexible encoding
    request = bytearray()
    request.extend(struct.pack('>h', api_key))
    request.extend(struct.pack('>h', api_version))
    request.extend(struct.pack('>i', correlation_id))
    
    # Flexible client_id (compact string)
    client_id_bytes = client_id.encode('utf-8')
    request.append(len(client_id_bytes) + 1)  # compact string length
    request.extend(client_id_bytes)
    
    # Tagged fields (empty)
    request.append(0)
    
    # Add size header
    full_request = struct.pack('>i', len(request)) + bytes(request)
    
    print(f"\n=== Testing {host}:{port} ===")
    print(f"Request size: {len(full_request)} bytes")
    
    sock.send(full_request)
    
    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print("ERROR: Could not read size header")
        return None
        
    response_size = struct.unpack('>i', size_bytes)[0]
    print(f"Response size: {response_size} bytes")
    
    response = sock.recv(response_size)
    while len(response) < response_size:
        response += sock.recv(response_size - len(response))
    
    sock.close()
    
    # Parse response header
    correlation_id = struct.unpack('>i', response[0:4])[0]
    print(f"Correlation ID: {correlation_id}")
    
    # Show hex dump of first 64 bytes
    print(f"First 64 bytes (hex):")
    for i in range(0, min(64, len(response)), 16):
        hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
        print(f"  {i:04x}: {hex_str}")
    
    # Parse the response to count APIs
    pos = 4  # Skip correlation ID
    
    # v3 response structure:
    # - error_code (INT16)
    # - api_versions (COMPACT_ARRAY)
    error_code = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    print(f"Error code: {error_code}")
    
    # Read compact array length (varint)
    array_len_encoded = response[pos]
    if array_len_encoded == 0:
        api_count = -1
    else:
        api_count = array_len_encoded - 1
    pos += 1
    
    print(f"API count (from compact array): {api_count}")
    
    # Show the varint byte
    print(f"Varint byte: 0x{response[6]:02x} (decimal: {response[6]})")
    
    return response

if __name__ == "__main__":
    # Test CP Kafka
    cp_response = send_api_versions_v3("localhost", 29092)
    
    # Test Chronik
    chronik_response = send_api_versions_v3("localhost", 9092)
    
    if cp_response and chronik_response:
        print("\n=== Comparison ===")
        print(f"CP Kafka response size: {len(cp_response)} bytes")
        print(f"Chronik response size: {len(chronik_response)} bytes")
        
        if len(cp_response) != len(chronik_response):
            print(f"SIZE MISMATCH: {len(chronik_response) - len(cp_response):+d} bytes")
        
        # Find first difference
        for i in range(min(len(cp_response), len(chronik_response))):
            if cp_response[i] != chronik_response[i]:
                print(f"First difference at byte {i}: CP=0x{cp_response[i]:02x}, Chronik=0x{chronik_response[i]:02x}")
                break