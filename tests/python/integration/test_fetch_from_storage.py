#!/usr/bin/env python3
"""Test that fetch handler reads from storage after produce"""

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
    """Send produce request"""
    
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
    header += encode_string('fetch-test')  # Client ID
    
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
    
    print(f"Produce response received: {len(response_data)} bytes")

def send_fetch(host, port, topic, partition, fetch_offset, max_bytes=1024*1024):
    """Send fetch request and parse response"""
    
    # Build fetch request body
    body = b''
    body += struct.pack('>i', -1)  # replica_id (-1 for consumer)
    body += struct.pack('>i', 1000)  # max_wait_ms
    body += struct.pack('>i', 1)  # min_bytes
    body += struct.pack('>i', 1)  # topic count
    
    # Topic
    body += encode_string(topic)
    body += struct.pack('>i', 1)  # partition count
    
    # Partition
    body += struct.pack('>i', partition)  # partition index
    body += struct.pack('>q', fetch_offset)  # fetch_offset
    body += struct.pack('>i', max_bytes)  # partition_max_bytes
    
    # Request header
    header = b''
    header += struct.pack('>h', 1)  # API key (Fetch)
    header += struct.pack('>h', 0)  # API version 0
    header += struct.pack('>i', 456)  # Correlation ID
    header += encode_string('fetch-test')  # Client ID
    
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
    
    # Parse response
    pos = 4  # Skip correlation ID
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    print(f"Fetch response: {topic_count} topics")
    
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        topic_name = response_data[pos:pos+topic_name_len].decode('utf-8')
        pos += topic_name_len
        
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        print(f"  Topic: {topic_name}, {partition_count} partitions")
        
        for _ in range(partition_count):
            partition_id = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            high_watermark = struct.unpack('>q', response_data[pos:pos+8])[0]
            pos += 8
            message_set_size = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            
            print(f"    Partition {partition_id}: error={error_code}, high_watermark={high_watermark}, message_set_size={message_set_size}")
            
            if message_set_size > 0:
                message_set = response_data[pos:pos+message_set_size]
                pos += message_set_size
                
                # Parse messages
                msg_pos = 0
                msg_count = 0
                while msg_pos < len(message_set):
                    msg_offset = struct.unpack('>q', message_set[msg_pos:msg_pos+8])[0]
                    msg_pos += 8
                    msg_size = struct.unpack('>i', message_set[msg_pos:msg_pos+4])[0]
                    msg_pos += 4
                    
                    if msg_pos + msg_size > len(message_set):
                        break
                    
                    # Skip CRC, magic, attributes
                    msg_pos += 4 + 1 + 1
                    
                    # Key
                    key_size = struct.unpack('>i', message_set[msg_pos:msg_pos+4])[0]
                    msg_pos += 4
                    if key_size >= 0:
                        key = message_set[msg_pos:msg_pos+key_size].decode('utf-8')
                        msg_pos += key_size
                    else:
                        key = None
                    
                    # Value
                    value_size = struct.unpack('>i', message_set[msg_pos:msg_pos+4])[0]
                    msg_pos += 4
                    if value_size >= 0:
                        value = message_set[msg_pos:msg_pos+value_size].decode('utf-8')
                        msg_pos += value_size
                    else:
                        value = None
                    
                    print(f"      Message: offset={msg_offset}, key={key}, value={value}")
                    msg_count += 1
                
                print(f"      Total messages: {msg_count}")
            
    return response_data

# Test sequence
print("=== Testing Fetch from Storage ===\n")

print("1. Create topic first")
subprocess.run(['python', 'test_create_topic.py'], capture_output=True)
time.sleep(1)

print("\n2. Send messages via produce")
send_produce('localhost', 9092, 'test-topic', 0, [
    ('key1', 'Message from storage test 1'),
    ('key2', 'Message from storage test 2'),
    ('key3', 'Message from storage test 3')
])

print("\n3. Wait for messages to be flushed to storage")
time.sleep(2)

print("\n4. Fetch messages from offset 0")
send_fetch('localhost', 9092, 'test-topic', 0, 0)

print("\n5. Send more messages")
send_produce('localhost', 9092, 'test-topic', 0, [
    ('key4', 'Message 4'),
    ('key5', 'Message 5')
])

time.sleep(2)

print("\n6. Fetch from offset 3 (should get messages 4 and 5)")
send_fetch('localhost', 9092, 'test-topic', 0, 3)

print("\n7. Check if messages are in buffer or read from storage")
print("   (Check logs to see if messages came from buffer or segments)")

print("\n=== Test Complete ===")