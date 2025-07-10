#!/usr/bin/env python3
"""Verify that api_version is being parsed correctly."""

import socket
import struct

def test_version(version):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send request with specific version
    request = b''
    request += struct.pack('>h', 3)       # API key (Metadata)
    request += struct.pack('>h', version) # API version
    request += struct.pack('>i', 1000 + version)  # Correlation ID (1000 + version for tracking)
    request += struct.pack('>h', -1)      # Client ID (null)
    request += struct.pack('>i', -1)      # Topics (-1 = all)
    
    full_request = struct.pack('>I', len(request)) + request
    sock.send(full_request)
    
    # Read response
    resp_len = struct.unpack('>I', sock.recv(4))[0]
    response = sock.recv(resp_len)
    
    # Check correlation ID to make sure we got the right response
    corr_id = struct.unpack('>I', response[:4])[0]
    expected_corr_id = 1000 + version
    
    print(f"Version {version}: Correlation ID {corr_id} (expected {expected_corr_id})")
    
    # Check if throttle_time_ms is present
    # If it's present for v1/v2, the 4 bytes after correlation ID will be 0x00000000
    # For v3+, this is correct
    next_4_bytes = response[4:8]
    print(f"  Next 4 bytes after correlation ID: {next_4_bytes.hex()}")
    
    if version < 3 and next_4_bytes == b'\x00\x00\x00\x00':
        print(f"  ERROR: Version {version} should NOT have throttle_time_ms, but it does!")
    elif version >= 3 and next_4_bytes == b'\x00\x00\x00\x00':
        print(f"  OK: Version {version} correctly has throttle_time_ms=0")
    
    sock.close()

# Test versions 0-4
for v in range(5):
    test_version(v)
    print()