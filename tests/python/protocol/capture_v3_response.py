#!/usr/bin/env python3
"""
Capture ApiVersions v3 response from real Kafka to see how it handles tagged fields.
"""

import socket
import struct

def send_apiversion_v3_request(host, port):
    """Send ApiVersions v3 request."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build ApiVersions v3 request (flexible format)
    correlation_id = 123
    client_id = b'test-client'
    
    # Request header for v3 (flexible)
    request_body = b''
    request_body += struct.pack('>h', 18)  # API key (ApiVersions)
    request_body += struct.pack('>h', 3)   # API version (v3)
    request_body += struct.pack('>i', correlation_id)  # Correlation ID
    request_body += b'\x00'  # Tagged fields in header
    
    # Compact string for client ID
    client_id_len = len(client_id) + 1
    request_body += bytes([client_id_len])  # Compact string length
    request_body += client_id
    
    # Request body for v3
    request_body += bytes([12])  # Compact string for ClientSoftwareName
    request_body += b'test-client'
    request_body += bytes([6])  # Compact string for ClientSoftwareVersion  
    request_body += b'1.0.0'
    request_body += b'\x00'  # Tagged fields in body
    
    request = struct.pack('>i', len(request_body)) + request_body
    
    print(f"Sending ApiVersions v3 request to {host}:{port}")
    print(f"Request size: {len(request)} bytes")
    print(f"Request hex: {request.hex()}")
    
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print(f"Failed to read size, got {len(size_bytes)} bytes")
        return None
        
    size = struct.unpack('>i', size_bytes)[0]
    print(f"Response size: {size} bytes")
    
    # Check for reasonable size
    if size > 10000 or size < 0:
        print(f"WARNING: Unexpected response size: {size}")
        # Try to read anyway (up to 1000 bytes)
        response = sock.recv(min(1000, abs(size)))
    else:
        response = b''
        while len(response) < size:
            chunk = sock.recv(min(4096, size - len(response)))
            if not chunk:
                break
            response += chunk
    
    sock.close()
    
    print(f"Received {len(response)} bytes")
    return response

def analyze_v3_response(response, source):
    """Analyze v3 response structure."""
    print(f"\n{'='*60}")
    print(f"Response from {source} (v3 format)")
    print(f"{'='*60}")
    
    # Show hex dump
    print("\nHex dump (first 100 bytes):")
    for i in range(0, min(100, len(response)), 16):
        hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
        ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in response[i:i+16])
        print(f"  {i:04x}: {hex_str:<48} {ascii_str}")
    
    # Parse v3 response
    print("\n--- Parsing as v3 response ---")
    offset = 0
    
    # Correlation ID
    if offset + 4 <= len(response):
        correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Correlation ID: {correlation_id}")
        offset += 4
    else:
        print("Not enough data for correlation ID")
        return
    
    # Tagged fields after correlation ID?
    print(f"\nByte at offset {offset} (after correlation ID): 0x{response[offset]:02x}")
    
    # Check if it looks like a tagged field (should be 0x00 for empty)
    if response[offset] == 0x00:
        print("  -> Looks like tagged field (0x00 = empty)")
        offset += 1
    else:
        print("  -> Does NOT look like tagged field")
    
    # Error code
    if offset + 2 <= len(response):
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        print(f"\nError code at offset {offset}: {error_code}")
        offset += 2
    
    # Array length (compact)
    if offset < len(response):
        array_len_encoded = response[offset]
        array_len = array_len_encoded - 1 if array_len_encoded > 0 else 0
        print(f"\nArray length at offset {offset}: {array_len} (encoded as 0x{array_len_encoded:02x})")
        offset += 1
    
    # Show next few bytes
    print(f"\nNext 16 bytes from offset {offset}:")
    next_bytes = response[offset:offset+16]
    print(f"  {' '.join(f'{b:02x}' for b in next_bytes)}")

def main():
    # Test real Kafka with v3
    print("Testing real Kafka with ApiVersions v3...")
    kafka_response = send_apiversion_v3_request('localhost', 9093)
    if kafka_response:
        analyze_v3_response(kafka_response, "Real Kafka")
        with open('/tmp/kafka_v3_response.bin', 'wb') as f:
            f.write(kafka_response)
    
    print("\n" + "="*60)
    
    # Test Chronik with v3
    print("\nTesting Chronik with ApiVersions v3...")
    chronik_response = send_apiversion_v3_request('localhost', 9092)
    if chronik_response:
        analyze_v3_response(chronik_response, "Chronik")
        with open('/tmp/chronik_v3_response.bin', 'wb') as f:
            f.write(chronik_response)
    
    # Compare
    if kafka_response and chronik_response:
        print("\n" + "="*60)
        print("COMPARISON")
        print("="*60)
        
        min_len = min(len(kafka_response), len(chronik_response))
        for i in range(min(20, min_len)):
            k = kafka_response[i] if i < len(kafka_response) else 0
            c = chronik_response[i] if i < len(chronik_response) else 0
            match = "✓" if k == c else "✗"
            print(f"  Byte {i:2d}: Kafka=0x{k:02x}  Chronik=0x{c:02x}  {match}")

if __name__ == "__main__":
    main()