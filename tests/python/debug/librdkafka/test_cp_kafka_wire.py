#!/usr/bin/env python3
"""Test CP Kafka wire protocol response for ApiVersions."""

import socket
import struct
import time

# Wait for CP Kafka to be ready
time.sleep(5)

def test_kafka(port, name):
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        sock.connect(("localhost", port))
        
        # Build ApiVersions v3 request
        request = bytearray()
        request.extend(struct.pack('>h', 18))  # ApiVersions
        request.extend(struct.pack('>h', 3))   # v3
        request.extend(struct.pack('>i', 123)) # correlation_id
        request.append(12)  # client_id length+1 (compact string)
        request.extend(b'test-client')
        request.append(0)  # tagged fields
        
        full_request = struct.pack('>i', len(request)) + bytes(request)
        sock.send(full_request)
        
        # Read response
        size_bytes = sock.recv(4)
        if len(size_bytes) < 4:
            print(f"{name}: Could not read response")
            return None
            
        response_size = struct.unpack('>i', size_bytes)[0]
        response = sock.recv(response_size)
        while len(response) < response_size:
            response += sock.recv(response_size - len(response))
        sock.close()
        
        print(f"\n=== {name} ===")
        print(f"Response size: {response_size} bytes")
        
        # Show hex dump focusing on the varint area
        print("Hex dump around varint position:")
        for i in range(0, min(32, len(response))):
            if i % 16 == 0:
                print(f"\n  {i:04x}: ", end="")
            print(f"{response[i]:02x} ", end="")
            if i == 3:
                print("| ", end="")  # After correlation ID
            elif i == 5:
                print("| ", end="")  # After error code
            elif i == 6:
                print("<-varint? ", end="")
        print()
        
        # Parse the varint at position 6
        pos = 6
        varint_value = 0
        shift = 0
        while pos < len(response):
            byte = response[pos]
            varint_value |= (byte & 0x7F) << shift
            pos += 1
            if (byte & 0x80) == 0:
                break
            shift += 7
        
        print(f"\nVarint at position 6: {varint_value}")
        if varint_value > 0:
            print(f"Array length: {varint_value - 1}")
        else:
            print("NULL array (varint = 0)")
            
        return response
        
    except Exception as e:
        print(f"{name}: Error - {e}")
        return None

# Test CP Kafka on port 9092 (internal port mapped to 29092)
# But since we're running this inside Docker context, we use external port
cp_response = test_kafka(29092, "CP Kafka (port 29092)")
chronik_response = test_kafka(9092, "Chronik (port 9092)")

if cp_response and chronik_response:
    print("\n=== Comparison ===")
    # Find first difference
    for i in range(min(len(cp_response), len(chronik_response))):
        if cp_response[i] != chronik_response[i]:
            print(f"First difference at byte {i}:")
            print(f"  CP Kafka: 0x{cp_response[i]:02x}")
            print(f"  Chronik:  0x{chronik_response[i]:02x}")
            break
    else:
        if len(cp_response) == len(chronik_response):
            print("Responses are IDENTICAL!")
        else:
            print(f"Length mismatch: CP={len(cp_response)}, Chronik={len(chronik_response)}")