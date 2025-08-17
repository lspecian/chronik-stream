#!/usr/bin/env python3
"""Test producing messages to Chronik Stream ingest service."""

import socket
import struct
import time
import json
import zlib

def encode_varint(value):
    """Encode an integer as a varint."""
    result = b''
    while value > 127:
        result += bytes([(value & 0x7F) | 0x80])
        value >>= 7
    result += bytes([value])
    return result

def encode_compact_string(s):
    """Encode a string as Kafka compact string."""
    if s is None:
        return b'\x00'
    data = s.encode('utf-8')
    length = len(data) + 1
    return encode_varint(length) + data

def encode_compact_bytes(data):
    """Encode bytes as Kafka compact bytes."""
    if data is None:
        return b'\x00'
    length = len(data) + 1
    return encode_varint(length) + data

def create_record_batch(messages):
    """Create a Kafka record batch."""
    # Record batch header
    base_offset = 0
    partition_leader_epoch = -1
    magic = 2  # Current magic value
    crc = 0  # We'll calculate this later
    attributes = 0  # No compression
    last_offset_delta = len(messages) - 1
    base_timestamp = int(time.time() * 1000)
    max_timestamp = base_timestamp
    producer_id = -1
    producer_epoch = -1
    base_sequence = -1
    
    # Encode records
    records_data = b''
    for i, (key, value) in enumerate(messages):
        # Record header
        length = 0  # We'll calculate this
        attributes_byte = 0
        timestamp_delta = 0
        offset_delta = i
        
        # Encode key and value
        key_bytes = encode_compact_bytes(key.encode('utf-8') if key else None)
        value_bytes = encode_compact_bytes(value.encode('utf-8') if value else None)
        
        # Headers array (empty)
        headers_count = encode_varint(0)
        
        # Calculate record length
        record_data = (
            bytes([attributes_byte]) +
            encode_varint(timestamp_delta) +
            encode_varint(offset_delta) +
            key_bytes +
            value_bytes +
            headers_count
        )
        
        # Add length-prefixed record
        records_data += encode_varint(len(record_data)) + record_data
    
    # Build batch without CRC
    batch_data = struct.pack(
        '>qihhIhqqqhhi',
        base_offset,
        len(records_data) + 49,  # batch length
        partition_leader_epoch,
        magic,
        0,  # CRC placeholder
        attributes,
        last_offset_delta,
        base_timestamp,
        max_timestamp,
        producer_id,
        producer_epoch,
        base_sequence
    ) + struct.pack('>i', len(messages)) + records_data
    
    # Calculate CRC of everything after CRC field
    crc_data = batch_data[21:]  # Skip to after CRC field
    crc = zlib.crc32(crc_data) & 0xffffffff
    
    # Rebuild with correct CRC
    batch_data = batch_data[:17] + struct.pack('>I', crc) + batch_data[21:]
    
    return batch_data

def create_produce_request(topic, partition, messages):
    """Create a Produce request."""
    # Request header
    api_key = 0  # Produce
    api_version = 8
    correlation_id = 1
    client_id = encode_compact_string("test-producer")
    
    # Tagged fields (empty)
    header_tagged_fields = b'\x00'
    
    # Request body
    transactional_id = encode_compact_string(None)
    acks = struct.pack('>h', 1)  # Wait for leader
    timeout = struct.pack('>i', 30000)  # 30 seconds
    
    # Topics array
    topics_count = encode_varint(1)
    
    # Topic
    topic_name = encode_compact_string(topic)
    
    # Partitions array
    partitions_count = encode_varint(1)
    
    # Partition
    partition_index = struct.pack('>i', partition)
    
    # Records
    record_batch = create_record_batch(messages)
    records = struct.pack('>i', len(record_batch)) + record_batch
    
    # Tagged fields
    partition_tagged_fields = b'\x00'
    topic_tagged_fields = b'\x00'
    body_tagged_fields = b'\x00'
    
    # Build request
    body = (
        transactional_id + acks + timeout + topics_count +
        topic_name + partitions_count + partition_index + 
        records + partition_tagged_fields + topic_tagged_fields +
        body_tagged_fields
    )
    
    header = struct.pack('>hhI', api_key, api_version, correlation_id)
    request = header + client_id + header_tagged_fields + body
    
    # Add length prefix
    return struct.pack('>I', len(request)) + request

