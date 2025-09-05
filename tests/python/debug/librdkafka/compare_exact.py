#!/usr/bin/env python3
"""Compare exact responses from CP Kafka vs Chronik."""

import socket
import struct
import time

def get_response(host, port, name):
    """Get ApiVersions v3 response."""
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        sock.connect((host, port))
        
        # Build ApiVersions v3 request
        request = bytearray()
        request.extend(struct.pack('>h', 18))  # ApiVersions
        request.extend(struct.pack('>h', 3))   # v3
        request.extend(struct.pack('>i', 123)) # correlation_id
        request.append(12)  # client_id length+1
        request.extend(b'test-client')
        request.append(0)  # tagged fields
        
        full_request = struct.pack('>i', len(request)) + bytes(request)
        sock.send(full_request)
        
        # Read response
        size_bytes = sock.recv(4)
        if len(size_bytes) < 4:
            return None
            
        response_size = struct.unpack('>i', size_bytes)[0]
        response = sock.recv(response_size)
        while len(response) < response_size:
            response += sock.recv(response_size - len(response))
        sock.close()
        
        return response
        
    except Exception as e:
        print(f"{name}: Error - {e}")
        return None

# Test both
chronik = get_response("localhost", 9092, "Chronik")

# CP Kafka is on 29092
time.sleep(2)
cp_kafka = get_response("localhost", 29092, "CP Kafka")

if chronik:
    print(f"Chronik: {len(chronik)} bytes")
    print(f"First 64 bytes: {' '.join(f'{b:02x}' for b in chronik[:64])}")
    
    # Check the varint at position 6
    if len(chronik) > 6:
        varint = chronik[6]
        if varint == 0:
            print("ERROR: Varint is 0 (NULL array)")
        else:
            print(f"Varint at position 6: 0x{varint:02x} ({varint-1} APIs)")

if cp_kafka:
    print(f"\nCP Kafka: {len(cp_kafka)} bytes")
    print(f"First 64 bytes: {' '.join(f'{b:02x}' for b in cp_kafka[:64])}")
    
    # Check the varint at position 6
    if len(cp_kafka) > 6:
        varint = cp_kafka[6]
        print(f"Varint at position 6: 0x{varint:02x} ({varint-1} APIs)")
else:
    print("\nCP Kafka not available (might still be starting)")

if chronik and cp_kafka:
    print(f"\n=== Comparison ===")
    if len(chronik) != len(cp_kafka):
        print(f"Size difference: Chronik={len(chronik)}, CP={len(cp_kafka)} ({len(chronik)-len(cp_kafka):+d} bytes)")
    
    # Find first difference
    for i in range(min(len(chronik), len(cp_kafka))):
        if chronik[i] != cp_kafka[i]:
            print(f"First difference at byte {i}:")
            print(f"  Chronik: 0x{chronik[i]:02x}")
            print(f"  CP Kafka: 0x{cp_kafka[i]:02x}")
            break
    else:
        print("Responses are identical!")