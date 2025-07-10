#!/usr/bin/env python3
"""Simple Kafka producer example for Chronik Stream."""

import socket
import struct
import time
import sys

def encode_string(s):
    """Encode string in Kafka format."""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def create_produce_request(topic, partition, messages, client_id="python-producer"):
    """Create a produce request."""
    # Request header
    api_key = 0  # Produce
    api_version = 8  # Using v8 which is widely supported
    correlation_id = int(time.time() * 1000) % 2147483647
    
    # Build request
    request = b''
    request += struct.pack('>h', api_key)
    request += struct.pack('>h', api_version)
    request += struct.pack('>i', correlation_id)
    request += encode_string(client_id)
    
    # Produce request body
    # Transactional ID (null for non-transactional)
    request += encode_string(None)
    # Acks (-1 = all replicas)
    request += struct.pack('>h', -1)
    # Timeout
    request += struct.pack('>i', 30000)
    
    # Topics array
    request += struct.pack('>i', 1)  # 1 topic
    request += encode_string(topic)
    
    # Partitions array
    request += struct.pack('>i', 1)  # 1 partition
    request += struct.pack('>i', partition)
    
    # Create simple message batch
    batch = create_message_batch(messages)
    request += struct.pack('>i', len(batch))
    request += batch
    
    # Add length prefix
    return struct.pack('>i', len(request)) + request, correlation_id

def create_message_batch(messages):
    """Create a simple record batch (v2 format)."""
    records = b''
    
    for i, (key, value) in enumerate(messages):
        record = b''
        # Record attributes
        record += b'\x00'
        # Timestamp delta (varint)
        record += encode_varint(0)
        # Offset delta (varint)
        record += encode_varint(i)
        # Key length (varint)
        if key is None:
            record += encode_varint(-1)
        else:
            key_bytes = key.encode('utf-8')
            record += encode_varint(len(key_bytes))
            record += key_bytes
        # Value length (varint)
        value_bytes = value.encode('utf-8')
        record += encode_varint(len(value_bytes))
        record += value_bytes
        # Headers array length (varint)
        record += encode_varint(0)
        
        # Record length (varint)
        records += encode_varint(len(record))
        records += record
    
    # Batch header
    batch = b''
    batch += struct.pack('>q', 0)  # Base offset
    batch += struct.pack('>i', len(records) + 49)  # Batch length
    batch += struct.pack('>i', 0)  # Partition leader epoch
    batch += b'\x02'  # Magic byte (v2)
    batch += struct.pack('>i', 0)  # CRC placeholder
    batch += struct.pack('>h', 0)  # Attributes
    batch += struct.pack('>i', len(messages) - 1)  # Last offset delta
    batch += struct.pack('>q', int(time.time() * 1000))  # First timestamp
    batch += struct.pack('>q', int(time.time() * 1000))  # Max timestamp
    batch += struct.pack('>q', -1)  # Producer ID
    batch += struct.pack('>h', -1)  # Producer epoch
    batch += struct.pack('>i', -1)  # Base sequence
    batch += struct.pack('>i', len(messages))  # Record count
    batch += records
    
    return batch

def encode_varint(value):
    """Encode as varint."""
    if value < 0:
        # Negative varints are encoded differently
        value = (1 << 64) + value
    
    result = []
    while (value & 0xFFFFFFFFFFFFFF80) != 0:
        result.append((value & 0x7F) | 0x80)
        value >>= 7
    result.append(value & 0x7F)
    return bytes(result)

def produce_messages(host='localhost', port=9092, topic='test-topic', messages=None):
    """Send messages to Chronik Stream."""
    if messages is None:
        messages = [
            (None, f"Test message {i} at {time.strftime('%Y-%m-%d %H:%M:%S')}")
            for i in range(5)
        ]
    
    print(f"Connecting to {host}:{port}...")
    
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(10)
        sock.connect((host, port))
        print("Connected successfully")
        
        # Send produce request
        request, correlation_id = create_produce_request(topic, 0, messages)
        print(f"Sending produce request ({len(request)} bytes) with {len(messages)} messages...")
        sock.sendall(request)
        
        # Read response
        length_data = sock.recv(4)
        if len(length_data) < 4:
            print("Failed to read response length")
            return False
            
        response_length = struct.unpack('>i', length_data)[0]
        print(f"Response length: {response_length} bytes")
        
        # Read response
        response = b''
        while len(response) < response_length:
            chunk = sock.recv(min(4096, response_length - len(response)))
            if not chunk:
                break
            response += chunk
            
        print(f"Received {len(response)} bytes")
        
        # Parse response
        if len(response) >= 4:
            resp_correlation_id = struct.unpack('>i', response[0:4])[0]
            print(f"Response correlation ID: {resp_correlation_id}")
            
            if resp_correlation_id != correlation_id:
                print("WARNING: Correlation ID mismatch!")
            
            # Parse produce response
            offset = 4
            # Topics array
            if offset + 4 <= len(response):
                topic_count = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                print(f"Topics in response: {topic_count}")
                
                for _ in range(topic_count):
                    # Topic name
                    if offset + 2 <= len(response):
                        name_len = struct.unpack('>h', response[offset:offset+2])[0]
                        offset += 2
                        if name_len > 0 and offset + name_len <= len(response):
                            topic_name = response[offset:offset+name_len].decode('utf-8')
                            offset += name_len
                            print(f"  Topic: {topic_name}")
                            
                            # Partitions array
                            if offset + 4 <= len(response):
                                partition_count = struct.unpack('>i', response[offset:offset+4])[0]
                                offset += 4
                                
                                for _ in range(partition_count):
                                    if offset + 12 <= len(response):
                                        partition_index = struct.unpack('>i', response[offset:offset+4])[0]
                                        error_code = struct.unpack('>h', response[offset+4:offset+6])[0]
                                        base_offset = struct.unpack('>q', response[offset+6:offset+14])[0]
                                        offset += 14
                                        
                                        print(f"    Partition {partition_index}: error={error_code}, offset={base_offset}")
                                        
                                        if error_code == 0:
                                            print(f"Successfully produced messages to partition {partition_index}")
                                        else:
                                            print(f"Error producing to partition {partition_index}: code {error_code}")
        
        return True
        
    except socket.timeout:
        print("Connection timeout")
        return False
    except ConnectionRefusedError:
        print("Connection refused - is the server running?")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False
    finally:
        sock.close()

if __name__ == "__main__":
    if len(sys.argv) > 1:
        topic = sys.argv[1]
    else:
        topic = "test-topic"
    
    print(f"Producing messages to topic: {topic}")
    success = produce_messages(topic=topic)
    sys.exit(0 if success else 1)