def read_response(sock):
    """Read a response from the socket."""
    # Read length
    length_bytes = sock.recv(4)
    if len(length_bytes) < 4:
        raise Exception("Failed to read response length")
    
    length = struct.unpack('>I', length_bytes)[0]
    
    # Read response
    response = b''
    while len(response) < length:
        chunk = sock.recv(length - len(response))
        if not chunk:
            raise Exception("Connection closed while reading response")
        response += chunk
    
    return response

def decode_varint(data, offset):
    """Decode a varint from data starting at offset."""
    result = 0
    shift = 0
    pos = offset
    
    while pos < len(data):
        byte = data[pos]
        result |= (byte & 0x7F) << shift
        pos += 1
        
        if (byte & 0x80) == 0:
            break
        shift += 7
    
    return result, pos

def main():
    """Test producing messages."""
    host = 'localhost'
    port = 9092
    
    print(f"Connecting to {host}:{port}...")
    
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.connect((host, port))
        print("Connected!")
        
        # Prepare test messages
        messages = [
            ("user-123", json.dumps({"event": "login", "timestamp": time.time()})),
            ("user-456", json.dumps({"event": "purchase", "amount": 99.99, "timestamp": time.time()})),
            ("user-789", json.dumps({"event": "logout", "timestamp": time.time()})),
        ]
        
        # Send produce request
        print(f"\nSending Produce request with {len(messages)} messages...")
        request = create_produce_request("events", 0, messages)
        print(f"Request size: {len(request)} bytes")
        sock.sendall(request)
        
        try:
            response = read_response(sock)
            print(f"Received Produce response: {len(response)} bytes")
            
            # Parse response
            offset = 0
            
            # Correlation ID
            correlation_id = struct.unpack('>I', response[offset:offset+4])[0]
            offset += 4
            print(f"Correlation ID: {correlation_id}")
            
            # Tagged fields
            tagged_fields_len, offset = decode_varint(response, offset)
            
            # Topics array
            topics_count, offset = decode_varint(response, offset)
            print(f"Topics count: {topics_count - 1}")  # Compact array
            
            for i in range(topics_count - 1):
                # Topic name
                name_len, offset = decode_varint(response, offset)
                if name_len > 0:
                    topic_name = response[offset:offset+name_len-1].decode('utf-8')
                    offset += name_len - 1
                    print(f"\nTopic: {topic_name}")
                
                # Partitions array
                partitions_count, offset = decode_varint(response, offset)
                print(f"  Partitions count: {partitions_count - 1}")
                
                for j in range(partitions_count - 1):
                    # Partition index
                    partition_index = struct.unpack('>i', response[offset:offset+4])[0]
                    offset += 4
                    
                    # Error code
                    error_code = struct.unpack('>h', response[offset:offset+2])[0]
                    offset += 2
                    
                    # Base offset
                    base_offset = struct.unpack('>q', response[offset:offset+8])[0]
                    offset += 8
                    
                    # Log append time
                    log_append_time = struct.unpack('>q', response[offset:offset+8])[0]
                    offset += 8
                    
                    # Log start offset
                    log_start_offset = struct.unpack('>q', response[offset:offset+8])[0]
                    offset += 8
                    
                    print(f"    Partition {partition_index}:")
                    print(f"      Error code: {error_code}")
                    print(f"      Base offset: {base_offset}")
                    print(f"      Log append time: {log_append_time}")
                    print(f"      Log start offset: {log_start_offset}")
                    
                    # Skip record errors array and error message
                    record_errors_count, offset = decode_varint(response, offset)
                    error_message_len, offset = decode_varint(response, offset)
                    if error_message_len > 0:
                        offset += error_message_len - 1
                    
                    # Skip tagged fields
                    partition_tagged_len, offset = decode_varint(response, offset)
                
                # Skip topic tagged fields
                topic_tagged_len, offset = decode_varint(response, offset)
                
        except Exception as e:
            print(f"Error parsing response: {e}")
            # Print hex dump of response for debugging
            print("\nResponse hex dump:")
            for i in range(0, min(len(response), 200), 16):
                hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
                ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in response[i:i+16])
                print(f"{i:04x}: {hex_str:<48} {ascii_str}")

if __name__ == "__main__":
    main()