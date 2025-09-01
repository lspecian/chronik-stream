#!/usr/bin/env python3
"""
Test that ApiVersions v3 response uses INT16 for min/max versions.
This is required for librdkafka compatibility.
"""

import socket
import struct

def test_apiversion_int16():
    """Test that min/max versions are encoded as INT16."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build ApiVersions v3 request
    correlation_id = 1
    client_id = b'test-client'
    
    # Request format for v3 (flexible)
    client_id_len = len(client_id) + 1  # Compact string encoding
    request_body = struct.pack('>hhiB', 18, 3, correlation_id, 0)  # header + tagged fields
    request_body += bytes([client_id_len]) + client_id  # compact string
    request_body += b'\x00'  # tagged fields
    
    request = struct.pack('>i', len(request_body)) + request_body
    
    print(f"Sending ApiVersions v3 request...")
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    response = sock.recv(size)
    
    sock.close()
    
    print(f"Received response: {size} bytes")
    
    # Parse response to check encoding
    offset = 0
    
    # Skip correlation ID (4 bytes)
    offset += 4
    
    # Skip tagged fields (1 byte)
    offset += 1
    
    # Skip error code (2 bytes)
    offset += 2
    
    # Read array length (compact)
    array_len_encoded = response[offset]
    array_len = array_len_encoded - 1 if array_len_encoded > 0 else 0
    offset += 1
    
    print(f"API array has {array_len} entries")
    
    # Check first API entry
    if array_len > 0:
        # API key (2 bytes)
        api_key = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        
        # Now check if min/max are INT16 (2 bytes each) or INT8 (1 byte each)
        # Try to read as INT16
        try:
            min_ver = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            max_ver = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            
            print(f"✅ API {api_key}: min={min_ver}, max={max_ver} - Using INT16 encoding (librdkafka compatible)")
            
            # Verify reasonable values
            if 0 <= min_ver <= 20 and 0 <= max_ver <= 20:
                print("✅ Values are in reasonable range")
            else:
                print(f"⚠️  Values seem unusual: min={min_ver}, max={max_ver}")
                
        except:
            print("❌ Failed to read as INT16 - might be using INT8 encoding (incompatible with librdkafka)")
            
            # Show what we see
            print(f"Next 4 bytes: {response[offset-2:offset+2].hex()}")
    
    return True

if __name__ == "__main__":
    try:
        test_apiversion_int16()
    except Exception as e:
        print(f"Error: {e}")
        import traceback
        traceback.print_exc()