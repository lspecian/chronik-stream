#!/usr/bin/env python3
"""Analyze our response byte by byte"""

import socket
import struct

def decode_compact_varint(data, pos):
    """Decode a compact varint (zigzag encoded)"""
    value = data[pos]
    pos += 1
    
    if value == 0:
        return 0, pos
    
    # Decode zigzag
    actual_value = value - 1
    return actual_value, pos

def analyze_response():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send ApiVersions v3 request matching librdkafka exactly
    request = bytearray()
    request.extend((0).to_bytes(4, 'big'))  # Size placeholder
    request.extend((18).to_bytes(2, 'big'))  # API key = 18
    request.extend((3).to_bytes(2, 'big'))   # API version = 3
    request.extend((1).to_bytes(4, 'big'))   # Correlation ID = 1
    request.extend(b'\x00\x0f')              # Client ID length = 15
    request.extend(b'test-librdkafka')       # Client ID
    # Client software name (compact string)
    request.append(19)  # length + 1 = 19
    request.extend(b'confluent-kafka-go')
    # Client software version (compact string)
    request.append(7)   # length + 1 = 7
    request.extend(b'2.11.1')
    request.append(0)   # Tagged fields = 0
    
    # Update size
    size = len(request) - 4
    request[0:4] = size.to_bytes(4, 'big')
    
    print(f"Sending request ({len(request)} bytes)")
    sock.send(request)
    
    # Read response
    response_size_bytes = sock.recv(4)
    response_size = int.from_bytes(response_size_bytes, 'big')
    response = sock.recv(response_size)
    
    print(f"\nResponse analysis:")
    print(f"Response size: {response_size} bytes")
    print(f"Actual received: {len(response)} bytes")
    
    pos = 0
    
    # Correlation ID
    correlation_id = int.from_bytes(response[pos:pos+4], 'big')
    print(f"\nPosition {pos}: Correlation ID = {correlation_id}")
    pos += 4
    
    # Error code
    error_code = int.from_bytes(response[pos:pos+2], 'big', signed=True)
    print(f"Position {pos}: Error code = {error_code}")
    pos += 2
    
    # API count (compact varint)
    api_count_raw = response[pos]
    api_count = api_count_raw - 1 if api_count_raw > 0 else -1
    print(f"Position {pos}: API count byte = 0x{api_count_raw:02x}, decoded = {api_count}")
    pos += 1
    
    # Parse first few APIs to understand structure
    for i in range(min(3, api_count)):
        print(f"\nAPI #{i}:")
        
        # API key (INT16)
        api_key = int.from_bytes(response[pos:pos+2], 'big')
        print(f"  Position {pos}: API key = {api_key}")
        pos += 2
        
        # Next we expect min/max versions
        # Check if INT8 or INT16
        byte1 = response[pos]
        byte2 = response[pos+1]
        
        # Try INT16 interpretation
        as_int16_min = int.from_bytes(response[pos:pos+2], 'big')
        as_int16_max = int.from_bytes(response[pos+2:pos+4], 'big')
        
        print(f"  Position {pos}: Next 4 bytes = {response[pos:pos+4].hex()}")
        print(f"    As INT16: min={as_int16_min}, max={as_int16_max}")
        print(f"    As INT8: min={byte1}, max={byte2}")
        
        # Assume INT16 for now
        pos += 4
        
        # Tagged fields (varint)
        tagged_fields = response[pos]
        print(f"  Position {pos}: Tagged fields = 0x{tagged_fields:02x}")
        pos += 1
    
    # Check what's at the end
    print(f"\nRemaining bytes from position {pos}: {len(response) - pos}")
    print(f"Last 10 bytes: {response[-10:].hex()}")
    
    # Check if there's throttle_time at the end
    if len(response) >= pos + 4:
        possible_throttle = int.from_bytes(response[-4:], 'big')
        print(f"Last 4 bytes as INT32: {possible_throttle}")
    
    sock.close()

if __name__ == "__main__":
    analyze_response()