#!/usr/bin/env python3
"""Test raw Produce v9 request to verify server response"""

import socket
import struct
import time

def send_produce_v9():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build Produce v9 request
    request = bytearray()
    
    # Request header
    request.extend((0).to_bytes(2, 'big'))     # API key = 0 (Produce)
    request.extend((9).to_bytes(2, 'big'))     # API version = 9
    request.extend((123).to_bytes(4, 'big'))   # Correlation ID = 123
    request.extend(b'\x10')                     # Client ID length (compact)
    request.extend(b'test-python-cli')         # Client ID
    request.extend(b'\x00')                     # Tagged fields (empty)
    
    # Produce request body
    request.extend(b'\x00')                     # Transactional ID (null)
    request.extend((1).to_bytes(2, 'big'))     # ACKs = 1
    request.extend((5000).to_bytes(4, 'big'))  # Timeout = 5000ms
    request.extend(b'\x02')                     # Topics array length = 1
    request.extend(b'\x0b')                     # Topic name length = 10
    request.extend(b'test-topic')              # Topic name
    request.extend(b'\x02')                     # Partitions array length = 1
    request.extend((1).to_bytes(4, 'big'))     # Partition index = 1
    
    # Records (null for now to simplify)
    request.extend(b'\xff\xff\xff\xff')        # Records = null
    request.extend(b'\x00')                     # Partition tagged fields
    request.extend(b'\x00')                     # Topic tagged fields
    request.extend(b'\x00')                     # Request tagged fields
    
    # Add size prefix
    size = len(request)
    full_request = size.to_bytes(4, 'big') + request
    
    print(f"Sending Produce v9 request: {len(full_request)} bytes total")
    print(f"  Size field: {size}")
    print(f"  Request body: {len(request)} bytes")
    
    sock.send(full_request)
    
    # Read response
    print("\nWaiting for response...")
    
    # Read size field
    size_bytes = sock.recv(4)
    if len(size_bytes) != 4:
        print(f"ERROR: Expected 4 bytes for size, got {len(size_bytes)}")
        return
    
    response_size = int.from_bytes(size_bytes, 'big')
    print(f"Response size field: {response_size} bytes")
    
    # Read response body
    response = bytearray()
    while len(response) < response_size:
        chunk = sock.recv(response_size - len(response))
        if not chunk:
            break
        response.extend(chunk)
    
    print(f"Received response body: {len(response)} bytes")
    
    if len(response) < 4:
        print(f"ERROR: Response too short: {response.hex()}")
        return
        
    # Parse response
    pos = 0
    
    # Correlation ID
    correlation_id = int.from_bytes(response[pos:pos+4], 'big')
    pos += 4
    print(f"\nResponse header:")
    print(f"  Correlation ID: {correlation_id} (expected: 123)")
    
    # Response body
    print(f"\nResponse body (from byte {pos}):")
    print(f"  Remaining bytes: {len(response) - pos}")
    
    # throttle_time_ms
    if pos + 4 <= len(response):
        throttle_time = int.from_bytes(response[pos:pos+4], 'big')
        pos += 4
        print(f"  throttle_time_ms: {throttle_time}")
    
    # Topics array
    if pos < len(response):
        topics_len = response[pos] - 1  # Compact array
        pos += 1
        print(f"  Topics array length: {topics_len}")
        
        for i in range(topics_len):
            if pos >= len(response):
                break
                
            # Topic name
            topic_name_len = response[pos] - 1  # Compact string
            pos += 1
            topic_name = response[pos:pos+topic_name_len].decode('utf-8')
            pos += topic_name_len
            print(f"    Topic: '{topic_name}'")
            
            # Partitions array
            if pos < len(response):
                partitions_len = response[pos] - 1  # Compact array
                pos += 1
                print(f"      Partitions: {partitions_len}")
                
                for j in range(partitions_len):
                    if pos + 4 <= len(response):
                        partition_index = int.from_bytes(response[pos:pos+4], 'big')
                        pos += 4
                        print(f"        Partition {partition_index}:")
                    
                    if pos + 2 <= len(response):
                        error_code = int.from_bytes(response[pos:pos+2], 'big', signed=True)
                        pos += 2
                        print(f"          error_code: {error_code}")
                    
                    if pos + 8 <= len(response):
                        base_offset = int.from_bytes(response[pos:pos+8], 'big', signed=True)
                        pos += 8
                        print(f"          base_offset: {base_offset}")
                    
                    if pos + 8 <= len(response):
                        log_append_time = int.from_bytes(response[pos:pos+8], 'big', signed=True)
                        pos += 8
                        print(f"          log_append_time: {log_append_time}")
                    
                    if pos + 8 <= len(response):
                        log_start_offset = int.from_bytes(response[pos:pos+8], 'big', signed=True)
                        pos += 8
                        print(f"          log_start_offset: {log_start_offset}")
                    
                    # v8+ fields
                    if pos < len(response):
                        record_errors_len = response[pos]
                        pos += 1
                        print(f"          record_errors length: {record_errors_len - 1 if record_errors_len > 0 else 0}")
                    
                    if pos < len(response):
                        error_message = response[pos]
                        pos += 1
                        print(f"          error_message: {'null' if error_message == 0 else 'present'}")
                    
                    # Tagged fields
                    if pos < len(response):
                        partition_tagged = response[pos]
                        pos += 1
                        print(f"          partition tagged fields: {partition_tagged}")
            
            # Topic tagged fields
            if pos < len(response):
                topic_tagged = response[pos]
                pos += 1
                print(f"      topic tagged fields: {topic_tagged}")
    
    # Response tagged fields
    if pos < len(response):
        response_tagged = response[pos]
        pos += 1
        print(f"  response tagged fields: {response_tagged}")
    
    print(f"\nTotal parsed: {pos} bytes out of {len(response)} response bytes")
    print(f"Full response received: {4 + len(response)} bytes (size field + response)")
    
    # Hex dump for debugging
    print(f"\nResponse hex dump:")
    print(f"  Size field: {size_bytes.hex()}")
    print(f"  First 32 bytes: {response[:32].hex()}")
    if len(response) > 32:
        print(f"  Last 20 bytes: {response[-20:].hex()}")
    
    sock.close()

if __name__ == "__main__":
    send_produce_v9()