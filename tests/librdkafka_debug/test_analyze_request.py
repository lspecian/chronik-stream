#!/usr/bin/env python3
"""Analyze what librdkafka sends for Metadata v12"""

import socket
import struct
import threading
import time

def read_compact_string(data, offset):
    """Read a compact string from data"""
    length = data[offset] - 1
    if length < 0:
        return None, offset + 1
    return data[offset+1:offset+1+length].decode('utf-8'), offset + 1 + length

def analyze_request(client_socket, addr):
    """Analyze incoming request"""
    print(f"Connection from {addr}")
    
    # Read size
    size_bytes = client_socket.recv(4)
    if len(size_bytes) < 4:
        print("Failed to read size")
        return
        
    size = struct.unpack('>i', size_bytes)[0]
    print(f"Request size: {size} bytes")
    
    # Read full request
    request_body = b''
    while len(request_body) < size:
        chunk = client_socket.recv(size - len(request_body))
        if not chunk:
            break
        request_body += chunk
    
    print(f"Received {len(request_body)} bytes")
    
    # Parse header
    api_key = struct.unpack('>h', request_body[0:2])[0]
    api_version = struct.unpack('>h', request_body[2:4])[0]
    correlation_id = struct.unpack('>i', request_body[4:8])[0]
    
    print(f"API Key: {api_key} (Metadata)")
    print(f"API Version: {api_version}")
    print(f"Correlation ID: {correlation_id}")
    
    # Parse client ID (compact string)
    client_id, offset = read_compact_string(request_body, 8)
    print(f"Client ID: {client_id}")
    print(f"Offset after client ID: {offset}")
    
    # Tagged fields (should be 0)
    tagged_fields = request_body[offset]
    print(f"Tagged fields: {tagged_fields}")
    offset += 1
    
    # Now the request body starts
    print(f"\nRequest body starts at offset {offset}")
    print(f"Remaining bytes: {len(request_body) - offset}")
    
    # Show hex dump of request body
    body_start = request_body[offset:]
    print(f"\nRequest body hex (first 30 bytes):")
    for i in range(0, min(30, len(body_start)), 16):
        hex_str = ' '.join(f'{b:02x}' for b in body_start[i:i+16])
        ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in body_start[i:i+16])
        print(f"{i:04x}: {hex_str:<48} {ascii_str}")
    
    # Try to parse topics array
    print(f"\nAttempting to parse topics array...")
    topics_byte = body_start[0]
    print(f"First byte of body (topics array): 0x{topics_byte:02x} ({topics_byte})")
    
    if topics_byte == 0:
        print("Topics array is NULL (0x00)")
        body_offset = 1
    else:
        # It's a compact array
        topics_len = topics_byte - 1
        print(f"Topics array length: {topics_len}")
        body_offset = 1
        
        if topics_len > 0:
            # Parse topic entries
            for i in range(topics_len):
                # Each topic has topic_id (UUID) and name (compact string)
                topic_id = body_start[body_offset:body_offset+16]
                body_offset += 16
                print(f"  Topic {i} ID: {topic_id.hex()}")
                
                # Topic name
                name, body_offset_new = read_compact_string(body_start, body_offset)
                print(f"  Topic {i} name: {name}")
                body_offset = body_offset_new - offset
    
    print(f"\nAfter topics, offset in body: {body_offset}")
    print(f"Next bytes: {body_start[body_offset:body_offset+10].hex()}")
    
    # Send a dummy response (just correlation ID)
    response = struct.pack('>i', 4) + struct.pack('>i', correlation_id)
    client_socket.send(response)
    client_socket.close()

def main():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('localhost', 19092))
    server.listen(1)
    
    print("Mock server listening on port 19092")
    print("Waiting for librdkafka connection...")
    
    client_socket, addr = server.accept()
    analyze_request(client_socket, addr)
    
    server.close()

if __name__ == "__main__":
    main()