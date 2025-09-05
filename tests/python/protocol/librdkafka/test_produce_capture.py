#!/usr/bin/env python3
"""Capture exact server response to debug librdkafka issue"""

import socket
import struct

def capture_produce_response():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build minimal Produce v9 request
    request = bytearray()
    
    # Request header
    request.extend((0).to_bytes(2, 'big'))     # API key = 0 (Produce)
    request.extend((9).to_bytes(2, 'big'))     # API version = 9
    request.extend((456).to_bytes(4, 'big'))   # Correlation ID = 456
    request.extend(b'\x10')                     # Client ID length (compact) = 15 + 1
    request.extend(b'test-python-cli')         # Client ID (15 bytes)
    request.extend(b'\x00')                     # Tagged fields (empty)
    
    # Produce request body (minimal)
    request.extend(b'\x00')                     # Transactional ID (null)
    request.extend((1).to_bytes(2, 'big'))     # ACKs = 1
    request.extend((5000).to_bytes(4, 'big'))  # Timeout = 5000ms
    request.extend(b'\x02')                     # Topics array length = 1 (compact)
    request.extend(b'\x0b')                     # Topic name length = 10 (compact)
    request.extend(b'test-topic')              # Topic name
    request.extend(b'\x02')                     # Partitions array length = 1 (compact)
    request.extend((1).to_bytes(4, 'big'))     # Partition index = 1
    request.extend(b'\xff\xff\xff\xff')        # Records = null (-1 as INT32)
    request.extend(b'\x00')                     # Partition tagged fields
    request.extend(b'\x00')                     # Topic tagged fields
    request.extend(b'\x00')                     # Request tagged fields
    
    # Add size prefix
    size = len(request)
    full_request = size.to_bytes(4, 'big') + request
    
    print(f"Sending Produce v9 request: {size + 4} bytes total")
    sock.send(full_request)
    
    # Read response
    size_bytes = sock.recv(4)
    response_size = int.from_bytes(size_bytes, 'big')
    print(f"\nResponse size: {response_size} bytes")
    
    response = bytearray()
    while len(response) < response_size:
        chunk = sock.recv(response_size - len(response))
        if not chunk:
            break
        response.extend(chunk)
    
    print(f"Received: {len(response)} bytes")
    print(f"\nFull response hex ({len(response)} bytes):")
    print(response.hex())
    
    # Parse and annotate each byte
    print("\n=== Detailed byte analysis ===")
    pos = 0
    
    # Correlation ID (4 bytes)
    print(f"Bytes {pos:2d}-{pos+3:2d}: {response[pos:pos+4].hex()} = Correlation ID: {int.from_bytes(response[pos:pos+4], 'big')}")
    pos += 4
    
    # Throttle time (4 bytes)
    print(f"Bytes {pos:2d}-{pos+3:2d}: {response[pos:pos+4].hex()} = Throttle time: {int.from_bytes(response[pos:pos+4], 'big', signed=True)}")
    pos += 4
    
    # Topics array length (compact varint)
    print(f"Byte  {pos:2d}:    {response[pos:pos+1].hex()}       = Topics array length (compact): {response[pos]} -> {response[pos]-1} topics")
    pos += 1
    
    # Topic name length (compact)
    print(f"Byte  {pos:2d}:    {response[pos:pos+1].hex()}       = Topic name length (compact): {response[pos]} -> {response[pos]-1} bytes")
    topic_len = response[pos] - 1
    pos += 1
    
    # Topic name
    print(f"Bytes {pos:2d}-{pos+topic_len-1:2d}: {response[pos:pos+topic_len].hex()} = Topic name: '{response[pos:pos+topic_len].decode()}'")
    pos += topic_len
    
    # Partitions array length (compact)
    print(f"Byte  {pos:2d}:    {response[pos:pos+1].hex()}       = Partitions array length (compact): {response[pos]} -> {response[pos]-1} partitions")
    pos += 1
    
    # Partition index (4 bytes)
    print(f"Bytes {pos:2d}-{pos+3:2d}: {response[pos:pos+4].hex()} = Partition index: {int.from_bytes(response[pos:pos+4], 'big')}")
    pos += 4
    
    # Error code (2 bytes)
    print(f"Bytes {pos:2d}-{pos+1:2d}: {response[pos:pos+2].hex()}     = Error code: {int.from_bytes(response[pos:pos+2], 'big', signed=True)}")
    pos += 2
    
    # Base offset (8 bytes)
    print(f"Bytes {pos:2d}-{pos+7:2d}: {response[pos:pos+8].hex()} = Base offset: {int.from_bytes(response[pos:pos+8], 'big', signed=True)}")
    pos += 8
    
    # Log append time (8 bytes)
    print(f"Bytes {pos:2d}-{pos+7:2d}: {response[pos:pos+8].hex()} = Log append time: {int.from_bytes(response[pos:pos+8], 'big', signed=True)}")
    pos += 8
    
    # Log start offset (8 bytes)
    print(f"Bytes {pos:2d}-{pos+7:2d}: {response[pos:pos+8].hex()} = Log start offset: {int.from_bytes(response[pos:pos+8], 'big', signed=True)}")
    pos += 8
    
    # v8+ fields
    print(f"\n--- v8+ fields (record_errors, error_message) ---")
    print(f"Byte  {pos:2d}:    {response[pos:pos+1].hex()}       = Record errors array (compact): {response[pos]} -> {response[pos]-1 if response[pos] > 0 else 'empty'}")
    pos += 1
    
    print(f"Byte  {pos:2d}:    {response[pos:pos+1].hex()}       = Error message (compact string): {response[pos]} -> {'null' if response[pos] == 0 else f'{response[pos]-1} bytes'}")
    pos += 1
    
    # Tagged fields
    print(f"\n--- Tagged fields ---")
    print(f"Byte  {pos:2d}:    {response[pos:pos+1].hex()}       = Partition tagged fields: {response[pos]}")
    pos += 1
    
    print(f"Byte  {pos:2d}:    {response[pos:pos+1].hex()}       = Topic tagged fields: {response[pos]}")
    pos += 1
    
    print(f"Byte  {pos:2d}:    {response[pos:pos+1].hex()}       = Response tagged fields: {response[pos]}")
    pos += 1
    
    print(f"\nTotal bytes parsed: {pos} out of {len(response)}")
    
    if pos != len(response):
        print(f"ERROR: {len(response) - pos} bytes unparsed!")
        print(f"Unparsed bytes: {response[pos:].hex()}")
    
    sock.close()

if __name__ == "__main__":
    capture_produce_response()