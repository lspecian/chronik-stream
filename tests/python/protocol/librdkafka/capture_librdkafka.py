#!/usr/bin/env python3
"""Capture what librdkafka sends to debug parsing issue"""

import socket
import struct
import sys

def capture_librdkafka_request():
    # Create a server socket to capture the request
    server_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server_sock.bind(('localhost', 9093))
    server_sock.listen(1)
    
    print("Waiting for librdkafka connection on port 9093...")
    client_sock, addr = server_sock.accept()
    print(f"Connection from {addr}")
    
    # Read the request
    size_bytes = client_sock.recv(4)
    request_size = int.from_bytes(size_bytes, 'big')
    print(f"Request size: {request_size} bytes")
    
    request = bytearray()
    while len(request) < request_size:
        chunk = client_sock.recv(request_size - len(request))
        if not chunk:
            break
        request.extend(chunk)
    
    print(f"Received {len(request)} bytes")
    
    # Parse header
    pos = 0
    api_key = int.from_bytes(request[pos:pos+2], 'big', signed=True)
    pos += 2
    api_version = int.from_bytes(request[pos:pos+2], 'big', signed=True)
    pos += 2
    correlation_id = int.from_bytes(request[pos:pos+4], 'big')
    pos += 4
    
    print(f"\nRequest header:")
    print(f"  API key: {api_key}")
    print(f"  API version: {api_version}")
    print(f"  Correlation ID: {correlation_id}")
    
    # Now check the client ID part
    print(f"\nBytes after correlation ID (position {pos}):")
    print(f"  Next 20 bytes hex: {request[pos:pos+20].hex()}")
    print(f"  Next 20 bytes decimal: {list(request[pos:pos+20])}")
    
    # Try to parse client ID
    if api_version >= 9:  # Flexible version
        print("\nTrying to parse as flexible version (v9+):")
        
        # Check if it's a compact string
        first_byte = request[pos]
        print(f"  First byte: 0x{first_byte:02x} ({first_byte})")
        
        if first_byte == 0:
            print("  -> Null compact string")
        elif first_byte == 1:
            print("  -> Empty compact string")
        else:
            # Compact string length is (byte - 1)
            str_len = first_byte - 1
            print(f"  -> Compact string length: {str_len}")
            if pos + 1 + str_len <= len(request):
                client_id = request[pos+1:pos+1+str_len].decode('utf-8', errors='replace')
                print(f"  -> Client ID: '{client_id}'")
            else:
                print(f"  -> ERROR: Not enough bytes for string (need {str_len}, have {len(request) - pos - 1})")
        
        print("\nAlternatively, trying as normal string:")
        str_len = int.from_bytes(request[pos:pos+2], 'big', signed=True)
        print(f"  String length (INT16): {str_len}")
        if str_len >= 0 and pos + 2 + str_len <= len(request):
            client_id = request[pos+2:pos+2+str_len].decode('utf-8', errors='replace')
            print(f"  -> Client ID: '{client_id}'")
        else:
            print(f"  -> ERROR: Invalid string length or not enough bytes")
    
    # Send a minimal response to not break the client
    if api_key == 0 and api_version == 9:  # Produce v9
        # Build a minimal error response
        response = bytearray()
        response.extend(correlation_id.to_bytes(4, 'big'))  # Correlation ID
        response.extend((0).to_bytes(4, 'big'))  # Throttle time
        response.extend(b'\x01')  # Empty topics array (compact)
        response.extend(b'\x00')  # Tagged fields
        
        # Send with size prefix
        client_sock.send(len(response).to_bytes(4, 'big'))
        client_sock.send(response)
    
    client_sock.close()
    server_sock.close()

if __name__ == "__main__":
    capture_librdkafka_request()