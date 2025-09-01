#!/usr/bin/env python3
"""
Capture ApiVersions response from real Kafka and compare with Chronik.
"""

import socket
import struct
import sys

def get_apiversion_response(host, port):
    """Get ApiVersions response from a Kafka broker."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build ApiVersions v3 request
    correlation_id = 1
    client_id = b'test-client'
    
    # Request format for v3 (flexible)
    client_id_len = len(client_id) + 1  # Compact string encoding
    request_body = struct.pack('>hhiB', 18, 3, correlation_id, 0)  # header + tagged fields
    request_body += bytes([client_id_len]) + client_id  # compact string
    request_body += b'\x00'  # tagged fields
    
    request = struct.pack('>i', len(request_body)) + request_body
    
    print(f"Sending ApiVersions v3 request to {host}:{port}...")
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print(f"Failed to read size, got {len(size_bytes)} bytes")
        return None
        
    size = struct.unpack('>i', size_bytes)[0]
    print(f"Response size: {size} bytes")
    
    response = b''
    while len(response) < size:
        chunk = sock.recv(size - len(response))
        if not chunk:
            break
        response += chunk
    
    sock.close()
    
    return response

def analyze_response(response, source):
    """Analyze ApiVersions response structure."""
    print(f"\n{'='*60}")
    print(f"Response from {source}: {len(response)} bytes")
    print(f"{'='*60}")
    
    # Show hex dump
    print("\nHex dump (first 100 bytes):")
    for i in range(0, min(100, len(response)), 16):
        hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
        print(f"  {i:04x}: {hex_str}")
    
    # Parse header
    offset = 0
    correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"\nCorrelation ID: {correlation_id}")
    offset += 4
    
    # Tagged fields after correlation ID
    tagged_fields = response[offset]
    print(f"Tagged fields after correlation ID: 0x{tagged_fields:02x}")
    offset += 1
    
    # Error code
    error_code = struct.unpack('>h', response[offset:offset+2])[0]
    print(f"Error code: {error_code}")
    offset += 2
    
    # API array length (compact)
    array_len_encoded = response[offset]
    array_len = array_len_encoded - 1 if array_len_encoded > 0 else 0
    print(f"API array length: {array_len} (encoded as 0x{array_len_encoded:02x})")
    offset += 1
    
    # Parse first few APIs
    print("\nFirst 5 API entries:")
    for i in range(min(5, array_len)):
        if offset + 6 > len(response):
            print(f"  Not enough data for API {i}")
            break
            
        api_key = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        
        # Check if we're reading INT8 or INT16 for min/max
        # Try INT16 first (what librdkafka expects)
        if offset + 4 <= len(response):
            min_ver_int16 = struct.unpack('>h', response[offset:offset+2])[0]
            max_ver_int16 = struct.unpack('>h', response[offset+2:offset+4])[0]
            
            if 0 <= min_ver_int16 <= 20 and 0 <= max_ver_int16 <= 20:
                print(f"  API {api_key:3d}: v{min_ver_int16}-v{max_ver_int16} (INT16 encoding)")
                offset += 4
            else:
                # Might be INT8
                min_ver_int8 = response[offset]
                max_ver_int8 = response[offset+1]
                print(f"  API {api_key:3d}: v{min_ver_int8}-v{max_ver_int8} (INT8 encoding)")
                offset += 2
                
        # Skip tagged fields for this API
        api_tagged = response[offset] if offset < len(response) else 0
        offset += 1
    
    return offset

def main():
    # Test real Kafka
    print("Testing real Kafka...")
    kafka_response = get_apiversion_response('localhost', 9093)
    if kafka_response:
        analyze_response(kafka_response, "Real Kafka")
    
    # Test Chronik
    print("\n\nTesting Chronik Stream...")
    chronik_response = get_apiversion_response('localhost', 9092)
    if chronik_response:
        analyze_response(chronik_response, "Chronik Stream")
    
    # Compare
    if kafka_response and chronik_response:
        print(f"\n\n{'='*60}")
        print("COMPARISON")
        print(f"{'='*60}")
        print(f"Kafka response size: {len(kafka_response)} bytes")
        print(f"Chronik response size: {len(chronik_response)} bytes")
        
        # Find differences
        min_len = min(len(kafka_response), len(chronik_response))
        first_diff = None
        for i in range(min_len):
            if kafka_response[i] != chronik_response[i]:
                first_diff = i
                break
        
        if first_diff:
            print(f"\nFirst difference at byte {first_diff}:")
            start = max(0, first_diff - 8)
            end = min(min_len, first_diff + 8)
            
            print(f"  Kafka:   {' '.join(f'{b:02x}' for b in kafka_response[start:end])}")
            print(f"  Chronik: {' '.join(f'{b:02x}' for b in chronik_response[start:end])}")
            print(f"           {' ' * (3 * (first_diff - start))}^^")
        else:
            print("\nResponses are identical!")

if __name__ == "__main__":
    main()