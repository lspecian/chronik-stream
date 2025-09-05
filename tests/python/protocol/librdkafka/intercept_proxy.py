#!/usr/bin/env python3
"""Intercept proxy to capture what librdkafka sends"""

import socket
import sys
import threading

def handle_client(client_sock, server_sock):
    """Forward data from client to server and capture it"""
    while True:
        data = client_sock.recv(4096)
        if not data:
            break
        
        # Log the raw data
        print(f"CLIENT -> SERVER ({len(data)} bytes):")
        print(f"  Hex: {data[:min(100, len(data))].hex()}")
        
        # Parse Kafka request if it's complete
        if len(data) >= 4:
            size = int.from_bytes(data[0:4], 'big')
            print(f"  Request size field: {size}")
            
            if len(data) >= 4 + size and size > 8:
                # Parse header
                api_key = int.from_bytes(data[4:6], 'big', signed=True)
                api_version = int.from_bytes(data[6:8], 'big', signed=True)
                correlation_id = int.from_bytes(data[8:12], 'big')
                print(f"  API: {api_key} v{api_version}, Correlation ID: {correlation_id}")
                
                # Show next bytes after correlation ID
                if len(data) > 12:
                    print(f"  After correlation ID: {data[12:min(12+50, len(data))].hex()}")
                    print(f"  Bytes 12-16 as INT32: {int.from_bytes(data[12:16], 'big') if len(data) >= 16 else 'N/A'}")
                    print(f"  Byte 12 as UINT8: {data[12] if len(data) > 12 else 'N/A'}")
                    print(f"  Bytes 12-13 as INT16: {int.from_bytes(data[12:14], 'big', signed=True) if len(data) >= 14 else 'N/A'}")
        
        server_sock.send(data)

def handle_server(server_sock, client_sock):
    """Forward data from server to client"""
    while True:
        data = server_sock.recv(4096)
        if not data:
            break
        
        print(f"SERVER -> CLIENT ({len(data)} bytes)")
        client_sock.send(data)

def main():
    # Create proxy socket
    proxy_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    proxy_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    proxy_sock.bind(('localhost', 9093))
    proxy_sock.listen(1)
    
    print("Proxy listening on port 9093, forwarding to 9092...")
    
    while True:
        client_sock, addr = proxy_sock.accept()
        print(f"\nNew connection from {addr}")
        
        # Connect to real server
        server_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        server_sock.connect(('localhost', 9092))
        
        # Start forwarding threads
        t1 = threading.Thread(target=handle_client, args=(client_sock, server_sock))
        t2 = threading.Thread(target=handle_server, args=(server_sock, client_sock))
        t1.daemon = True
        t2.daemon = True
        t1.start()
        t2.start()

if __name__ == "__main__":
    main()