#\!/usr/bin/env python3
"""Test metadata request with proper Kafka protocol."""

import socket
import struct

def create_metadata_request_v0():
    """Create a Metadata request v0 (simplest version)."""
    # Request header
    api_key = 3  # Metadata
    api_version = 0
    correlation_id = 1
    client_id_len = -1  # null client ID
    
    # Build request
    header = struct.pack('>hhih', api_key, api_version, correlation_id, client_id_len)
    
    # Add length prefix
    return struct.pack('>I', len(header)) + header

def main():
    """Test metadata request."""
    host = 'localhost'
    port = 9092
    
    print(f"Connecting to {host}:{port}...")
    
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.connect((host, port))
        sock.settimeout(5.0)
        print("Connected\!")
        
        # Send metadata request
        print("\nSending Metadata request v0...")
        request = create_metadata_request_v0()
        print(f"Request bytes: {request.hex()}")
        sock.sendall(request)
        
        # Read response length
        print("Reading response...")
        length_bytes = sock.recv(4)
        if len(length_bytes) < 4:
            print(f"Failed to read response length, got {len(length_bytes)} bytes")
            return
            
        length = struct.unpack('>I', length_bytes)[0]
        print(f"Response length: {length}")
        
        # Read response
        response = b''
        while len(response) < length:
            chunk = sock.recv(length - len(response))
            if not chunk:
                print("Connection closed while reading response")
                break
            response += chunk
        
        print(f"Received response: {len(response)} bytes")
        print(f"Response hex: {response[:100].hex()}...")
        
        # Parse correlation ID
        if len(response) >= 4:
            correlation_id = struct.unpack('>I', response[:4])[0]
            print(f"Correlation ID: {correlation_id}")

if __name__ == "__main__":
    main()
