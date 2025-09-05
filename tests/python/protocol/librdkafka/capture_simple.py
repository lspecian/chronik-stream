#!/usr/bin/env python3
"""Simple capture server that logs everything to a file"""

import socket
import struct
import binascii
import sys

def capture_server():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('127.0.0.1', 9095))
    server.listen(1)
    
    print("Capture server on port 9095", flush=True)
    
    client, addr = server.accept()
    print(f"Connection from {addr}", flush=True)
    
    # Read request
    size_bytes = client.recv(4)
    request_size = struct.unpack('>I', size_bytes)[0]
    request = client.recv(request_size)
    
    # Log everything
    with open('/tmp/librdkafka_request.txt', 'w') as f:
        f.write(f"Size: {request_size}\n")
        f.write(f"Request hex: {binascii.hexlify(request).decode()}\n")
        
        if len(request) >= 8:
            api_key = struct.unpack('>h', request[0:2])[0]
            api_version = struct.unpack('>h', request[2:4])[0]
            correlation_id = struct.unpack('>i', request[4:8])[0]
            f.write(f"API Key: {api_key}\n")
            f.write(f"API Version: {api_version}\n")
            f.write(f"Correlation ID: {correlation_id}\n")
            
            # Check for client ID
            pos = 8
            if pos + 2 <= len(request):
                client_id_len = struct.unpack('>h', request[pos:pos+2])[0]
                f.write(f"Client ID len: {client_id_len}\n")
                pos += 2
                if client_id_len > 0:
                    client_id = request[pos:pos+client_id_len]
                    f.write(f"Client ID: {client_id}\n")
                    pos += client_id_len
                f.write(f"Remaining bytes: {binascii.hexlify(request[pos:]).decode()}\n")
    
    print("Request captured to /tmp/librdkafka_request.txt", flush=True)
    
    # For Metadata, send a better error response
    if api_key == 3:  # Metadata
        response = bytearray()
        response.extend(b'\x00\x00\x00\x05')  # Size = 5 bytes
        response.extend(struct.pack('>i', correlation_id))
        response.append(0)  # Tagged fields for flexible header
        client.send(response)
    else:
        # Send minimal response for other APIs
        response = bytearray()
        response.extend(b'\x00\x00\x00\x07')  # Size
        response.extend(struct.pack('>i', correlation_id))
        response.extend(struct.pack('>h', 35))  # Error
        response.append(1)  # Empty array
        client.send(response)
    
    client.close()
    server.close()

if __name__ == "__main__":
    capture_server()