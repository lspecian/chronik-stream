#!/usr/bin/env python3
"""Capture DescribeConfigs request from kafkactl"""

import socket
import struct
import binascii

def start_capture_server():
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('0.0.0.0', 9094))
    server.listen(1)
    
    print("Listening on port 9094...")
    
    while True:
        client, addr = server.accept()
        print(f"\nConnection from {addr}")
        
        # Read data
        data = b''
        while True:
            chunk = client.recv(4096)
            if not chunk:
                break
            data += chunk
            
            # Check if we have a complete request
            if len(data) >= 4:
                req_len = struct.unpack('>I', data[:4])[0]
                if len(data) >= 4 + req_len:
                    # Parse request
                    print(f"\nRequest length: {req_len}")
                    print(f"Request data ({len(data)} bytes):")
                    print(binascii.hexlify(data).decode())
                    
                    # Parse header
                    if len(data) >= 12:
                        api_key, api_version, correlation_id = struct.unpack('>hhi', data[4:12])
                        print(f"\nAPI Key: {api_key}")
                        print(f"API Version: {api_version}")
                        print(f"Correlation ID: {correlation_id}")
                        
                        if api_key == 32:  # DescribeConfigs
                            print("\nThis is a DescribeConfigs request!")
                            # Parse the body
                            offset = 12
                            # Client ID
                            client_id_len = struct.unpack('>h', data[offset:offset+2])[0]
                            offset += 2
                            if client_id_len > 0:
                                client_id = data[offset:offset+client_id_len].decode()
                                offset += client_id_len
                                print(f"Client ID: {client_id}")
                            
                            # Resources array
                            if offset + 4 <= len(data):
                                resource_count = struct.unpack('>i', data[offset:offset+4])[0]
                                offset += 4
                                print(f"\nResource count: {resource_count}")
                                
                                for i in range(resource_count):
                                    if offset + 1 <= len(data):
                                        resource_type = struct.unpack('>b', data[offset:offset+1])[0]
                                        offset += 1
                                        print(f"\nResource {i}: type = {resource_type}")
                                        
                                        # Resource name
                                        if offset + 2 <= len(data):
                                            name_len = struct.unpack('>h', data[offset:offset+2])[0]
                                            offset += 2
                                            print(f"Resource {i}: name length = {name_len}")
                                            
                                            if name_len > 0 and offset + name_len <= len(data):
                                                name = data[offset:offset+name_len].decode()
                                                offset += name_len
                                                print(f"Resource {i}: name = '{name}'")
                    
                    # Send empty response to close connection
                    client.close()
                    break
        
        print("\nConnection closed")

if __name__ == "__main__":
    start_capture_server()