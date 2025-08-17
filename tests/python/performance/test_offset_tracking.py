#!/usr/bin/env python3
"""Test offset tracking and persistence"""

import struct
import socket
import time
import subprocess

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

def create_simple_batch(messages, base_offset=0):
    """Create a simple message batch"""
    # For simplicity, use magic v0 messages
    result = b''
    
    for i, (key, value) in enumerate(messages):
        msg = b''
        
        # CRC (placeholder)
        msg += struct.pack('>i', 0)
        
        # Magic byte v0
        msg += struct.pack('>b', 0)
        
        # Attributes
        msg += struct.pack('>b', 0)
        
        # Key
        if key:
            key_bytes = key.encode('utf-8')
            msg += struct.pack('>i', len(key_bytes))
            msg += key_bytes
        else:
            msg += struct.pack('>i', -1)
        
        # Value
        if value:
            value_bytes = value.encode('utf-8')
            msg += struct.pack('>i', len(value_bytes))
            msg += value_bytes
        else:
            msg += struct.pack('>i', -1)
        
        # Message set entry
        entry = b''
        entry += struct.pack('>q', base_offset + i)  # offset
        entry += struct.pack('>i', len(msg))  # message size
        entry += msg
        
        result += entry
    
    return result

def send_produce(host, port, topic, partition, messages):
    """Send produce request and return base offset"""
    
    # Create messages
    messages_data = create_simple_batch(messages)
    
    # Build produce request body
    body = b''
    body += struct.pack('>h', 1)  # acks
    body += struct.pack('>i', 30000)  # timeout
    body += struct.pack('>i', 1)  # topic count
    
    # Topic
    body += encode_string(topic)
    body += struct.pack('>i', 1)  # partition count
    
    # Partition
    body += struct.pack('>i', partition)  # partition index
    body += encode_bytes(messages_data)  # message set
    
    # Request header
    header = b''
    header += struct.pack('>h', 0)  # API key (Produce)  
    header += struct.pack('>h', 0)  # API version 0
    header += struct.pack('>i', 123)  # Correlation ID
    header += encode_string('offset-test')  # Client ID
    
    # Full request
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    # Send request
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    # Read response
    sock.settimeout(5.0)
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    sock.close()
    
    # Parse response to get offset
    # Skip correlation ID, topic count, topic name
    pos = 4
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2 + topic_name_len
        
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        for _ in range(partition_count):
            partition = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            base_offset = struct.unpack('>q', response_data[pos:pos+8])[0]
            pos += 8
            
            if error_code == 0:
                return base_offset
            else:
                print(f"Error code: {error_code}")
                return -1
    
    return -1

# Test sequence
print("=== Testing Offset Tracking ===\n")

print("1. Create topic first")
subprocess.run(['python', 'test_create_topic.py'], capture_output=True)
time.sleep(1)

print("\n2. Send first batch of messages")
offset1 = send_produce('localhost', 9092, 'test-topic', 0, [
    ('key1', 'First message'),
    ('key2', 'Second message'),
    ('key3', 'Third message')
])
print(f"First batch base offset: {offset1}")
if offset1 == 0:
    print("✓ First batch started at offset 0")
else:
    print(f"✗ Expected offset 0, got {offset1}")

time.sleep(1)

print("\n3. Send second batch")
offset2 = send_produce('localhost', 9092, 'test-topic', 0, [
    ('key4', 'Fourth message'),
    ('key5', 'Fifth message')
])
print(f"Second batch base offset: {offset2}")
if offset2 == 3:
    print("✓ Second batch started at correct offset")
else:
    print(f"✗ Expected offset 3, got {offset2}")

print("\n4. Restart to test offset persistence")
print("Restarting containers...")
subprocess.run(['docker-compose', 'restart', 'ingest'], capture_output=True)
print("Waiting for service to come up...")
time.sleep(10)

print("\n5. Send third batch after restart")
offset3 = send_produce('localhost', 9092, 'test-topic', 0, [
    ('key6', 'Sixth message'),
    ('key7', 'Seventh message')
])
print(f"Third batch base offset: {offset3}")
if offset3 == 5:
    print("✓ Offset persisted across restart!")
else:
    print(f"✗ Expected offset 5, got {offset3}")

print("\n6. Check TiKV for stored offset")
result = subprocess.run([
    'docker', 'exec', 'chronik-stream-tikv-1', 
    'tikv-ctl', '--host', 'tikv:20160', 
    'scan', '--from', 'partition:', '--limit', '10'
], capture_output=True, text=True)

if 'partition:test-topic:0:offset' in result.stdout:
    print("✓ Offset found in TiKV")
else:
    print("✗ Offset not found in TiKV")
    print(f"TiKV scan output: {result.stdout}")

print("\n=== Test Complete ===")