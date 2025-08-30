#!/usr/bin/env python3
"""Debug exact metadata response bytes."""

import socket
import struct

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
    correlation_id = 55555
    
    # Request header for v9+ (flexible)
    header = bytearray()
    header.extend(struct.pack('>h', 3))  # API key (Metadata)
    header.extend(struct.pack('>h', 9))  # API version 9
    header.extend(struct.pack('>i', correlation_id))  # Correlation ID
    header.extend(encode_compact_string("test-debug"))  # Client ID (compact)
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
        if pos >= len(data):
            return None, pos
        byte = data[pos]
        pos += 1
        value |= (byte & 0x7F) << shift
        if (byte & 0x80) == 0:
            break
        shift += 7
    return value, pos

def debug_response():
    """Debug the exact response bytes."""
    print("\n=== Debugging Exact Metadata v9 Response Bytes ===")
    
    try:
        # Connect to server
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        sock.connect(('localhost', 9092))
        
        # Send request
        request, expected_corr_id = build_metadata_v9_request()
        print(f"Sending request with correlation ID: {expected_corr_id}")
        sock.sendall(request)
        
        # Read response
        size_bytes = sock.recv(4)
        response_size = struct.unpack('>i', size_bytes)[0]
        print(f"Response size: {response_size} bytes")
        
        response = b""
        while len(response) < response_size:
            chunk = sock.recv(min(4096, response_size - len(response)))
            if not chunk:
                break
            response += chunk
        
        # Parse byte by byte
        pos = 0
        print(f"\n=== Parsing Response Byte by Byte ===")
        
        # Correlation ID (4 bytes)
        corr_id = struct.unpack('>i', response[pos:pos+4])[0]
        print(f"[{pos:04x}] Correlation ID: {corr_id} (bytes: {response[pos:pos+4].hex()})")
        pos += 4
        
        # Tagged fields (varint)
        tagged_fields, new_pos = decode_unsigned_varint(response, pos)
        print(f"[{pos:04x}] Tagged fields: {tagged_fields} (bytes: {response[pos:new_pos].hex()})")
        pos = new_pos
        
        # Throttle time (4 bytes) 
        throttle = struct.unpack('>i', response[pos:pos+4])[0]
        print(f"[{pos:04x}] Throttle time: {throttle}ms (bytes: {response[pos:pos+4].hex()})")
        pos += 4
        
        # Brokers array length (varint)
        brokers_len, new_pos = decode_unsigned_varint(response, pos)
        print(f"[{pos:04x}] Brokers array varint: {brokers_len} (bytes: {response[pos:new_pos].hex()})")
        pos = new_pos
        
        # PROBLEM: If brokers_len is 0, we're reading the wrong thing
        # Let's see what the next bytes are
        print(f"\nNext 32 bytes after broker count:")
        for i in range(32):
            if pos + i < len(response):
                print(f"  [{pos+i:04x}] 0x{response[pos+i]:02x} = {response[pos+i]:3d} = '{chr(response[pos+i]) if 32 <= response[pos+i] < 127 else '.'}'")
        
        sock.close()
        
    except Exception as e:
        print(f"❌ Exception: {e}")
        import traceback
        traceback.print_exc()

if __name__ == "__main__":
    debug_response()