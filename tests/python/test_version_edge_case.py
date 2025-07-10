#!/usr/bin/env python3
"""Test if maybe the version is being misinterpreted."""

import socket
import struct

# What if the issue is with how we interpret the version?
# Let's send a request with version that might be edge cases

def test_version_interpretation():
    # Test if maybe negative version or very high version causes issues
    test_versions = [-1, 0, 1, 2, 3, 100, 32767, -32768]
    
    for version in test_versions:
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.connect(('localhost', 9092))
            
            request = b''
            request += struct.pack('>h', 3)       # API key
            request += struct.pack('>h', version) # API version (as signed 16-bit)
            request += struct.pack('>i', 2000)    # Correlation ID
            request += struct.pack('>h', -1)      # Client ID null
            
            # For valid versions, add the rest
            if 0 <= version <= 12:
                request += struct.pack('>i', -1)  # Topics
            
            full_request = struct.pack('>I', len(request)) + request
            sock.send(full_request)
            
            # Try to read response
            try:
                resp_len_bytes = sock.recv(4)
                if len(resp_len_bytes) == 4:
                    resp_len = struct.unpack('>I', resp_len_bytes)[0]
                    response = sock.recv(resp_len)
                    
                    if len(response) >= 8:
                        corr_id = struct.unpack('>I', response[:4])[0]
                        next_4 = response[4:8]
                        print(f"Version {version:5d}: Got response, next 4 bytes after corr_id: {next_4.hex()}")
                    else:
                        print(f"Version {version:5d}: Short response ({len(response)} bytes)")
                else:
                    print(f"Version {version:5d}: No response length")
            except:
                print(f"Version {version:5d}: Error reading response")
                
            sock.close()
        except Exception as e:
            print(f"Version {version:5d}: Connection error: {e}")

test_version_interpretation()