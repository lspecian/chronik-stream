#!/usr/bin/env python3
"""Debug what kafkactl sends by creating a mock server."""

import socket
import struct
import sys
import threading

def handle_client(client_socket, addr):
    """Handle a client connection."""
    print(f"Connection from {addr}")
    
    try:
        while True:
            # Read length prefix
            length_data = client_socket.recv(4)
            if not length_data:
                break
                
            request_length = struct.unpack('>i', length_data)[0]
            print(f"Request length: {request_length} bytes")
            
            # Read request
            request = b''
            while len(request) < request_length:
                chunk = client_socket.recv(min(4096, request_length - len(request)))
                if not chunk:
                    break
                request += chunk
                
            print(f"Received {len(request)} bytes")
            
            # Parse request header
            if len(request) >= 8:
                api_key = struct.unpack('>h', request[0:2])[0]
                api_version = struct.unpack('>h', request[2:4])[0]
                correlation_id = struct.unpack('>i', request[4:8])[0]
                print(f"API Key: {api_key}, Version: {api_version}, Correlation ID: {correlation_id}")
                
                # Print hex dump of first 64 bytes
                print("Hex dump:")
                for i in range(0, min(64, len(request)), 16):
                    hex_str = ' '.join(f'{b:02x}' for b in request[i:i+16])
                    ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in request[i:i+16])
                    print(f"{i:04x}: {hex_str:<48} {ascii_str}")
            
            # Send a minimal metadata response
            response = create_metadata_response(correlation_id, api_version)
            client_socket.sendall(response)
            print(f"Sent response ({len(response)} bytes)")
            print("-" * 60)
            
    except Exception as e:
        print(f"Error: {e}")
    finally:
        client_socket.close()

def create_metadata_response(correlation_id, api_version):
    """Create a metadata response."""
    response = b''
    
    # Response header - just correlation ID
    response += struct.pack('>i', correlation_id)
    
    # Throttle time (v3+)
    if api_version >= 3:
        response += struct.pack('>i', 0)
    
    # Brokers array
    response += struct.pack('>i', 1)  # 1 broker
    response += struct.pack('>i', 1)  # node_id
    response += encode_string("localhost")  # host
    response += struct.pack('>i', 9093)  # port (different to avoid conflict)
    
    if api_version >= 1:
        response += encode_string(None)  # rack
    
    # Cluster ID (v2+)
    if api_version >= 2:
        response += encode_string("test-cluster")
    
    # Controller ID (v1+)
    if api_version >= 1:
        response += struct.pack('>i', 1)
    
    # Topics array
    response += struct.pack('>i', 0)  # 0 topics
    
    # Add length prefix
    return struct.pack('>i', len(response)) + response

def encode_string(s):
    """Encode string in Kafka format."""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def main():
    """Run mock server."""
    host = 'localhost'
    port = 9093
    
    server_socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server_socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server_socket.bind((host, port))
    server_socket.listen(5)
    
    print(f"Mock Kafka server listening on {host}:{port}")
    print("Press Ctrl+C to stop")
    
    try:
        while True:
            client_socket, addr = server_socket.accept()
            thread = threading.Thread(target=handle_client, args=(client_socket, addr))
            thread.daemon = True
            thread.start()
    except KeyboardInterrupt:
        print("\nShutting down...")
    finally:
        server_socket.close()

if __name__ == "__main__":
    main()