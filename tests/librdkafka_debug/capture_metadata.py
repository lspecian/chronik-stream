#!/usr/bin/env python3
"""Capture actual Metadata request from librdkafka"""

import socket
import struct

def main():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('localhost', 19092))
    server.listen(1)
    
    print("Mock server listening on port 19092")
    print("Waiting for librdkafka connection...")
    
    # Accept connection
    client_socket, addr = server.accept()
    print(f"Connection from {addr}")
    
    # First request should be ApiVersionRequest
    size_bytes = client_socket.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    print(f"First request size: {size} bytes")
    
    # Read full request
    request = client_socket.recv(size)
    
    # Parse header
    api_key = struct.unpack('>h', request[0:2])[0]
    api_version = struct.unpack('>h', request[2:4])[0]
    correlation_id = struct.unpack('>i', request[4:8])[0]
    
    print(f"API Key: {api_key} (ApiVersions)")
    print(f"API Version: {api_version}")
    print(f"Correlation ID: {correlation_id}")
    
    # Send ApiVersions response with Metadata v12 support
    response = bytearray()
    response.extend(struct.pack('>i', correlation_id))  # Correlation ID
    response.extend(struct.pack('>h', 0))  # Error code
    response.extend(struct.pack('>b', 2))  # 1 API (compact array = length+1)
    response.extend(struct.pack('>h', 3))  # API key 3 (Metadata)
    response.extend(struct.pack('>h', 0))  # Min version
    response.extend(struct.pack('>h', 12))  # Max version
    response.append(0)  # Tagged fields
    response.extend(struct.pack('>i', 0))  # Throttle time
    response.append(0)  # Tagged fields
    
    # Send response with size
    full_response = struct.pack('>i', len(response)) + response
    client_socket.send(full_response)
    
    print("\nApiVersions response sent")
    
    # Now wait for Metadata request
    size_bytes = client_socket.recv(4)
    if len(size_bytes) < 4:
        print("Connection closed")
        return
        
    size = struct.unpack('>i', size_bytes)[0]
    print(f"\nSecond request size: {size} bytes")
    
    # Read full request
    request = client_socket.recv(size)
    
    # Parse header
    api_key = struct.unpack('>h', request[0:2])[0]
    api_version = struct.unpack('>h', request[2:4])[0]
    correlation_id = struct.unpack('>i', request[4:8])[0]
    
    print(f"API Key: {api_key} (Metadata)")
    print(f"API Version: {api_version}")
    print(f"Correlation ID: {correlation_id}")
    
    # Show hex dump
    print("\nHex dump of Metadata request:")
    for i in range(0, min(len(request), 100), 16):
        hex_str = ' '.join(f'{b:02x}' for b in request[i:i+16])
        ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in request[i:i+16])
        print(f"{i:04x}: {hex_str:<48} {ascii_str}")
    
    # Save to file for analysis
    with open('metadata_request.bin', 'wb') as f:
        f.write(size_bytes + request)
    print("\nSaved to metadata_request.bin")
    
    client_socket.close()
    server.close()

if __name__ == "__main__":
    main()
