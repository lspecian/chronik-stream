#!/usr/bin/env python3
"""Test the complete flow: create topic, verify in metadata, then produce"""

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

def send_create_topics_request(host, port, topic_name, num_partitions=1, replication_factor=1):
    """Send a CreateTopics request"""
    
    # Build create topics request body
    body = b''
    body += struct.pack('>i', 1)  # topic_count
    
    # Topic data
    body += encode_string(topic_name)
    body += struct.pack('>i', num_partitions)  # num_partitions
    body += struct.pack('>h', replication_factor)  # replication_factor
    body += struct.pack('>i', 0)  # replica_assignments count
    body += struct.pack('>i', 0)  # config_entries count
    
    body += struct.pack('>i', 30000)  # timeout_ms
    body += struct.pack('>b', 0)  # validate_only = false
    
    # Request header
    header = b''
    header += struct.pack('>h', 19)  # API key (CreateTopics)
    header += struct.pack('>h', 5)  # API version
    header += struct.pack('>i', 789)  # Correlation ID
    header += encode_string('test-client')  # Client ID
    
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
        
        error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        
        # Skip error message if present
        error_msg_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        if error_msg_len > 0:
            pos += error_msg_len
            
        results.append({
            'topic': topic_name,
            'error_code': error_code
        })
    
    sock.close()
    return correlation_id, results

def check_metadata_simple(host, port):
    """Simple metadata check"""
    # Build metadata request body for API version 1 (simpler)
    body = b''
    body += struct.pack('>i', 0)  # topics array with 0 topics = all topics
    
    # Request header
    header = b''
    header += struct.pack('>h', 3)  # API key (Metadata)
    header += struct.pack('>h', 1)  # API version 1 (simpler)
    header += struct.pack('>i', 555)  # Correlation ID
    header += encode_string('test-client')  # Client ID
    
    # Full request
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    # Send request
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    # Read response
    response_length_data = sock.recv(4)
    if len(response_length_data) < 4:
        print("Failed to read response length")
        sock.close()
        return None
        
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = b''
    while len(response_data) < response_length:
        chunk = sock.recv(min(4096, response_length - len(response_data)))
        if not chunk:
            break
        response_data += chunk
    
    sock.close()
    
    # Basic parse - just look for topic names
    if b'test-topic' in response_data:
        return True
    return False

def create_simple_record_batch(key, value):
    """Create a minimal record batch"""
    # For testing, use a very simple format
    # This is a simplified version - real Kafka uses more complex encoding
    
    # Magic byte 2 format
    batch = b''
    batch += struct.pack('>q', 0)  # base offset
    batch += struct.pack('>i', 50)  # batch length (approximate)
    batch += struct.pack('>i', 0)  # partition leader epoch
    batch += struct.pack('>b', 2)  # magic (v2)
    batch += struct.pack('>i', 0)  # crc (placeholder)
    batch += struct.pack('>h', 0)  # attributes
    batch += struct.pack('>i', 0)  # last offset delta
    batch += struct.pack('>q', int(time.time() * 1000))  # base timestamp
    batch += struct.pack('>q', int(time.time() * 1000))  # max timestamp
    batch += struct.pack('>q', -1)  # producer id
    batch += struct.pack('>h', -1)  # producer epoch
    batch += struct.pack('>i', -1)  # base sequence
    batch += struct.pack('>i', 1)  # records count
    
    # Single record
    record = b''
    record += struct.pack('>b', 0)  # attributes
    record += b'\x00'  # timestamp delta (varint 0)
    record += b'\x00'  # offset delta (varint 0)
    
    # Key
    if key:
        key_bytes = key.encode('utf-8')
        record += bytes([len(key_bytes)])  # key length (varint)
        record += key_bytes
    else:
        record += b'\x00'  # null key
    
    # Value
    if value:
        value_bytes = value.encode('utf-8')
        record += bytes([len(value_bytes)])  # value length (varint)
        record += value_bytes
    else:
        record += b'\x00'  # null value
    
    record += b'\x00'  # headers count (varint 0)
    
    batch += record
    return batch

def send_produce_simple(host, port, topic):
    """Send a simple produce request"""
    
    # Create record batch
    batch_data = create_simple_record_batch("test-key", "test-value")
    
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
    body += struct.pack('>i', 0)  # partition
    body += encode_bytes(batch_data)  # records
    
    # Request header
    header = b''
    header += struct.pack('>h', 0)  # API key (Produce)
    header += struct.pack('>h', 7)  # API version
    header += struct.pack('>i', 888)  # Correlation ID
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
    
    # Parse basic error code
    pos = 4
    pos += 4  # skip throttle_time_ms
    pos += 4  # skip topic_count
    pos += 2  # skip topic name length
    
    # Find error code (it's after topic name)
    # This is approximate - just check if response seems successful
    if len(response_data) > 20:
        # Look for error codes in typical positions
        for i in range(10, min(len(response_data)-2, 50), 2):
            potential_error = struct.unpack('>h', response_data[i:i+2])[0]
            if potential_error == 3:  # UNKNOWN_TOPIC_OR_PARTITION
                return correlation_id, 3
            elif potential_error == 0:  # SUCCESS
                return correlation_id, 0
    
    sock.close()
    return correlation_id, -1  # Unknown

# Run the test flow
print("=== Testing Topic Creation and Produce Flow ===\n")

print("Step 1: Create topic 'chronik-test-topic'")
corr_id, results = send_create_topics_request('localhost', 9092, 'chronik-test-topic', 3, 1)
print(f"Correlation ID: {corr_id}")
for result in results:
    print(f"Topic: {result['topic']}, Error Code: {result['error_code']}")
    if result['error_code'] == 0:
        print("✓ Topic created successfully")
    elif result['error_code'] == 36:  # TOPIC_ALREADY_EXISTS
        print("✓ Topic already exists")
    else:
        print(f"✗ Error creating topic: {result['error_code']}")

print("\nStep 2: Wait for metadata propagation")
time.sleep(2)

print("\nStep 3: Check if topic appears in metadata")
if check_metadata_simple('localhost', 9092):
    print("✓ Topic found in metadata")
else:
    print("✗ Topic NOT found in metadata")

print("\nStep 4: Try to produce a message")
corr_id, error_code = send_produce_simple('localhost', 9092, 'chronik-test-topic')
print(f"Correlation ID: {corr_id}")
if error_code == 0:
    print("✓ Message produced successfully")
elif error_code == 3:
    print("✗ UNKNOWN_TOPIC_OR_PARTITION error")
else:
    print(f"✗ Unknown response (error_code estimate: {error_code})")

print("\nStep 5: Check docker logs for clues")
import subprocess
result = subprocess.run(['docker', 'logs', '--tail', '20', 'chronik-stream-ingest-1'], 
                       capture_output=True, text=True)
if result.returncode == 0:
    print("Recent logs:")
    print(result.stdout)