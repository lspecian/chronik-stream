#!/usr/bin/env python3
"""Test that unknown API keys preserve correlation IDs"""

import socket
import struct
from typing import Tuple

def send_kafka_request(sock: socket.socket, api_key: int, api_version: int, correlation_id: int, client_id: str = "test-client") -> bytes:
    """Send a Kafka request and return the response"""
    
    # Build request header
    header = struct.pack('>h', api_key)  # API key (2 bytes)
    header += struct.pack('>h', api_version)  # API version (2 bytes)
    header += struct.pack('>i', correlation_id)  # Correlation ID (4 bytes)
    
    # Client ID (string)
    if client_id:
        client_id_bytes = client_id.encode('utf-8')
        header += struct.pack('>h', len(client_id_bytes))  # Length (2 bytes)
        header += client_id_bytes
    else:
        header += struct.pack('>h', -1)  # Null string
    
    # Empty body for now
    body = b''
    
    # Combine header and body
    request = header + body
    
    # Send with length prefix
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read response
    # First read the 4-byte length
    length_data = sock.recv(4)
    if len(length_data) < 4:
        raise Exception("Failed to read response length")
    
    response_length = struct.unpack('>i', length_data)[0]
    
    # Read the response
    response = b''
    while len(response) < response_length:
        chunk = sock.recv(response_length - len(response))
        if not chunk:
            raise Exception("Connection closed while reading response")
        response += chunk
    
    return response

def parse_response_header(response: bytes) -> Tuple[int, int]:
    """Parse correlation ID and error code from response"""
    if len(response) < 6:  # At least correlation ID (4) + error code (2)
        raise Exception(f"Response too short: {len(response)} bytes")
    
    correlation_id = struct.unpack('>i', response[0:4])[0]
    error_code = struct.unpack('>h', response[4:6])[0]
    
    return correlation_id, error_code

def test_unknown_api():
    """Test that unknown API preserves correlation ID"""
    
    # Connect to Chronik Stream
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    try:
        # Test 1: Known API (Metadata = 3)
        print("Test 1: Known API (Metadata)")
        response = send_kafka_request(sock, api_key=3, api_version=0, correlation_id=12345)
        corr_id, error_code = parse_response_header(response)
        print(f"  Sent correlation ID: 12345")
        print(f"  Received correlation ID: {corr_id}")
        print(f"  Error code: {error_code}")
        assert corr_id == 12345, f"Correlation ID mismatch for known API: expected 12345, got {corr_id}"
        print("  ✓ PASS\n")
        
        # Test 2: Unknown API (999)
        print("Test 2: Unknown API (999)")
        response = send_kafka_request(sock, api_key=999, api_version=0, correlation_id=67890)
        corr_id, error_code = parse_response_header(response)
        print(f"  Sent correlation ID: 67890")
        print(f"  Received correlation ID: {corr_id}")
        print(f"  Error code: {error_code} (35 = UNSUPPORTED_VERSION)")
        assert corr_id == 67890, f"Correlation ID mismatch for unknown API: expected 67890, got {corr_id}"
        assert error_code == 35, f"Expected error code 35 (UNSUPPORTED_VERSION), got {error_code}"
        print("  ✓ PASS\n")
        
        # Test 3: Another unknown API with different correlation ID
        print("Test 3: Another unknown API (5000)")
        response = send_kafka_request(sock, api_key=5000, api_version=0, correlation_id=99999)
        corr_id, error_code = parse_response_header(response)
        print(f"  Sent correlation ID: 99999")
        print(f"  Received correlation ID: {corr_id}")
        print(f"  Error code: {error_code} (35 = UNSUPPORTED_VERSION)")
        assert corr_id == 99999, f"Correlation ID mismatch for unknown API: expected 99999, got {corr_id}"
        assert error_code == 35, f"Expected error code 35 (UNSUPPORTED_VERSION), got {error_code}"
        print("  ✓ PASS\n")
        
        print("All tests passed! Unknown APIs now properly preserve correlation IDs.")
        
    finally:
        sock.close()

if __name__ == "__main__":
    test_unknown_api()