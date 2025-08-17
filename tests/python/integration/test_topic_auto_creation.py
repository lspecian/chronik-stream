#!/usr/bin/env python3
"""Test topic auto-creation functionality"""

import struct
import socket
import time
import subprocess
import threading

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
    """Send produce request and return error code"""
    messages_data = create_simple_batch(messages)
    
    body = b''
    body += struct.pack('>h', 1)  # acks
    body += struct.pack('>i', 30000)  # timeout
    body += struct.pack('>i', 1)  # topic count
    body += encode_string(topic)
    body += struct.pack('>i', 1)  # partition count
    body += struct.pack('>i', partition)  # partition index
    body += encode_bytes(messages_data)  # message set
    
    header = b''
    header += struct.pack('>h', 0)  # API key (Produce)  
    header += struct.pack('>h', 0)  # API version 0
    header += struct.pack('>i', 123)  # Correlation ID
    header += encode_string('auto-create-test')  # Client ID
    
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    sock.settimeout(5.0)
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    sock.close()
    
    # Parse response to get error code
    pos = 4  # Skip correlation ID
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2 + topic_name_len
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        for _ in range(partition_count):
            pos += 4  # partition index
            error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            return error_code

def test_auto_creation_enabled():
    """Test topic auto-creation when enabled"""
    print("=== Testing Topic Auto-Creation (Enabled) ===\n")
    
    # Try to produce to a non-existent topic
    topic_name = f"auto-created-topic-{int(time.time())}"
    print(f"1. Producing to non-existent topic: {topic_name}")
    
    error_code = send_produce('localhost', 9092, topic_name, 0, [('key1', 'value1')])
    
    if error_code == 0:
        print("✓ Topic was auto-created successfully")
    else:
        print(f"✗ Expected success (0) but got error code: {error_code}")
    
    # Produce again to verify topic exists
    print("\n2. Producing to the same topic again")
    error_code = send_produce('localhost', 9092, topic_name, 0, [('key2', 'value2')])
    
    if error_code == 0:
        print("✓ Second produce succeeded")
    else:
        print(f"✗ Expected success (0) but got error code: {error_code}")

def test_concurrent_auto_creation():
    """Test concurrent topic auto-creation"""
    print("\n=== Testing Concurrent Topic Auto-Creation ===\n")
    
    topic_name = f"concurrent-topic-{int(time.time())}"
    print(f"Spawning 10 threads to create topic: {topic_name}")
    
    results = []
    def produce_concurrent(thread_id):
        error_code = send_produce('localhost', 9092, topic_name, 0, 
                                [(f'key{thread_id}', f'value{thread_id}')])
        results.append((thread_id, error_code))
    
    threads = []
    for i in range(10):
        thread = threading.Thread(target=produce_concurrent, args=(i,))
        threads.append(thread)
        thread.start()
    
    for thread in threads:
        thread.join()
    
    success_count = sum(1 for _, error_code in results if error_code == 0)
    print(f"\nResults: {success_count}/10 requests succeeded")
    
    if success_count == 10:
        print("✓ All concurrent requests succeeded")
    else:
        print("✗ Some requests failed:")
        for thread_id, error_code in results:
            if error_code != 0:
                print(f"  Thread {thread_id}: error code {error_code}")

def test_invalid_topic_names():
    """Test auto-creation with invalid topic names"""
    print("\n=== Testing Invalid Topic Names ===\n")
    
    invalid_names = [
        ("", "Empty name"),
        ("a" * 250, "Too long (250 chars)"),
        ("topic with spaces", "Contains spaces"),
        ("topic@name", "Contains @"),
        (".", "Single dot"),
        ("..", "Double dot"),
        ("__consumer_offsets", "Reserved name"),
    ]
    
    for topic_name, description in invalid_names:
        error_code = send_produce('localhost', 9092, topic_name, 0, [('key', 'value')])
        
        if error_code != 0:
            print(f"✓ {description}: Correctly rejected (error {error_code})")
        else:
            print(f"✗ {description}: Should have failed but succeeded")

def test_valid_topic_names():
    """Test auto-creation with valid topic names"""
    print("\n=== Testing Valid Topic Names ===\n")
    
    timestamp = int(time.time())
    valid_names = [
        (f"valid-topic-{timestamp}", "Hyphenated name"),
        (f"valid_topic_{timestamp}", "Underscored name"),
        (f"valid.topic.{timestamp}", "Dotted name"),
        (f"123{timestamp}", "Numeric name"),
        (f"a{timestamp}", "Single character"),
        (f"{'x' * 249}", "Maximum length (249 chars)"),
    ]
    
    for topic_name, description in valid_names:
        error_code = send_produce('localhost', 9092, topic_name, 0, [('key', 'value')])
        
        if error_code == 0:
            print(f"✓ {description}: Created successfully")
        else:
            print(f"✗ {description}: Failed with error {error_code}")

def main():
    print("Starting Topic Auto-Creation Tests\n")
    
    # Ensure services are running
    print("Ensuring services are running...")
    subprocess.run(['docker-compose', 'up', '-d'], capture_output=True)
    time.sleep(5)
    
    try:
        test_auto_creation_enabled()
        test_concurrent_auto_creation()
        test_invalid_topic_names()
        test_valid_topic_names()
        
        print("\n=== All Tests Completed ===")
        
    except Exception as e:
        print(f"\n✗ Test failed: {e}")
        raise

if __name__ == "__main__":
    main()