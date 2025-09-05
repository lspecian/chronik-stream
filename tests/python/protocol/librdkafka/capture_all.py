#!/usr/bin/env python3
"""Capture multiple Kafka requests from librdkafka"""

import socket
import struct
import binascii

def capture_server():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('127.0.0.1', 9096))
    server.listen(1)
    
    print("Capture server on port 9096", flush=True)
    
    client, addr = server.accept()
    print(f"Connection from {addr}", flush=True)
    
    request_num = 0
    while True:
        try:
            # Read request size
            size_bytes = client.recv(4)
            if len(size_bytes) < 4:
                print(f"Connection closed after {request_num} requests", flush=True)
                break
                
            request_size = struct.unpack('>I', size_bytes)[0]
            print(f"Request #{request_num}: size={request_size}", flush=True)
            
            # Read request
            request = client.recv(request_size)
            
            # Parse and log
            if len(request) >= 8:
                api_key = struct.unpack('>h', request[0:2])[0]
                api_version = struct.unpack('>h', request[2:4])[0]
                correlation_id = struct.unpack('>i', request[4:8])[0]
                
                print(f"  API Key: {api_key}, Version: {api_version}, CorrId: {correlation_id}", flush=True)
                
                # Save full request
                with open(f'/tmp/request_{request_num}_{api_key}.bin', 'wb') as f:
                    f.write(request)
                
                # Send appropriate response
                if api_key == 18:  # ApiVersions
                    # Send our working ApiVersions response
                    # Just send a minimal success response
                    response = bytearray()
                    response.extend(b'\x00\x00\x01\xac')  # Size = 428 bytes (like our working response)
                    response.extend(struct.pack('>i', correlation_id))
                    # Error code
                    response.extend(b'\x00\x00')
                    # API count (60 APIs, compact encoding)
                    response.append(61)  # 60 + 1 for compact
                    # Add a few APIs
                    for i in range(60):
                        response.extend(struct.pack('>h', i))  # API key
                        response.extend(struct.pack('>h', 0))   # Min version
                        response.extend(struct.pack('>h', 9))   # Max version
                        response.append(0)  # Tagged fields
                    # Throttle time
                    response.extend(b'\x00\x00\x00\x00')
                    # Tagged fields
                    response.append(0)
                    # Update actual size
                    actual_size = len(response) - 4
                    response[0:4] = struct.pack('>I', actual_size)
                    client.send(response)
                    
                elif api_key == 3:  # Metadata
                    print(f"  METADATA REQUEST v{api_version}!", flush=True)
                    print(f"  Full hex: {binascii.hexlify(request).decode()}", flush=True)
                    # Send minimal response to prevent hang
                    response = bytearray()
                    response.extend(b'\x00\x00\x00\x05')  # Size = 5
                    response.extend(struct.pack('>i', correlation_id))
                    response.append(0)  # Tagged fields
                    client.send(response)
                    
                else:
                    # Unknown API, send error
                    response = bytearray()
                    response.extend(b'\x00\x00\x00\x07')  # Size
                    response.extend(struct.pack('>i', correlation_id))
                    response.extend(struct.pack('>h', 35))  # UNSUPPORTED_VERSION
                    response.append(0)
                    client.send(response)
                
            request_num += 1
            
        except Exception as e:
            print(f"Error: {e}", flush=True)
            break
    
    client.close()
    server.close()

if __name__ == "__main__":
    capture_server()