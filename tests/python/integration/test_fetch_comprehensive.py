#!/usr/bin/env python3
"""Comprehensive test suite for fetch handler correctness and performance"""

import struct
import socket
import time
import subprocess
import threading
import statistics

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
    header += encode_string('fetch-test')  # Client ID
    
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

def send_fetch(host, port, topic, partition, fetch_offset, max_bytes=1024*1024, max_wait_ms=1000, min_bytes=1):
    """Send fetch request and measure performance"""
    start_time = time.time()
    
    body = b''
    body += struct.pack('>i', -1)  # replica_id (-1 for consumer)
    body += struct.pack('>i', max_wait_ms)  # max_wait_ms
    body += struct.pack('>i', min_bytes)  # min_bytes
    body += struct.pack('>i', 1)  # topic count
    body += encode_string(topic)
    body += struct.pack('>i', 1)  # partition count
    body += struct.pack('>i', partition)  # partition index
    body += struct.pack('>q', fetch_offset)  # fetch_offset
    body += struct.pack('>i', max_bytes)  # partition_max_bytes
    
    header = b''
    header += struct.pack('>h', 1)  # API key (Fetch)
    header += struct.pack('>h', 0)  # API version 0
    header += struct.pack('>i', 456)  # Correlation ID
    header += encode_string('fetch-test')  # Client ID
    
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    sock.settimeout(max_wait_ms/1000.0 + 1)  # Add 1 second buffer
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    sock.close()
    
    fetch_time = time.time() - start_time
    
    # Parse response
    pos = 4  # Skip correlation ID
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    messages_fetched = 0
    bytes_fetched = 0
    error_code = 0
    high_watermark = 0
    
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2 + topic_name_len
        
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        for _ in range(partition_count):
            pos += 4  # partition id
            error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            high_watermark = struct.unpack('>q', response_data[pos:pos+8])[0]
            pos += 8
            message_set_size = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            
            bytes_fetched = message_set_size
            
            if message_set_size > 0:
                message_set = response_data[pos:pos+message_set_size]
                pos += message_set_size
                
                # Count messages
                msg_pos = 0
                while msg_pos < len(message_set) - 12:
                    msg_pos += 8  # offset
                    msg_size = struct.unpack('>i', message_set[msg_pos:msg_pos+4])[0]
                    msg_pos += 4
                    msg_pos += msg_size
                    messages_fetched += 1
    
    return {
        'time': fetch_time,
        'messages': messages_fetched,
        'bytes': bytes_fetched,
        'error': error_code,
        'high_watermark': high_watermark
    }

def test_correctness():
    """Test fetch correctness"""
    print("=== Testing Fetch Correctness ===\n")
    
    # Test 1: Fetch from empty topic
    print("1. Fetch from empty topic")
    result = send_fetch('localhost', 9092, 'test-empty', 0, 0)
    assert result['messages'] == 0
    assert result['error'] == 3  # UNKNOWN_TOPIC_OR_PARTITION
    print("✓ Empty topic returns correct error\n")
    
    # Test 2: Create topic and produce messages
    print("2. Create topic and produce messages")
    subprocess.run(['python', 'test_create_topic.py'], capture_output=True)
    time.sleep(1)
    
    messages = [(f'key{i}', f'value{i}' * 100) for i in range(100)]
    send_produce('localhost', 9092, 'test-topic', 0, messages[:50])
    time.sleep(1)
    send_produce('localhost', 9092, 'test-topic', 0, messages[50:])
    time.sleep(2)
    
    # Test 3: Fetch all messages
    print("3. Fetch all messages")
    result = send_fetch('localhost', 9092, 'test-topic', 0, 0, max_bytes=10*1024*1024)
    assert result['messages'] == 100
    assert result['error'] == 0
    print(f"✓ Fetched {result['messages']} messages\n")
    
    # Test 4: Fetch with offset
    print("4. Fetch from offset 50")
    result = send_fetch('localhost', 9092, 'test-topic', 0, 50)
    assert result['messages'] == 50
    print(f"✓ Fetched {result['messages']} messages from offset 50\n")
    
    # Test 5: Fetch beyond high watermark
    print("5. Fetch beyond high watermark")
    result = send_fetch('localhost', 9092, 'test-topic', 0, 200, max_wait_ms=100)
    assert result['messages'] == 0
    assert result['high_watermark'] == 100
    print(f"✓ No messages beyond high watermark {result['high_watermark']}\n")
    
    # Test 6: Test max_bytes limit
    print("6. Test max_bytes limit")
    result = send_fetch('localhost', 9092, 'test-topic', 0, 0, max_bytes=1000)
    assert result['messages'] < 100  # Should get partial results
    assert result['bytes'] <= 1000
    print(f"✓ Partial fetch: {result['messages']} messages, {result['bytes']} bytes\n")

