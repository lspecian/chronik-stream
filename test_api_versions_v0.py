#!/usr/bin/env python3
"""Test API versions v0 request - critical for kafkactl compatibility."""

import socket
import struct
import sys

def send_api_versions_v0():
    """Send an API versions v0 request and check field ordering."""
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))
        
        # Build API versions v0 request
        request = b''
        request += struct.pack('>h', 18)  # API Key (ApiVersions)
        request += struct.pack('>h', 0)   # API Version 0 - CRITICAL!
        request += struct.pack('>i', 123) # Correlation ID
        request += struct.pack('>h', 9)   # Client ID length
        request += b'test-v0-fix'         # Client ID
        
        # Send request with length prefix
        length_prefix = struct.pack('>i', len(request))
        sock.sendall(length_prefix + request)
        print(f"Sent API versions v0 request (length={len(request)})")
        
        # Read response length
        length_data = sock.recv(4)
        if len(length_data) != 4:
            print("ERROR: Failed to read response length")
            return False
            
        response_length = struct.unpack('>i', length_data)[0]
        print(f"Response length: {response_length}")
        
        # Read full response
        response = b''
        while len(response) < response_length:
            chunk = sock.recv(min(4096, response_length - len(response)))
            if not chunk:
                break
            response += chunk
        
        print(f"Response bytes ({len(response)}): {response.hex()}")
        
        # Parse response
        pos = 0
        
        # Correlation ID (4 bytes)
        corr_id = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"\nCorrelation ID: {corr_id}")
        
        # CRITICAL: For v0, the next field should be the API versions array, NOT error_code!
        # Array length (4 bytes)
        array_len = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"API versions array length: {array_len}")
        
        # Parse API versions
        print("\nAPI versions:")
        for i in range(array_len):
            api_key = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            min_version = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            max_version = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            print(f"  API {api_key}: v{min_version}-v{max_version}")
        
        # CRITICAL: Error code should come AFTER the array for v0
        error_code = struct.unpack('>h', response[pos:pos+2])[0]
        pos += 2
        print(f"\nError code: {error_code}")
        
        # Verify field ordering
        if error_code == 0:
            print("\n✅ SUCCESS: Field ordering appears correct for v0")
            print("   - API versions array came before error_code")
            return True
        else:
            print(f"\n❌ ERROR: Unexpected error code: {error_code}")
            return False
            
    except Exception as e:
        print(f"\n❌ ERROR: {e}")
        return False
    finally:
        sock.close()

if __name__ == "__main__":
    success = send_api_versions_v0()
    sys.exit(0 if success else 1)