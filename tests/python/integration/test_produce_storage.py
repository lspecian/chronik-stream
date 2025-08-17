#!/usr/bin/env python3
"""Test that Produce API actually stores messages"""

import struct
import socket
import time

def encode_string(s):
    """Encode a string as Kafka protocol string (length + data)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def encode_bytes(b):
    """Encode bytes as Kafka protocol bytes (length + data)"""
    if b is None:
        return struct.pack('>i', -1)
    return struct.pack('>i', len(b)) + b

def create_record_batch(key, value, offset=0):
    """Create a simple record batch v2"""
    # Record attributes
    attributes = 0  # No compression
    timestamp_delta = 0
    offset_delta = 0
    
    # Encode key and value
    key_bytes = key.encode('utf-8') if key else b''
    value_bytes = value.encode('utf-8') if value else b''
    
    # Record: attributes + timestamp_delta + offset_delta + key_len + key + value_len + value + headers_count
    record = b''
    record += struct.pack('>b', attributes)  # attributes
    record += struct.pack('>i', timestamp_delta)  # timestamp delta (varint)
    record += struct.pack('>i', offset_delta)  # offset delta (varint) 
    record += struct.pack('>i', len(key_bytes))  # key length (varint)
    record += key_bytes  # key
    record += struct.pack('>i', len(value_bytes))  # value length (varint)
    record += value_bytes  # value
    record += struct.pack('>i', 0)  # headers count (varint)
    
    # Record batch header
    batch = b''
    batch += struct.pack('>q', offset)  # base offset
    batch += struct.pack('>i', len(record))  # batch length
    batch += struct.pack('>i', 0)  # partition leader epoch
    batch += struct.pack('>b', 2)  # magic (v2)
    batch += struct.pack('>i', 0)  # crc (placeholder)
    batch += struct.pack('>h', attributes)  # attributes
    batch += struct.pack('>i', 0)  # last offset delta
    batch += struct.pack('>q', int(time.time() * 1000))  # base timestamp
    batch += struct.pack('>q', int(time.time() * 1000))  # max timestamp
    batch += struct.pack('>q', -1)  # producer id
    batch += struct.pack('>h', -1)  # producer epoch
    batch += struct.pack('>i', -1)  # base sequence
    batch += struct.pack('>i', 1)  # records count
    batch += record
    
    return batch

def send_produce_request(host, port, topic, partition, records):
    """Send a Produce request and return the response"""
    
    # Create record batch
    batch_data = create_record_batch(records[0][0], records[0][1])
    
    # Build produce request body
    body = b''
    body += encode_string(None)  # transactional_id
    body += struct.pack('>h', 1)  # acks
    body += struct.pack('>i', 30000)  # timeout_ms
    body += struct.pack('>i', 1)  # topic_count
    
    # Topic data
    body += encode_string(topic)
    body += struct.pack('>i', 1)  # partition_count
    
    # Partition data
    body += struct.pack('>i', partition)  # partition
    body += encode_bytes(batch_data)  # records
    
    # Request header
    header = b''
    header += struct.pack('>h', 0)  # API key (Produce)
    header += struct.pack('>h', 7)  # API version
    header += struct.pack('>i', 123)  # Correlation ID
    header += encode_string('test-producer')  # Client ID
    
    # Full request
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    # Send request
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    # Read response
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    
    # Parse response header
    correlation_id = struct.unpack('>i', response_data[:4])[0]
    
    # Parse response body
    pos = 4
    throttle_time_ms = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    results = []
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        topic_name = response_data[pos:pos+topic_name_len].decode('utf-8')
        pos += topic_name_len
        
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        for _ in range(partition_count):
            partition = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            base_offset = struct.unpack('>q', response_data[pos:pos+8])[0]
            pos += 8
            log_append_time = struct.unpack('>q', response_data[pos:pos+8])[0]
            pos += 8
            log_start_offset = struct.unpack('>q', response_data[pos:pos+8])[0]
            pos += 8
            
            results.append({
                'topic': topic_name,
                'partition': partition,
                'error_code': error_code,
                'base_offset': base_offset,
                'log_append_time': log_append_time,
                'log_start_offset': log_start_offset
            })
    
    sock.close()
    return correlation_id, results

def send_fetch_request(host, port, topic, partition, offset):
    """Send a Fetch request and return the response"""
    
    # Build fetch request body
    body = b''
    body += struct.pack('>i', -1)  # replica_id
    body += struct.pack('>i', 1000)  # max_wait_ms
    body += struct.pack('>i', 1)  # min_bytes
    body += struct.pack('>i', 1048576)  # max_bytes
    body += struct.pack('>b', 0)  # isolation_level
    body += struct.pack('>i', 0)  # session_id
    body += struct.pack('>i', -1)  # session_epoch
    body += struct.pack('>i', 1)  # topic_count
    
    # Topic data
    body += encode_string(topic)
    body += struct.pack('>i', 1)  # partition_count
    
    # Partition data
    body += struct.pack('>i', partition)  # partition
    body += struct.pack('>q', offset)  # fetch_offset
    body += struct.pack('>q', -1)  # log_start_offset
    body += struct.pack('>i', 1048576)  # partition_max_bytes
    
    # Request header
    header = b''
    header += struct.pack('>h', 1)  # API key (Fetch)
    header += struct.pack('>h', 11)  # API version
    header += struct.pack('>i', 456)  # Correlation ID
    header += encode_string('test-consumer')  # Client ID
    
    # Full request
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    # Send request
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    # Read response with timeout
    sock.settimeout(5.0)
    try:
        response_length_data = sock.recv(4)
        response_length = struct.unpack('>i', response_length_data)[0]
        response_data = sock.recv(response_length)
        
        # Parse response to check if we got any records
        correlation_id = struct.unpack('>i', response_data[:4])[0]
        # For now, just check if we got a response
        has_data = len(response_data) > 100  # Rough check
        
        sock.close()
        return correlation_id, has_data
    except socket.timeout:
        sock.close()
        return 456, False

# Test produce and fetch
print("Test 1: Produce a message")
corr_id, results = send_produce_request('localhost', 9092, 'test-topic', 0, [('key1', 'value1')])
print(f"  Correlation ID: {corr_id}")
for result in results:
    print(f"  Topic: {result['topic']}, Partition: {result['partition']}")
    print(f"  Error Code: {result['error_code']}")
    print(f"  Base Offset: {result['base_offset']}")
    if result['error_code'] == 0:
        print("  ✓ Message produced successfully")
    else:
        print("  ✗ Error producing message")

print("\nTest 2: Try to fetch the message")
time.sleep(1)  # Give it time to persist
corr_id, has_data = send_fetch_request('localhost', 9092, 'test-topic', 0, 0)
print(f"  Correlation ID: {corr_id}")
if has_data:
    print("  ✓ Fetch returned data (messages are being stored)")
else:
    print("  ✗ Fetch returned no data (messages may not be persisted)")

print("\nTest 3: Check storage directory")
import os
storage_path = '/home/chronik/data'  # This is inside the container
# Since we can't directly access container filesystem, we'll check via docker
import subprocess
result = subprocess.run(['docker', 'exec', 'chronik-stream-ingest-1', 'ls', '-la', '/home/chronik/data'], 
                       capture_output=True, text=True)
if result.returncode == 0:
    print(f"  Storage directory contents:\n{result.stdout}")
    if 'segments' in result.stdout or '.segment' in result.stdout:
        print("  ✓ Segment files found in storage")
    else:
        print("  ✗ No segment files found in storage")
else:
    print(f"  Could not check storage: {result.stderr}")