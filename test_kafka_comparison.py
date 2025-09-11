#!/usr/bin/env python3
"""
Compare ProduceResponse v2 between real Kafka and Chronik.
This will help identify the exact protocol difference causing client timeouts.
"""

import socket
import struct
import time
import sys

def send_produce_v2_raw(host, port, topic="test-topic"):
    """Send a raw ProduceRequest v2 and capture the response."""
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.settimeout(5.0)
    
    # Build ProduceRequest v2
    # Header: api_key=0, api_version=2, correlation_id=123, client_id="test"
    api_key = 0  # Produce
    api_version = 2  # v2 (has log_append_time)
    correlation_id = 123
    client_id = b"test"
    
    # Request body for v2
    acks = 1  # Wait for leader
    timeout = 5000  # 5 seconds
    
    # Build a minimal record batch (magic v1 for v2 compatibility)
    # Using MessageSet format (pre-v2 format but still valid in v2)
    key = b"key1"
    value = b"test-value-v2"
    
    # Message v1 format (with timestamp)
    magic = 1
    attributes = 0  # No compression
    timestamp = int(time.time() * 1000)
    
    # Build message
    message = bytearray()
    message.append(magic)
    message.append(attributes)
    message.extend(struct.pack('>q', timestamp))  # Timestamp (8 bytes)
    # Key
    if key:
        message.extend(struct.pack('>i', len(key)))
        message.extend(key)
    else:
        message.extend(struct.pack('>i', -1))  # Null key
    # Value
    message.extend(struct.pack('>i', len(value)))
    message.extend(value)
    
    # Calculate CRC32 (covers magic to end of value)
    import zlib
    crc = zlib.crc32(message) & 0xffffffff
    
    # Complete message with CRC
    complete_message = struct.pack('>I', crc) + bytes(message)
    
    # MessageSet entry: offset + message_size + message
    message_set = struct.pack('>q', 0)  # Offset
    message_set += struct.pack('>i', len(complete_message))  # Message size
    message_set += complete_message
    
    # Build topics array
    topics_data = bytearray()
    topics_data.extend(struct.pack('>h', len(topic)))  # Topic name length
    topics_data.extend(topic.encode('utf-8'))
    
    # Partitions array
    topics_data.extend(struct.pack('>i', 1))  # 1 partition
    topics_data.extend(struct.pack('>i', 0))  # Partition 0
    topics_data.extend(struct.pack('>i', len(message_set)))  # MessageSet size
    topics_data.extend(message_set)
    
    # Complete request body
    body = bytearray()
    body.extend(struct.pack('>h', acks))
    body.extend(struct.pack('>i', timeout))
    body.extend(struct.pack('>i', 1))  # 1 topic
    body.extend(topics_data)
    
    # Build request header
    header = bytearray()
    header.extend(struct.pack('>h', api_key))
    header.extend(struct.pack('>h', api_version))
    header.extend(struct.pack('>i', correlation_id))
    header.extend(struct.pack('>h', len(client_id)))
    header.extend(client_id)
    
    # Complete request
    request = header + body
    
    # Send with size prefix
    full_request = struct.pack('>i', len(request)) + request
    
    print(f"Sending ProduceRequest v2 to {host}:{port}")
    print(f"Request size: {len(full_request)} bytes")
    sock.send(full_request)
    
    # Read response
    try:
        # Read size
        size_bytes = sock.recv(4)
        if len(size_bytes) < 4:
            print(f"Failed to read size from {host}:{port}")
            return None
            
        response_size = struct.unpack('>i', size_bytes)[0]
        print(f"Response size: {response_size} bytes")
        
        # Read response
        response = b''
        while len(response) < response_size:
            chunk = sock.recv(min(4096, response_size - len(response)))
            if not chunk:
                break
            response += chunk
            
        print(f"Received {len(response)} bytes from {host}:{port}")
        sock.close()
        return response
        
    except socket.timeout:
        print(f"TIMEOUT receiving response from {host}:{port}")
        sock.close()
        return None

