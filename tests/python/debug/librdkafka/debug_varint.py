#!/usr/bin/env python3
"""Debug the varint encoding issue."""

import socket
import struct

def test_server(host, port, name):
    """Test a server and debug the response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
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
    response_size = struct.unpack('>i', size_bytes)[0]
    response = sock.recv(response_size)
    while len(response) < response_size:
        response += sock.recv(response_size - len(response))
    sock.close()
    
    print(f"\n=== {name} ({host}:{port}) ===")
    print(f"Response size: {response_size} bytes")
    
    # Parse response
    pos = 0
    correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    print(f"Correlation ID: {correlation_id}")
    
    # Error code
    error_code = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    print(f"Error code: {error_code}")
    
    # The next byte should be the varint for array length
    print(f"\nBytes around varint position {pos}:")
    for i in range(max(0, pos-2), min(len(response), pos+10)):
        marker = " <-- VARINT" if i == pos else ""
        print(f"  [{i:3d}] 0x{response[i]:02x} ({response[i]:3d}){marker}")
    
    # Read varint
    varint_byte = response[pos]
    if varint_byte == 0:
        array_len = -1
        print(f"\nVARINT = 0x00 means NULL array!")
    else:
        array_len = varint_byte - 1
        print(f"\nVARINT = 0x{varint_byte:02x} ({varint_byte}) means array length = {array_len}")
    pos += 1
    
    # If we have APIs, show the first one
    if array_len > 0:
        api_key = struct.unpack('>h', response[pos:pos+2])[0]
        min_ver = struct.unpack('>h', response[pos+2:pos+4])[0]
        max_ver = struct.unpack('>h', response[pos+4:pos+6])[0]
        print(f"First API: key={api_key}, min={min_ver}, max={max_ver}")
    
    return response

# Test both servers
cp_response = test_server("localhost", 29092, "CP Kafka")
chronik_response = test_server("localhost", 9092, "Chronik")

print("\n=== ISSUE FOUND ===")
print("Both servers are sending 0x00 for the varint, which means NULL array!")
print("But they're sending 429 bytes of data - the APIs are there but the length is wrong!")
print("\nThe varint should be 0x3d (61 in decimal) for 60 APIs (60+1=61 in compact encoding)")