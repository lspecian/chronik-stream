#!/usr/bin/env python3
"""Capture and decode Kafka requests to understand what librdkafka is sending"""

import socket
import struct
import binascii

def start_mock_server():
    """Start a mock Kafka server to capture librdkafka's request"""
    
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('0.0.0.0', 9094))
    server.listen(1)
    
    print("Mock Kafka server listening on port 9094")
    print("Run librdkafka client connecting to localhost:9094")
    print("=" * 60)
    
    while True:
        client, addr = server.accept()
        print(f"\nConnection from {addr}")
        
        try:
            # Read request size
            size_bytes = client.recv(4)
            if len(size_bytes) < 4:
                print("Failed to read size")
                continue
                
            request_size = struct.unpack('>I', size_bytes)[0]
            print(f"Request size: {request_size} bytes")
            
            # Read request
            request = client.recv(request_size)
            print(f"Request received ({len(request)} bytes)")
            print(f"Hex: {binascii.hexlify(request).decode()}")
            
            # Parse request header
            if len(request) >= 8:
                api_key = struct.unpack('>h', request[0:2])[0]
                api_version = struct.unpack('>h', request[2:4])[0]
                correlation_id = struct.unpack('>i', request[4:8])[0]
                
                print(f"\nParsed request header:")
                print(f"  API Key: {api_key}")
                print(f"  API Version: {api_version}")
                print(f"  Correlation ID: {correlation_id}")
                
                # Parse client ID
                pos = 8
                if api_version >= 1 and pos + 2 <= len(request):
                    client_id_len = struct.unpack('>h', request[pos:pos+2])[0]
                    pos += 2
                    if client_id_len > 0 and pos + client_id_len <= len(request):
                        client_id = request[pos:pos+client_id_len].decode('utf-8', errors='ignore')
                        print(f"  Client ID: {client_id}")
                        pos += client_id_len
                
                # For ApiVersions request, check if there's tagged fields
                if api_key == 18 and api_version >= 3:
                    print(f"\nRemaining bytes after client_id: {binascii.hexlify(request[pos:]).decode()}")
                    if pos < len(request):
                        # Check for tagged fields
                        tagged_field = request[pos]
                        print(f"  Tagged fields byte: 0x{tagged_field:02x}")
            
            # Send a minimal response to prevent client from hanging
            # ApiVersions v3 response with error
            if api_key == 18:
                response = bytearray()
                # Size placeholder
                response.extend(b'\x00\x00\x00\x00')
                # Correlation ID
                response.extend(struct.pack('>i', correlation_id))
                # Error code (35 = UNSUPPORTED_VERSION)
                response.extend(struct.pack('>h', 35))
                # Empty array (compact)
                response.append(1)  # length + 1 = 1 means 0 items
                
                # Update size
                size = len(response) - 4
                response[0:4] = struct.pack('>I', size)
                
                client.send(response)
                print(f"Sent error response ({len(response)} bytes)")
            
        except Exception as e:
            print(f"Error: {e}")
        finally:
            client.close()

if __name__ == "__main__":
    start_mock_server()