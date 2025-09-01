#!/usr/bin/env python3
"""
Compare ApiVersions responses between CP Kafka and Chronik.
"""

import socket
import struct

def send_apiversion_request(host, port, version=3):
    """Send ApiVersions request and get response."""
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        sock.connect((host, port))
        
        correlation_id = 123
        client_id = b'test-client'
        
        if version == 0:
            # v0 request
            request_body = b''
            request_body += struct.pack('>h', 18)  # API key
            request_body += struct.pack('>h', 0)   # API version
            request_body += struct.pack('>i', correlation_id)
            request_body += struct.pack('>h', len(client_id))
            request_body += client_id
        else:
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
        
        sock.send(request)
        
        # Read response
        size_bytes = sock.recv(4)
        if len(size_bytes) < 4:
            sock.close()
            return None
            
        size = struct.unpack('>i', size_bytes)[0]
        
        response = b''
        while len(response) < size:
            chunk = sock.recv(min(4096, size - len(response)))
            if not chunk:
                break
            response += chunk
        
        sock.close()
        return response
    except Exception as e:
        print(f"Error connecting to {host}:{port}: {e}")
        return None

def analyze_response(response, name):
    """Analyze response structure."""
    print(f"\n=== {name} Response ===")
    print(f"Size: {len(response)} bytes")
    
    # Show first 100 bytes
    print("\nFirst 100 bytes (hex):")
    for i in range(0, min(100, len(response)), 16):
        hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
        print(f"  {i:04x}: {hex_str}")
    
    # Parse header
    offset = 0
    correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"\nCorrelation ID: {correlation_id}")
    offset += 4
    
    # Check for tagged field after correlation ID
    print(f"Byte at offset 4: 0x{response[offset]:02x}")
    
    # Detailed analysis of next bytes
    print("\nDetailed byte analysis (offsets 4-20):")
    for i in range(4, min(20, len(response))):
        b = response[i]
        print(f"  Offset {i:2d}: 0x{b:02x} ({b:3d}) {'[printable: ' + chr(b) + ']' if 32 <= b < 127 else ''}")
    
    return response

def main():
    print("="*60)
    print("Comparing CP Kafka 7.5.0 vs Chronik ApiVersions Responses")
    print("="*60)
    
    # Test both with v0 first
    print("\n--- ApiVersions v0 Requests ---")
    
    cp_v0 = send_apiversion_request('localhost', 29092, version=0)
    if cp_v0:
        analyze_response(cp_v0, "CP Kafka v0")
    
    chronik_v0 = send_apiversion_request('localhost', 9092, version=0)
    if chronik_v0:
        analyze_response(chronik_v0, "Chronik v0")
    
    # Compare v0
    if cp_v0 and chronik_v0:
        if cp_v0 == chronik_v0:
            print("\n✅ v0 responses are IDENTICAL")
        else:
            print("\n❌ v0 responses DIFFER")
            for i in range(min(50, len(cp_v0), len(chronik_v0))):
                if cp_v0[i] != chronik_v0[i]:
                    print(f"  First difference at byte {i}: CP=0x{cp_v0[i]:02x} Chronik=0x{chronik_v0[i]:02x}")
                    break
    
    # Test both with v3
    print("\n\n--- ApiVersions v3 Requests ---")
    
    cp_v3 = send_apiversion_request('localhost', 29092, version=3)
    if cp_v3:
        analyze_response(cp_v3, "CP Kafka v3")
    
    chronik_v3 = send_apiversion_request('localhost', 9092, version=3)
    if chronik_v3:
        analyze_response(chronik_v3, "Chronik v3")
    
    # Compare v3
    if cp_v3 and chronik_v3:
        if cp_v3 == chronik_v3:
            print("\n✅ v3 responses are IDENTICAL")
        else:
            print("\n❌ v3 responses DIFFER")
            # Find first difference
            for i in range(min(len(cp_v3), len(chronik_v3))):
                if cp_v3[i] != chronik_v3[i]:
                    print(f"  First difference at byte {i}:")
                    print(f"    CP Kafka: 0x{cp_v3[i]:02x}")
                    print(f"    Chronik:  0x{chronik_v3[i]:02x}")
                    
                    # Show context
                    start = max(0, i-5)
                    end = min(len(cp_v3), len(chronik_v3), i+10)
                    print(f"\n  Context (bytes {start}-{end}):")
                    print(f"    CP:      {' '.join(f'{b:02x}' for b in cp_v3[start:end])}")
                    print(f"    Chronik: {' '.join(f'{b:02x}' for b in chronik_v3[start:end])}")
                    break

if __name__ == "__main__":
    main()