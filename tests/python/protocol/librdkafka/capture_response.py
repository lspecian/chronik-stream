#!/usr/bin/env python3
"""Capture actual response from chronik-server"""

import socket
import struct
import binascii

def proxy_server():
    # Listen for client
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('127.0.0.1', 9098))
    server.listen(1)
    
    print("Proxy server on port 9098", flush=True)
    
    client, addr = server.accept()
    print(f"Client connected from {addr}", flush=True)
    
    # Connect to real server
    backend = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    backend.connect(('127.0.0.1', 9092))
    print("Connected to backend", flush=True)
    
    request_num = 0
    while True:
        try:
            # Read request from client
            size_bytes = client.recv(4)
            if len(size_bytes) < 4:
                print("Client disconnected", flush=True)
                break
                
            request_size = struct.unpack('>I', size_bytes)[0]
            request = b''
            while len(request) < request_size:
                chunk = client.recv(min(4096, request_size - len(request)))
                if not chunk:
                    break
                request += chunk
            
            # Parse request
            if len(request) >= 8:
                api_key = struct.unpack('>h', request[0:2])[0]
                api_version = struct.unpack('>h', request[2:4])[0]
                correlation_id = struct.unpack('>i', request[4:8])[0]
                print(f"\nRequest #{request_num}: API={api_key}, Version={api_version}, CorrId={correlation_id}", flush=True)
            
            # Forward to backend
            backend.send(size_bytes + request)
            
            # Read response from backend
            resp_size_bytes = backend.recv(4)
            if len(resp_size_bytes) < 4:
                print("Backend disconnected", flush=True)
                break
            
            response_size = struct.unpack('>I', resp_size_bytes)[0]
            response = b''
            while len(response) < response_size:
                chunk = backend.recv(min(4096, response_size - len(response)))
                if not chunk:
                    break
                response += chunk
            
            print(f"Response size: {response_size}, actual: {len(response)}", flush=True)
            
            # Save Metadata responses
            if api_key == 3:
                filename = f'/tmp/metadata_v{api_version}_resp{request_num}.bin'
                with open(filename, 'wb') as f:
                    f.write(response)
                print(f"Saved response to {filename}", flush=True)
                
                # Show first 100 bytes
                if len(response) > 0:
                    print(f"First 100 bytes: {binascii.hexlify(response[:min(100, len(response))]).decode()}", flush=True)
            
            # Forward response to client
            client.send(resp_size_bytes + response)
            
            request_num += 1
            
        except Exception as e:
            print(f"Error: {e}", flush=True)
            break
    
    client.close()
    backend.close()
    server.close()

if __name__ == "__main__":
    proxy_server()