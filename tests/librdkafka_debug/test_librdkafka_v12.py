#!/usr/bin/env python3
"""Test Metadata v12 with raw protocol to debug underflow at byte 1/4"""

import socket
import struct
import time

def encode_metadata_request_v12():
    """Create a Metadata v12 request as librdkafka would send it"""
    # Request header
    api_key = 3  # Metadata
    api_version = 12
    correlation_id = 1
    
    # Client ID - librdkafka sends compact string in flexible versions
    client_id = "rdkafka"
    client_id_bytes = encode_compact_string(client_id)
    
    # Tagged fields (empty)
    tagged_fields = bytes([0])
    
    # Topics array - null to get all topics
    topics = bytes([0])  # Null compact array
    
    # Other v12 fields
    allow_auto_topic_creation = 0x01  # True
    include_cluster_authorized_operations = 0x00  # False
    include_topic_authorized_operations = 0x00  # False
    
    # More tagged fields
    tagged_fields_end = bytes([0])
    
    # Build request body
    body = b''
    body += struct.pack('>h', api_key)
    body += struct.pack('>h', api_version)
    body += struct.pack('>i', correlation_id)
    body += client_id_bytes
    body += tagged_fields
    body += topics
    body += bytes([allow_auto_topic_creation])
    body += bytes([include_cluster_authorized_operations])
    body += bytes([include_topic_authorized_operations])
    body += tagged_fields_end
    
    # Add size header
    size = struct.pack('>i', len(body))
    
    return size + body

def encode_compact_string(s):
    """Encode a compact string (length+1 as varint, then data)"""
    if s is None:
        return bytes([0])  # Null
    
    length = len(s)
    # For small strings, varint is just the value + 1
    if length < 127:
        return bytes([length + 1]) + s.encode('utf-8')
    else:
        # Handle larger strings with proper varint encoding
        raise NotImplementedError("Large string varint encoding not implemented")

def decode_response(data):
    """Decode the Metadata v12 response"""
    print(f"\nResponse total size: {len(data)} bytes")
    
    if len(data) < 4:
        print(f"ERROR: Response too short! Only {len(data)} bytes")
        return
    
    # Response size
    size = struct.unpack('>i', data[0:4])[0]
    print(f"Response size header: {size}")
    
    if len(data) < 4 + size:
        print(f"ERROR: Expected {size} bytes after header but only got {len(data) - 4}")
        return
    
    body = data[4:]
    print(f"Response body size: {len(body)} bytes")
    
    if len(body) < 4:
        print(f"ERROR: Body too short to contain correlation ID! Only {len(body)} bytes")
        print(f"Body hex (first {min(len(body), 20)} bytes): {body[:min(len(body), 20)].hex()}")
        return
    
    # Correlation ID
    correlation_id = struct.unpack('>i', body[0:4])[0]
    print(f"Correlation ID: {correlation_id}")
    
    # Show first 100 bytes of response
    print(f"\nFirst 100 bytes of response:")
    for i in range(0, min(100, len(data)), 16):
        hex_part = ' '.join(f"{b:02x}" for b in data[i:i+16])
        ascii_part = ''.join(chr(b) if 32 <= b < 127 else '.' for b in data[i:i+16])
        print(f"{i:08x}: {hex_part:<48}  {ascii_part}")

def main():
    host = 'localhost'
    port = 9092
    
    print(f"Testing Metadata v12 against {host}:{port}")
    
    # Create request
    request = encode_metadata_request_v12()
    print(f"\nRequest size: {len(request)} bytes")
    print(f"Request hex (first 50 bytes): {request[:50].hex()}")
    
    # Send request
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(5)
        try:
            sock.connect((host, port))
            print(f"\nConnected to {host}:{port}")
            
            sock.sendall(request)
            print("Request sent")
            
            # Read response
            response = b''
            
            # First read the 4-byte size header
            while len(response) < 4:
                chunk = sock.recv(4 - len(response))
                if not chunk:
                    break
                response += chunk
            
            if len(response) < 4:
                print(f"ERROR: Could not read size header, only got {len(response)} bytes")
                return
            
            size = struct.unpack('>i', response[0:4])[0]
            print(f"Response size from header: {size} bytes")
            
            # Read the body
            while len(response) < 4 + size:
                remaining = 4 + size - len(response)
                chunk = sock.recv(min(remaining, 8192))
                if not chunk:
                    break
                response += chunk
            
            decode_response(response)
            
        except socket.timeout:
            print("ERROR: Connection timed out")
        except ConnectionRefusedError:
            print("ERROR: Connection refused")
        except Exception as e:
            print(f"ERROR: {e}")

if __name__ == "__main__":
    main()