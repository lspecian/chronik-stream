#!/usr/bin/env python3
import socket
import struct

def test_chronik():
    # Connect to Chronik
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(5)
    sock.connect(('localhost', 9092))

    # Build ApiVersions v3 request (exactly like librdkafka would)
    request = bytearray()
    request.extend(struct.pack('>h', 18))  # ApiVersions
    request.extend(struct.pack('>h', 3))   # v3
    request.extend(struct.pack('>i', 1))   # correlation_id (like librdkafka uses)
    
    # Client ID as compact string
    client_id = b'rdkafka'
    request.append(len(client_id) + 1)  # Compact string length
    request.extend(client_id)
    
    request.append(0)  # Tagged fields

    full_request = struct.pack('>i', len(request)) + bytes(request)
    print(f"Sending request: {len(full_request)} bytes")
    sock.send(full_request)

    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print(f"ERROR: Only got {len(size_bytes)} bytes for size header")
        return
        
    response_size = struct.unpack('>i', size_bytes)[0]
    print(f'Response size in header: {response_size} bytes')

    response = sock.recv(response_size)
    while len(response) < response_size:
        more = sock.recv(response_size - len(response))
        if not more:
            break
        response += more
    sock.close()

    print(f'Actual response received: {len(response)} bytes')
    
    # Parse response
    correlation_id = struct.unpack('>i', response[0:4])[0]
    print(f'Correlation ID: {correlation_id}')
    
    # Body starts at byte 4
    body = response[4:]
    print(f'Body size: {len(body)} bytes')
    
    # Parse body
    error_code = struct.unpack('>h', body[0:2])[0]
    print(f'Error code: {error_code}')
    
    # Compact array of APIs
    api_count_varint = body[2]
    if api_count_varint == 0:
        print("NULL array")
        return
    api_count = api_count_varint - 1
    print(f'API count: {api_count}')
    
    # Parse first few APIs
    offset = 3
    for i in range(min(5, api_count)):
        api_key = struct.unpack('>h', body[offset:offset+2])[0]
        min_ver = struct.unpack('>h', body[offset+2:offset+4])[0]
        max_ver = struct.unpack('>h', body[offset+4:offset+6])[0]
        # Skip tagged field (should be 0)
        tagged = body[offset+6]
        print(f'  API {api_key}: v{min_ver}-{max_ver} (tagged={tagged})')
        offset += 7
    
    print(f"\nTotal expected body size: 2 (error) + 1 (array len) + {api_count}*7 (APIs) = {2 + 1 + api_count*7}")
    print(f"Actual body size: {len(body)}")
    
    if len(body) == 2 + 1 + api_count*7:
        print("✅ Body size is CORRECT!")
    else:
        print(f"❌ Body size mismatch! Expected {2 + 1 + api_count*7}, got {len(body)}")

if __name__ == "__main__":
    test_chronik()