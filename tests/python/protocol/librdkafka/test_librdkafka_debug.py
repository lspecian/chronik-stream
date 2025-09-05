#!/usr/bin/env python3
"""
Deep analysis of what librdkafka expects vs what we send
"""
import socket
import struct

def hex_dump(data, prefix=""):
    """Pretty print hex dump"""
    for i in range(0, len(data), 16):
        hex_str = ' '.join(f'{b:02x}' for b in data[i:i+16])
        ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in data[i:i+16])
        print(f"{prefix}{i:04x}: {hex_str:<48}  {ascii_str}")

def test_api_versions_v3():
    print("=== Testing ApiVersions v3 Request/Response ===\n")
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(5)
    sock.connect(('localhost', 9092))

    # Build exactly what librdkafka sends
    request = bytearray()
    request.extend(struct.pack('>h', 18))  # ApiVersions
    request.extend(struct.pack('>h', 3))   # v3
    request.extend(struct.pack('>i', 1))   # correlation_id
    
    # Client ID as compact string (what librdkafka sends)
    client_id = b'rdkafka'
    request.append(len(client_id) + 1)  # Compact string length
    request.extend(client_id)
    
    request.append(0)  # Tagged fields

    full_request = struct.pack('>i', len(request)) + bytes(request)
    print(f"REQUEST ({len(full_request)} bytes):")
    hex_dump(full_request, "  ")
    
    sock.send(full_request)

    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print(f"ERROR: Only got {len(size_bytes)} bytes for size header")
        return
        
    response_size = struct.unpack('>i', size_bytes)[0]
    print(f"\nRESPONSE SIZE: {response_size} bytes")
    
    response = sock.recv(response_size)
    while len(response) < response_size:
        more = sock.recv(response_size - len(response))
        if not more:
            break
        response += more
    sock.close()

    print(f"\nFULL RESPONSE ({len(response)} bytes):")
    hex_dump(response, "  ")
    
    # Parse response
    print("\n=== Response Analysis ===")
    correlation_id = struct.unpack('>i', response[0:4])[0]
    print(f"Correlation ID: {correlation_id}")
    
    body = response[4:]
    print(f"Body size: {len(body)} bytes")
    
    # Parse body
    error_code = struct.unpack('>h', body[0:2])[0]
    print(f"Error code: {error_code}")
    
    # Compact array
    api_count_varint = body[2]
    api_count = api_count_varint - 1 if api_count_varint > 0 else -1
    print(f"API count varint: 0x{api_count_varint:02x} = {api_count} APIs")
    
    # Check if body matches expected size
    expected_body = 2 + 1 + (api_count * 7)  # error + varint + APIs
    print(f"\nExpected body size: {expected_body}")
    print(f"Actual body size: {len(body)}")
    
    if len(body) == expected_body:
        print("✅ SIZE MATCHES")
    else:
        print(f"❌ SIZE MISMATCH (diff: {len(body) - expected_body})")
    
    # Parse first few APIs to verify structure
    print("\nFirst 5 APIs:")
    offset = 3
    for i in range(min(5, api_count)):
        api_key = struct.unpack('>h', body[offset:offset+2])[0]
        min_ver = struct.unpack('>h', body[offset+2:offset+4])[0]
        max_ver = struct.unpack('>h', body[offset+4:offset+6])[0]
        tagged = body[offset+6]
        print(f"  API {api_key:3d}: v{min_ver:2d}-{max_ver:2d} (tagged=0x{tagged:02x})")
        offset += 7

def test_api_versions_v0():
    print("\n=== Testing ApiVersions v0 Request/Response ===\n")
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(5)
    sock.connect(('localhost', 9092))

    # Build ApiVersions v0 request
    request = bytearray()
    request.extend(struct.pack('>h', 18))  # ApiVersions
    request.extend(struct.pack('>h', 0))   # v0
    request.extend(struct.pack('>i', 2))   # correlation_id
    # v0 has no client_id field

    full_request = struct.pack('>i', len(request)) + bytes(request)
    print(f"REQUEST ({len(full_request)} bytes):")
    hex_dump(full_request, "  ")
    
    sock.send(full_request)

    # Read response
    size_bytes = sock.recv(4)
    response_size = struct.unpack('>i', size_bytes)[0]
    print(f"\nRESPONSE SIZE: {response_size} bytes")
    
    response = sock.recv(response_size)
    while len(response) < response_size:
        response += sock.recv(response_size - len(response))
    sock.close()

    print(f"\nFULL RESPONSE ({len(response)} bytes):")
    hex_dump(response, "  ")
    
    # Parse v0 response
    print("\n=== Response Analysis ===")
    correlation_id = struct.unpack('>i', response[0:4])[0]
    print(f"Correlation ID: {correlation_id}")
    
    body = response[4:]
    print(f"Body size: {len(body)} bytes")
    
    # v0 format: array first, then error code
    api_count = struct.unpack('>i', body[0:4])[0]
    print(f"API count: {api_count}")
    
    # Parse first few APIs
    print("\nFirst 5 APIs:")
    offset = 4
    for i in range(min(5, api_count)):
        api_key = struct.unpack('>h', body[offset:offset+2])[0]
        min_ver = struct.unpack('>h', body[offset+2:offset+4])[0]
        max_ver = struct.unpack('>h', body[offset+4:offset+6])[0]
        print(f"  API {api_key:3d}: v{min_ver:2d}-{max_ver:2d}")
        offset += 6
    
    # Error code at the end
    error_code = struct.unpack('>h', body[offset:offset+2])[0]
    print(f"\nError code at end: {error_code}")

if __name__ == "__main__":
    test_api_versions_v3()
    test_api_versions_v0()