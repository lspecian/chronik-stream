#!/usr/bin/env python3
"""Test Kafka protocol implementation with raw socket connection."""

import socket
import struct
import sys

def encode_varint(value):
    """Encode integer as varint."""
    result = []
    while (value & 0xFFFFFF80) != 0:
        result.append((value & 0x7F) | 0x80)
        value >>= 7
    result.append(value & 0x7F)
    return bytes(result)

def encode_string(s):
    """Encode string in Kafka format (length + bytes)."""
    if s is None:
        return b'\xff\xff'  # null string
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def create_metadata_request(client_id="test-client"):
    """Create a metadata request."""
    # Request header
    api_key = 3  # Metadata
    api_version = 9
    correlation_id = 1
    
    # Build request
    request = b''
    request += struct.pack('>h', api_key)
    request += struct.pack('>h', api_version)
    request += struct.pack('>i', correlation_id)
    request += encode_string(client_id)
    
    # Metadata request body (v9)
    # Topics array (null = all topics)
    request += b'\xff\xff\xff\xff'  # null array
    # Allow auto topic creation
    request += b'\x01'  # true
    # Include cluster authorized operations
    request += b'\x00'  # false
    # Include topic authorized operations  
    request += b'\x00'  # false
    
    # Add length prefix
    return struct.pack('>i', len(request)) + request

def parse_response(data):
    """Parse response and extract correlation ID."""
    if len(data) < 8:
        return None, "Response too short"
    
    # Skip length prefix (4 bytes)
    correlation_id = struct.unpack('>i', data[4:8])[0]
    return correlation_id, None

def test_kafka_connection(host='localhost', port=9092):
    """Test Kafka connection with metadata request."""
    print(f"Connecting to {host}:{port}...")
    
    try:
        # Create socket
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        
        # Connect
        sock.connect((host, port))
        print("Connected successfully")
        
        # Send metadata request
        request = create_metadata_request()
        print(f"Sending metadata request ({len(request)} bytes)...")
        sock.sendall(request)
        
        # Read response length
        length_data = sock.recv(4)
        if len(length_data) < 4:
            print(f"Failed to read response length: got {len(length_data)} bytes")
            return False
            
        response_length = struct.unpack('>i', length_data)[0]
        print(f"Response length: {response_length} bytes")
        
        # Read response
        response = b''
        while len(response) < response_length:
            chunk = sock.recv(min(4096, response_length - len(response)))
            if not chunk:
                break
            response += chunk
            
        print(f"Received {len(response)} bytes")
        
        # Parse response
        full_response = length_data + response
        correlation_id, error = parse_response(full_response)
        if error:
            print(f"Parse error: {error}")
            return False
            
        print(f"Correlation ID: {correlation_id}")
        
        # Check if response has error code
        if len(response) >= 2:
            error_code = struct.unpack('>h', response[0:2])[0]
            print(f"Error code: {error_code}")
            if error_code != 0:
                print(f"Server returned error code: {error_code}")
                return False
        
        print("Metadata request successful!")
        return True
        
    except socket.timeout:
        print("Connection timeout")
        return False
    except ConnectionRefusedError:
        print("Connection refused - is the server running?")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False
    finally:
        sock.close()

if __name__ == "__main__":
    success = test_kafka_connection()
    sys.exit(0 if success else 1)