#!/usr/bin/env python3
"""
Test real Kafka ApiVersions response on port 19092.
"""

import socket
import struct

def send_and_capture(host, port, version):
    """Send ApiVersions request and capture response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    correlation_id = 1
    client_id = b'test-client'
    
    # Build request based on version
    if version == 0:
        # v0 request
        request_body = b''
        request_body += struct.pack('>h', 18)  # API key
        request_body += struct.pack('>h', 0)   # API version
        request_body += struct.pack('>i', correlation_id)
        request_body += struct.pack('>h', len(client_id))
        request_body += client_id
    else:  # v3
        # v3 request (flexible)
        request_body = b''
        request_body += struct.pack('>h', 18)  # API key
        request_body += struct.pack('>h', 3)   # API version
        request_body += struct.pack('>i', correlation_id)
        request_body += b'\x00'  # Tagged fields in header
        
        # Compact string for client ID
        client_id_len = len(client_id) + 1
        request_body += bytes([client_id_len])
        request_body += client_id
        
        # Request body for v3
        request_body += bytes([12])  # ClientSoftwareName
        request_body += b'test-client'
        request_body += bytes([6])   # ClientSoftwareVersion
        request_body += b'1.0.0'
        request_body += b'\x00'  # Tagged fields
    
    request = struct.pack('>i', len(request_body)) + request_body
    
    print(f"Sending ApiVersions v{version} request to {host}:{port}")
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print(f"Failed to read size")
        return None
        
    size = struct.unpack('>i', size_bytes)[0]
    print(f"Response size: {size} bytes")
    
    response = b''
    while len(response) < size:
        chunk = sock.recv(min(4096, size - len(response)))
        if not chunk:
            break
        response += chunk
    
    sock.close()
    return response

def analyze_response(response, version):
    """Analyze the response structure."""
    print(f"\n--- ApiVersions v{version} Response Analysis ---")
    
    # Show hex dump
    print("First 50 bytes:")
    for i in range(0, min(50, len(response)), 16):
        hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
        print(f"  {i:04x}: {hex_str}")
    
    offset = 0
    
    # Correlation ID
    correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"\nCorrelation ID: {correlation_id}")
    offset += 4
    
    if version >= 3:
        # Check for tagged field
        print(f"Byte after correlation ID: 0x{response[offset]:02x}")
        if response[offset] == 0x00:
            print("  -> Has tagged field (0x00)")
            offset += 1
        else:
            print("  -> NO tagged field (goes directly to next field)")
    
    # The next field depends on version
    if version == 0:
        # v0: array comes first
        array_len = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Array length: {array_len}")
        offset += 4
        
        # Show first API
        if array_len > 0:
            api_key = struct.unpack('>h', response[offset:offset+2])[0]
            min_ver = struct.unpack('>h', response[offset+2:offset+4])[0]
            max_ver = struct.unpack('>h', response[offset+4:offset+6])[0]
            print(f"First API: {api_key} (v{min_ver}-v{max_ver})")
    else:
        # v3+: error code comes first
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        print(f"Error code: {error_code}")
        offset += 2
        
        # Then compact array
        array_len_encoded = response[offset]
        array_len = array_len_encoded - 1 if array_len_encoded > 0 else 0
        print(f"Array length: {array_len} (encoded as 0x{array_len_encoded:02x})")
        offset += 1
        
        # Show first API
        if array_len > 0:
            api_key = struct.unpack('>h', response[offset:offset+2])[0]
            # The question is: are min/max INT8 or INT16?
            # Try INT16 first
            min_ver = struct.unpack('>h', response[offset+2:offset+4])[0]
            max_ver = struct.unpack('>h', response[offset+4:offset+6])[0]
            print(f"First API (reading as INT16): {api_key} (v{min_ver}-v{max_ver})")
            
            # Also try INT8
            min_ver_int8 = response[offset+2]
            max_ver_int8 = response[offset+3]
            print(f"First API (reading as INT8): {api_key} (v{min_ver_int8}-v{max_ver_int8})")
    
    return response

def main():
    print("="*60)
    print("Testing REAL KAFKA on port 19092")
    print("="*60)
    
    # Test v0
    kafka_v0 = send_and_capture('localhost', 19092, 0)
    if kafka_v0:
        analyze_response(kafka_v0, 0)
    
    # Test v3
    kafka_v3 = send_and_capture('localhost', 19092, 3)
    if kafka_v3:
        analyze_response(kafka_v3, 3)
    
    print("\n" + "="*60)
    print("Testing CHRONIK on port 9092")
    print("="*60)
    
    # Test v0
    chronik_v0 = send_and_capture('localhost', 9092, 0)
    if chronik_v0:
        analyze_response(chronik_v0, 0)
    
    # Test v3
    chronik_v3 = send_and_capture('localhost', 9092, 3)
    if chronik_v3:
        analyze_response(chronik_v3, 3)
    
    # Compare v3 responses specifically
    if kafka_v3 and chronik_v3:
        print("\n" + "="*60)
        print("V3 RESPONSE COMPARISON (first 20 bytes)")
        print("="*60)
        for i in range(min(20, len(kafka_v3), len(chronik_v3))):
            k = kafka_v3[i]
            c = chronik_v3[i]
            match = "✓" if k == c else "✗"
            print(f"  Byte {i:2d}: Kafka=0x{k:02x}  Chronik=0x{c:02x}  {match}")
            
        # Specific check for tagged field
        print("\nKey difference at byte 4 (after correlation ID):")
        print(f"  Kafka:   0x{kafka_v3[4]:02x} - {'Tagged field' if kafka_v3[4] == 0x00 else 'NO tagged field'}")
        print(f"  Chronik: 0x{chronik_v3[4]:02x} - {'Tagged field' if chronik_v3[4] == 0x00 else 'NO tagged field'}")

if __name__ == "__main__":
    main()