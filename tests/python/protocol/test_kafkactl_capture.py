#!/usr/bin/env python3
"""Capture what kafkactl sends and analyze the response"""

import socket
import struct
import threading
import time

def proxy_server():
    """Act as a proxy to capture kafkactl requests"""
    # Listen on port 9093
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('0.0.0.0', 9093))
    server.listen(1)
    
    print("Proxy listening on port 9093...")
    
    while True:
        client, addr = server.accept()
        print(f"\nConnection from {addr}")
        
        # Connect to real server
        upstream = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        upstream.connect(('localhost', 9092))
        
        # Read from client
        data = client.recv(4096)
        if data:
            print(f"Request ({len(data)} bytes): {data[:100].hex()}")
            
            # Parse request
            if len(data) >= 4:
                req_len = struct.unpack('>i', data[:4])[0]
                print(f"Request length: {req_len}")
                
                if len(data) >= 12:
                    api_key, api_version, correlation_id = struct.unpack('>hhi', data[4:12])
                    print(f"API Key: {api_key}, Version: {api_version}, Correlation ID: {correlation_id}")
            
            # Forward to server
            upstream.send(data)
            
            # Read response
            response = upstream.recv(4096)
            if response:
                print(f"Response ({len(response)} bytes): {response[:100].hex()}")
                
                # Parse response
                if len(response) >= 4:
                    resp_len = struct.unpack('>i', response[:4])[0]
                    print(f"Response length: {resp_len}")
                    
                    if len(response) >= 8:
                        resp_correlation_id = struct.unpack('>i', response[4:8])[0]
                        print(f"Response Correlation ID: {resp_correlation_id}")
                        
                        # Check for double correlation ID
                        if len(response) >= 12:
                            second_id = struct.unpack('>i', response[8:12])[0]
                            print(f"Data after correlation ID starts with: {second_id}")
                
                # Forward to client
                client.send(response)
        
        client.close()
        upstream.close()

if __name__ == "__main__":
    proxy_server()