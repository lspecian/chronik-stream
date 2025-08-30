#!/usr/bin/env python3
"""Test Metadata v9+ flexible encoding fix."""

import socket
import struct
import time

def encode_unsigned_varint(value):
    """Encode an unsigned varint."""
    result = bytearray()
    while (value & ~0x7F) != 0:
        result.append((value & 0x7F) | 0x80)
        value >>= 7
    result.append(value & 0x7F)
    return bytes(result)

def encode_compact_string(s):
    """Encode a compact string (varint length + string bytes)."""
    if s is None:
        return encode_unsigned_varint(0)
    encoded = s.encode('utf-8')
    return encode_unsigned_varint(len(encoded) + 1) + encoded

def build_metadata_v9_request():
    """Build a Metadata v9 request with flexible encoding."""
    correlation_id = 99999
    
    # Request header for v9+ (flexible)
    header = bytearray()
    header.extend(struct.pack('>h', 3))  # API key (Metadata)
    header.extend(struct.pack('>h', 9))  # API version 9
    header.extend(struct.pack('>i', correlation_id))  # Correlation ID
    header.extend(encode_compact_string("test-client"))  # Client ID (compact)
    header.extend(encode_unsigned_varint(0))  # Tagged fields
    
    # Request body for Metadata v9
    body = bytearray()
    body.extend(encode_unsigned_varint(0))  # Empty topics array (get all)
    body.extend([0])  # allow_auto_topic_creation = false
    body.extend([0])  # include_cluster_authorized_operations = false
    body.extend([0])  # include_topic_authorized_operations = false
    body.extend(encode_unsigned_varint(0))  # Tagged fields
    
    # Full request
    request = header + body
    return struct.pack('>i', len(request)) + request, correlation_id

def decode_unsigned_varint(data, offset):
    """Decode an unsigned varint from data."""
    value = 0
    shift = 0
    pos = offset
    while True:
        byte = data[pos]
        pos += 1
        value |= (byte & 0x7F) << shift
        if (byte & 0x80) == 0:
            break
        shift += 7
    return value, pos

def decode_compact_string(data, offset):
    """Decode a compact string."""
    length, pos = decode_unsigned_varint(data, offset)
    if length == 0:
        return None, pos
    length -= 1  # Compact strings have length + 1
    string_data = data[pos:pos + length]
    return string_data.decode('utf-8'), pos + length

def test_metadata_v9():
    """Test Metadata v9 with flexible encoding."""
    print("\n=== Testing Metadata v9 Flexible Encoding ===")
    
    try:
        # Connect to server
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        sock.connect(('localhost', 9092))
        
        # Send request
        request, expected_corr_id = build_metadata_v9_request()
        print(f"Sending Metadata v9 request with correlation ID: {expected_corr_id}")
        print(f"Request size: {len(request)} bytes")
        sock.sendall(request)
        
        # Read response
        size_bytes = sock.recv(4)
        if len(size_bytes) < 4:
            print("❌ Failed to receive response size")
            return False
            
        response_size = struct.unpack('>i', size_bytes)[0]
        print(f"Response size: {response_size} bytes")
        
        response = b""
        while len(response) < response_size:
            chunk = sock.recv(min(4096, response_size - len(response)))
            if not chunk:
                break
            response += chunk
        
        if len(response) < response_size:
            print(f"❌ Incomplete response: got {len(response)}, expected {response_size}")
            return False
        
        # Parse response header (flexible)
        pos = 0
        corr_id = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        
        print(f"Received correlation ID: {corr_id}")
        
        if corr_id != expected_corr_id:
            print(f"❌ Correlation ID mismatch: expected {expected_corr_id}, got {corr_id}")
            print(f"Response bytes (first 100): {response[:100].hex()}")
            return False
        
        print("✅ Correlation ID matches!")
        
        # Parse tagged fields
        tagged_fields_count, pos = decode_unsigned_varint(response, pos)
        print(f"Tagged fields: {tagged_fields_count}")
        
        # Parse throttle time
        throttle_time = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"Throttle time: {throttle_time}ms")
        
        # Parse brokers array
        broker_count, pos = decode_unsigned_varint(response, pos)
        if broker_count > 0:
            broker_count -= 1  # Compact arrays use +1 encoding
        print(f"Broker count: {broker_count}")
        
        for i in range(broker_count):
            node_id = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            host, pos = decode_compact_string(response, pos)
            port = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            rack, pos = decode_compact_string(response, pos)
            tagged_fields, pos = decode_unsigned_varint(response, pos)
            print(f"  Broker {i}: node_id={node_id}, host={host}, port={port}, rack={rack}")
        
        # Parse cluster ID
        cluster_id, pos = decode_compact_string(response, pos)
        print(f"Cluster ID: {cluster_id}")
        
        # Parse controller ID
        controller_id = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"Controller ID: {controller_id}")
        
        # Parse topics array
        topic_count, pos = decode_unsigned_varint(response, pos)
        if topic_count > 0:
            topic_count -= 1  # Compact arrays use +1 encoding
        print(f"Topic count: {topic_count}")
        
        sock.close()
        return True
        
    except Exception as e:
        print(f"❌ Exception: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    test_metadata_v9()