def parse_produce_response_v2(response, source):
    """Parse and display ProduceResponse v2."""
    if not response:
        print(f"No response to parse from {source}")
        return
        
    print(f"\n{'='*60}")
    print(f"ProduceResponse v2 from {source}")
    print(f"{'='*60}")
    
    # Show hex dump
    print("\nFirst 64 bytes (hex):")
    for i in range(0, min(64, len(response)), 16):
        hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
        print(f"  {i:04x}: {hex_str}")
    
    if len(response) > 64:
        print(f"\n... ({len(response) - 64} more bytes)")
    
    # Parse response
    print("\nParsing response:")
    offset = 0
    
    # Correlation ID
    if offset + 4 <= len(response):
        correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"  Correlation ID: {correlation_id}")
        offset += 4
    
    # Now the question: where is throttle_time_ms?
    # Try parsing as if it's at the beginning (after correlation_id)
    print("\n--- Assuming throttle_time_ms at BEGINNING ---")
    test_offset = offset
    
    if test_offset + 4 <= len(response):
        possible_throttle = struct.unpack('>i', response[test_offset:test_offset+4])[0]
        print(f"  Next 4 bytes as int32: {possible_throttle}")
        
        # If this is throttle_time, it should be 0 or a small positive number
        if 0 <= possible_throttle <= 10000:  # Reasonable throttle time
            print(f"  -> Could be throttle_time_ms")
        else:
            print(f"  -> Unlikely to be throttle_time_ms")
    
    # Try parsing the rest as topics array
    print("\n--- Attempting to parse topics array ---")
    
    # Try with throttle at beginning
    print("\nOption 1: throttle_time_ms at beginning")
    try:
        test_offset = 4  # After correlation_id
        throttle_time = struct.unpack('>i', response[test_offset:test_offset+4])[0]
        test_offset += 4
        topics_count = struct.unpack('>i', response[test_offset:test_offset+4])[0]
        print(f"  Throttle: {throttle_time}ms, Topics: {topics_count}")
        if topics_count == 1:
            test_offset += 4
            topic_len = struct.unpack('>h', response[test_offset:test_offset+2])[0]
            if topic_len > 0 and topic_len < 100:
                test_offset += 2
                topic_name = response[test_offset:test_offset+topic_len].decode('utf-8', errors='ignore')
                print(f"  Topic name: '{topic_name}'")
                if "test" in topic_name.lower():
                    print("  ✓ VALID: Found expected topic name with throttle at beginning")
    except:
        print("  ✗ Failed to parse with throttle at beginning")
    
    # Try without throttle at beginning
    print("\nOption 2: NO throttle_time_ms at beginning (v0 style)")
    try:
        test_offset = 4  # After correlation_id
        topics_count = struct.unpack('>i', response[test_offset:test_offset+4])[0]
        print(f"  Topics: {topics_count}")
        if topics_count == 1:
            test_offset += 4
            topic_len = struct.unpack('>h', response[test_offset:test_offset+2])[0]
            if topic_len > 0 and topic_len < 100:
                test_offset += 2
                topic_name = response[test_offset:test_offset+topic_len].decode('utf-8', errors='ignore')
                print(f"  Topic name: '{topic_name}'")
                if "test" in topic_name.lower():
                    print("  ✓ VALID: Found expected topic name without throttle at beginning")
                    
                    # Check if throttle_time might be at the end
                    if len(response) >= 4:
                        possible_throttle_at_end = struct.unpack('>i', response[-4:])[0]
                        print(f"  Last 4 bytes as throttle_time: {possible_throttle_at_end}ms")
    except:
        print("  ✗ Failed to parse without throttle at beginning")
    
    return response

def main():
    """Compare responses from real Kafka and Chronik."""
    
    print("ProduceResponse v2 Protocol Comparison")
    print("="*60)
    
    # Test against real Kafka (on port 29092)
    print("\n1. Testing against real Kafka (port 29092)...")
    kafka_response = send_produce_v2_raw("localhost", 29092)
    
    # Test against Chronik (port 9092)
    print("\n2. Testing against Chronik (port 9092)...")
    chronik_response = send_produce_v2_raw("localhost", 9092)
    
    # Parse and compare
    kafka_parsed = parse_produce_response_v2(kafka_response, "Kafka")
    chronik_parsed = parse_produce_response_v2(chronik_response, "Chronik")
    
    # Binary comparison
    if kafka_response and chronik_response:
        print("\n" + "="*60)
        print("BINARY COMPARISON")
        print("="*60)
        
        if kafka_response == chronik_response:
            print("✓ Responses are IDENTICAL!")
        else:
            print("✗ Responses differ")
            
            # Find first difference
            min_len = min(len(kafka_response), len(chronik_response))
            for i in range(min_len):
                if kafka_response[i] != chronik_response[i]:
                    print(f"\nFirst difference at byte {i} (0x{i:04x}):")
                    
                    # Show context
                    start = max(0, i - 8)
                    end = min(min_len, i + 8)
                    
                    kafka_hex = ' '.join(f'{b:02x}' for b in kafka_response[start:end])
                    chronik_hex = ' '.join(f'{b:02x}' for b in chronik_response[start:end])
                    
                    print(f"  Kafka:   {kafka_hex}")
                    print(f"  Chronik: {chronik_hex}")
                    print(f"           {' ' * (3 * (i - start))}^^")
                    
                    # Interpret the difference
                    if i == 4:  # Right after correlation_id
                        print("\nDifference is at offset 4 - this is where throttle_time_ms might be")
                        kafka_val = struct.unpack('>i', kafka_response[i:i+4])[0] if i+4 <= len(kafka_response) else 0
                        chronik_val = struct.unpack('>i', chronik_response[i:i+4])[0] if i+4 <= len(chronik_response) else 0
                        print(f"  Kafka value: {kafka_val}")
                        print(f"  Chronik value: {chronik_val}")
                    break
            
            if len(kafka_response) != len(chronik_response):
                print(f"\nLength difference: Kafka={len(kafka_response)}, Chronik={len(chronik_response)}")

if __name__ == "__main__":
    main()