def test_timeout_behavior():
    """Test timeout and min_bytes behavior"""
    print("=== Testing Timeout Behavior ===\n")
    
    # Test 1: Wait for min_bytes
    print("1. Test min_bytes with timeout")
    start = time.time()
    result = send_fetch('localhost', 9092, 'test-topic', 0, 100, 
                       max_wait_ms=1000, min_bytes=10000)
    elapsed = time.time() - start
    
    if result['bytes'] >= 10000:
        print(f"✓ Got min_bytes in {elapsed:.2f}s")
    else:
        print(f"✓ Timeout after {elapsed:.2f}s with {result['bytes']} bytes")
    
    # Test 2: Long polling for new data
    print("\n2. Test long polling")
    
    # Start a producer in background
    def produce_delayed():
        time.sleep(0.5)
        send_produce('localhost', 9092, 'test-topic', 0, [('delayed', 'message')])
    
    threading.Thread(target=produce_delayed).start()
    
    start = time.time()
    result = send_fetch('localhost', 9092, 'test-topic', 0, 100, 
                       max_wait_ms=2000, min_bytes=1)
    elapsed = time.time() - start
    
    if result['messages'] > 0:
        print(f"✓ Got new message after {elapsed:.2f}s")
    else:
        print(f"✗ No message received after {elapsed:.2f}s")

def test_performance():
    """Test fetch performance"""
    print("\n=== Testing Fetch Performance ===\n")
    
    # Produce large dataset
    print("Producing 10,000 messages...")
    for i in range(10):
        messages = [(f'key{j}', f'value{j}' * 10) for j in range(i*1000, (i+1)*1000)]
        send_produce('localhost', 9092, 'perf-topic', 0, messages)
    time.sleep(2)
    
    # Test different fetch sizes
    fetch_sizes = [100, 1000, 10000]
    
    for size in fetch_sizes:
        times = []
        
        for _ in range(5):
            result = send_fetch('localhost', 9092, 'perf-topic', 0, 0, 
                              max_bytes=size*1000)
            times.append(result['time'])
        
        avg_time = statistics.mean(times)
        throughput = result['bytes'] / avg_time / 1024 / 1024  # MB/s
        
        print(f"Fetch {size}KB: {avg_time*1000:.1f}ms avg, "
              f"{result['messages']} msgs, {throughput:.1f} MB/s")

def test_cache_performance():
    """Test cache vs storage performance"""
    print("\n=== Testing Cache Performance ===\n")
    
    # First fetch (cold - from storage)
    cold_times = []
    for i in range(5):
        # Clear cache by fetching different partition
        send_fetch('localhost', 9092, f'cache-test-{i}', 0, 0)
        
        start = time.time()
        result = send_fetch('localhost', 9092, 'test-topic', 0, 0, max_bytes=1024*1024)
        cold_times.append(time.time() - start)
    
    # Second fetch (warm - from cache)
    warm_times = []
    for _ in range(5):
        start = time.time()
        result = send_fetch('localhost', 9092, 'test-topic', 0, 0, max_bytes=1024*1024)
        warm_times.append(time.time() - start)
    
    cold_avg = statistics.mean(cold_times) * 1000
    warm_avg = statistics.mean(warm_times) * 1000
    speedup = cold_avg / warm_avg
    
    print(f"Cold fetch: {cold_avg:.1f}ms")
    print(f"Warm fetch: {warm_avg:.1f}ms")
    print(f"Cache speedup: {speedup:.1f}x")

def main():
    print("Starting Chronik-Stream Fetch Handler Tests\n")
    
    # Ensure services are running
    print("Starting services...")
    subprocess.run(['docker-compose', 'up', '-d'], capture_output=True)
    time.sleep(5)
    
    try:
        test_correctness()
        test_timeout_behavior()
        test_performance()
        test_cache_performance()
        
        print("\n=== All Tests Passed ===")
        
    except Exception as e:
        print(f"\n✗ Test failed: {e}")
        raise

if __name__ == "__main__":
    main()