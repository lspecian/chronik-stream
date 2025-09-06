#!/usr/bin/env python3
"""Capture and analyze actual librdkafka Metadata v12 request"""

import socket
import struct
import sys

def capture_request():
    """Capture raw request from librdkafka"""
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('localhost', 29092))
    server.listen(1)
    
    print("Listening on port 29092...")
    
    # Accept connection
    client, addr = server.accept()
    print(f"Connection from {addr}")
    
    # First, handle ApiVersions request
    size_bytes = client.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    print(f"ApiVersions request size: {size}")
    
    # Read and discard ApiVersions request
    api_req = client.recv(size)
    
    # Send ApiVersions response (simplified)
    api_resp = bytearray()
    api_resp.extend(struct.pack('>i', 428))  # Size
    api_resp.extend(struct.pack('>i', 1))    # Correlation ID
    api_resp.extend(struct.pack('>h', 0))    # Error code
    
    # Add some API versions including Metadata v12
    api_resp.extend(struct.pack('>i', 1))    # 1 API
    api_resp.extend(struct.pack('>h', 3))    # API key 3 (Metadata)
    api_resp.extend(struct.pack('>h', 0))    # Min version
    api_resp.extend(struct.pack('>h', 12))   # Max version
    api_resp.append(0)  # Tagged fields
    
    api_resp.extend(struct.pack('>i', 0))    # Throttle time
    api_resp.append(0)  # Tagged fields
    
    # Update size
    actual_size = len(api_resp) - 4
    api_resp[0:4] = struct.pack('>i', actual_size)
    
    client.send(api_resp)
    
    # Now capture the Metadata request
    size_bytes = client.recv(4)
    if len(size_bytes) < 4:
        print("Failed to read Metadata size")
        return
        
    size = struct.unpack('>i', size_bytes)[0]
    print(f"\nMetadata request size: {size}")
    
    # Read full request
    request = b''
    while len(request) < size:
        chunk = client.recv(size - len(request))
        if not chunk:
            break
        request += chunk
    
    print(f"Received {len(request)} bytes")
    
    # Save to file for analysis
    with open('librdkafka_metadata_v12.bin', 'wb') as f:
        f.write(size_bytes + request)
    
    print("\nRequest saved to librdkafka_metadata_v12.bin")
    
    # Show hex dump
    print("\nHex dump of Metadata request:")
    for i in range(0, min(len(request), 200), 16):
        hex_str = ' '.join(f'{b:02x}' for b in request[i:i+16])
        ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in request[i:i+16])
        print(f"{i:04x}: {hex_str:<48} {ascii_str}")
    
    # Parse header
    api_key = struct.unpack('>h', request[0:2])[0]
    api_version = struct.unpack('>h', request[2:4])[0]
    correlation_id = struct.unpack('>i', request[4:8])[0]
    
    print(f"\nHeader: API={api_key}, Version={api_version}, CorrelationID={correlation_id}")
    
    # Show remaining bytes after header
    print(f"\nBytes after correlation ID (offset 8):")
    remaining = request[8:]
    for i in range(0, min(len(remaining), 100), 16):
        hex_str = ' '.join(f'{b:02x}' for b in remaining[i:i+16])
        ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in remaining[i:i+16])
        print(f"{i:04x}: {hex_str:<48} {ascii_str}")
    
    client.close()
    server.close()

if __name__ == "__main__":
    capture_request()