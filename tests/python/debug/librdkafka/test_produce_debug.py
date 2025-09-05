#!/usr/bin/env python3
"""Test Produce v9 request to trigger transactional_id parsing issue"""

import socket
import struct

def test_produce_request():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build Produce v9 request
    request = bytearray()
    request.extend((0).to_bytes(4, 'big'))  # Size placeholder
    request.extend((0).to_bytes(2, 'big'))  # API key = 0 (Produce)
    request.extend((9).to_bytes(2, 'big'))  # API version = 9
    request.extend((99).to_bytes(4, 'big')) # Correlation ID = 99
    request.extend(b'\x00\x10test-python-cli')  # Client ID (length=16, string=16 bytes)
    request.extend(b'\x00')  # Tagged fields for header (empty)
    
    # Produce request body (v9)
    request.extend(b'\x01\x00')  # transactional_id as compact string: length=0 (null)
    request.extend((1).to_bytes(2, 'big'))  # acks = 1
    request.extend((30000).to_bytes(4, 'big'))  # timeout_ms = 30000
    
    # Topic array (flexible)
    request.extend(b'\x02')  # topic array length = 1 (varint encoded)
    request.extend(b'\x0btest-topic')  # topic name as compact string (length=10, data=test-topic)
    
    # Partition array (flexible) 
    request.extend(b'\x02')  # partition array length = 1 (varint encoded)
    request.extend((0).to_bytes(4, 'big'))  # partition = 0
    
    # Record batch - minimal
    record_batch = bytearray()
    record_batch.extend((0).to_bytes(8, 'big'))  # base_offset = 0
    record_batch.extend((37).to_bytes(4, 'big'))  # batch_length = 37
    record_batch.extend((0).to_bytes(4, 'big'))   # partition_leader_epoch = 0 
    record_batch.extend(b'\x02')  # magic = 2
    record_batch.extend((0).to_bytes(4, 'big'))   # crc = 0 (placeholder)
    record_batch.extend((0).to_bytes(2, 'big'))   # attributes = 0
    record_batch.extend((-1).to_bytes(4, 'big', signed=True))  # last_offset_delta = -1
    record_batch.extend((0).to_bytes(8, 'big'))   # base_timestamp = 0
    record_batch.extend((0).to_bytes(8, 'big'))   # max_timestamp = 0
    record_batch.extend((-1).to_bytes(8, 'big', signed=True))  # producer_id = -1
    record_batch.extend((0).to_bytes(2, 'big'))   # producer_epoch = 0
    record_batch.extend((-1).to_bytes(4, 'big', signed=True))  # base_sequence = -1
    record_batch.extend((-1).to_bytes(4, 'big', signed=True))  # records_count = -1
    
    # Single record
    record_batch.extend(b'\x00')  # length = 0 (varint)
    record_batch.extend(b'\x00')  # attributes = 0
    record_batch.extend(b'\x00')  # timestamp_delta = 0 (varint)
    record_batch.extend(b'\x00')  # offset_delta = 0 (varint)
    record_batch.extend(b'\x02')  # key_length = 1 (varint)
    record_batch.extend(b'k')     # key = "k"
    record_batch.extend(b'\x02')  # value_length = 1 (varint) 
    record_batch.extend(b'v')     # value = "v"
    record_batch.extend(b'\x00')  # headers_count = 0 (varint)
    
    # Add record batch length to request
    request.extend(len(record_batch).to_bytes(4, 'big'))
    request.extend(record_batch)
    
    # End topic/partition arrays with tagged fields
    request.extend(b'\x00')  # partition tagged fields 
    request.extend(b'\x00')  # topic tagged fields
    request.extend(b'\x00')  # produce request tagged fields
    
    # Update size
    size = len(request) - 4
    request[0:4] = size.to_bytes(4, 'big')
    
    print(f"Sending Produce v9 request ({len(request)} bytes)")
    print(f"First 50 bytes: {request[:50].hex()}")
    
    sock.send(request)
    
    # Read response
    try:
        response_size_bytes = sock.recv(4)
        if len(response_size_bytes) == 0:
            print("No response received")
            return
            
        response_size = int.from_bytes(response_size_bytes, 'big')
        response = sock.recv(response_size)
        
        print(f"Response size field: {response_size}")
        print(f"Actual response bytes: {len(response)}")
        print(f"Response hex: {response.hex()}")
        
        if len(response) >= 4:
            correlation_id = int.from_bytes(response[0:4], 'big')
            print(f"Correlation ID: {correlation_id}")
        
    except Exception as e:
        print(f"Error reading response: {e}")
    
    sock.close()

if __name__ == "__main__":
    test_produce_request()