#!/usr/bin/env python3
"""Test metadata responses for different versions."""

import socket
import struct

def send_metadata_request(version):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build request
    request = b''
    request += struct.pack('>h', 3)   # API Key (Metadata)
    request += struct.pack('>h', version)   # API Version
    request += struct.pack('>i', 100 + version)  # Correlation ID
    request += struct.pack('>h', 8)   # Client ID length
    request += b'test-cli'            # Client ID
    
    if version >= 1:
        request += struct.pack('>i', -1)  # Topics: -1 for all
    
    if version >= 4:
        request += b'\x01'  # allow_auto_topic_creation = true
        
    if version >= 8:
        request += b'\x00'  # include_cluster_authorized_operations = false
        request += b'\x00'  # include_topic_authorized_operations = false
    
    # Send
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read response
    length_data = sock.recv(4)
    response_length = struct.unpack('>i', length_data)[0]
    
    response = b''
    while len(response) < response_length:
        chunk = sock.recv(min(4096, response_length - len(response)))
        if not chunk:
            break
        response += chunk
    
    sock.close()
    return response

def analyze_response(version, response):
    print(f"\n=== Metadata v{version} Response ===")
    print(f"Total length: {len(response)} bytes")
    print(f"First 32 bytes: {response[:32].hex()}")
    
    offset = 0
    
    # Correlation ID
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"Offset {offset:3d}: Correlation ID = {corr_id}")
    offset += 4
    
    # Check what's at offset 4
    if offset + 4 <= len(response):
        val = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Offset {offset:3d}: Next 4 bytes = {val}")
        
        # For v0-v2, this should be broker count
        # For v3+, this should be throttle_time
        if version < 3:
            print(f"            ^ Expected: broker count")
            if val == 0 or val == 1:
                print(f"            ^ WARNING: Might be throttle_time (should not exist for v{version})")
        else:
            print(f"            ^ Expected: throttle_time_ms")
            
    return response

# Test versions 0 through 9
for v in range(10):
    try:
        response = send_metadata_request(v)
        analyze_response(v, response)
    except Exception as e:
        print(f"\nError testing v{v}: {e}")

# Compare v1 and v3 responses
print("\n=== COMPARISON ===")
v1_resp = send_metadata_request(1)
v3_resp = send_metadata_request(3)

print(f"v1 response length: {len(v1_resp)}")
print(f"v3 response length: {len(v3_resp)}")
print(f"Difference: {len(v3_resp) - len(v1_resp)} bytes")

# Check if v1 has throttle_time
v1_offset_4 = struct.unpack('>i', v1_resp[4:8])[0]
v1_offset_8 = struct.unpack('>i', v1_resp[8:12])[0] if len(v1_resp) > 11 else None

print(f"\nv1 offset 4: {v1_offset_4}")
print(f"v1 offset 8: {v1_offset_8}")

if v1_offset_4 in [0, 1] and v1_offset_8 == 1:
    print("ERROR: v1 response includes throttle_time_ms field!")
else:
    print("OK: v1 response structure looks correct")