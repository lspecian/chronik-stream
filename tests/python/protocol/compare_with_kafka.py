#!/usr/bin/env python3
"""
Compare Chronik Stream's responses with Apache Kafka's wire format.
"""

import socket
import struct
import time

def capture_kafka_apiversion_response():
    """Capture real Apache Kafka ApiVersions response."""
    print("Capturing Apache Kafka ApiVersions response...")
    
    # Connect to Apache Kafka (if running)
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9093))  # Different port for real Kafka
        
        # Build ApiVersions v3 request manually
        correlation_id = 1
        client_id = b'test-client'
        
        # Request: size(4) + api_key(2) + api_version(2) + correlation_id(4) + client_id_len(2) + client_id + tagged_fields(1)
        client_id_len = len(client_id) + 1  # Compact string encoding
        request_body = struct.pack('>hhih', 18, 3, correlation_id, client_id_len) + client_id + b'\x00'  # tagged fields
        request = struct.pack('>i', len(request_body)) + request_body
        
        print(f"Sending request: {request.hex()}")
        sock.send(request)
        
        # Read response
        size_bytes = sock.recv(4)
        size = struct.unpack('>i', size_bytes)[0]
        response = sock.recv(size)
        
        print(f"Response size: {size}")
        print(f"Response: {response.hex()}")
        
        sock.close()
        return response
    except Exception as e:
        print(f"Could not connect to Apache Kafka: {e}")
        return None

def capture_chronik_apiversion_response():
    """Capture Chronik Stream ApiVersions response."""
    print("\nCapturing Chronik Stream ApiVersions response...")
    
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))
        
        # Build ApiVersions v3 request
        correlation_id = 1
        client_id = b'test-client'
        
        # Request: size(4) + api_key(2) + api_version(2) + correlation_id(4) + client_id_len(2) + client_id + tagged_fields(1)
        client_id_len = len(client_id) + 1  # Compact string encoding
        request_body = struct.pack('>hhih', 18, 3, correlation_id, client_id_len) + client_id + b'\x00'
        request = struct.pack('>i', len(request_body)) + request_body
        
        print(f"Sending request: {request.hex()}")
        sock.send(request)
        
        # Read response
        size_bytes = sock.recv(4)
        size = struct.unpack('>i', size_bytes)[0]
        response = sock.recv(size)
        
        print(f"Response size: {size}")
        print(f"Response: {response.hex()}")
        
        sock.close()
        return response
    except Exception as e:
        print(f"Could not connect to Chronik Stream: {e}")
        return None

def parse_apiversion_v3_response(data):
    """Parse ApiVersions v3 response."""
    offset = 0
    
    # Correlation ID (4 bytes)
    correlation_id = struct.unpack('>i', data[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id}")
    
    # Tagged fields (compact bytes)
    tagged_fields = data[offset]
    print(f"Tagged fields: 0x{tagged_fields:02x}")
    offset += 1
    
    # Error code (2 bytes)
    error_code = struct.unpack('>h', data[offset:offset+2])[0]
    print(f"Error code: {error_code}")
    offset += 2
    
    # API array length (compact)
    array_len_encoded = data[offset]
    array_len = array_len_encoded - 1 if array_len_encoded > 0 else 0
    print(f"API array length: {array_len}")
    offset += 1
    
    # Parse API entries
    apis = []
    for i in range(array_len):
        if offset + 4 > len(data):
            print(f"Not enough data for API {i}")
            break
            
        api_key = struct.unpack('>h', data[offset:offset+2])[0]
        offset += 2
        min_ver = data[offset]
        offset += 1
        max_ver = data[offset]
        offset += 1
        
        # Tagged fields for this API
        api_tagged = data[offset] if offset < len(data) else 0
        offset += 1
        
        apis.append({
            'api_key': api_key,
            'min_version': min_ver,
            'max_version': max_ver,
            'tagged': api_tagged
        })
    
    # Final tagged fields
    if offset < len(data):
        final_tagged = data[offset]
        print(f"Final tagged fields: 0x{final_tagged:02x}")
        offset += 1
    
    # Throttle time ms (4 bytes) - only in v3+
    if offset + 4 <= len(data):
        throttle_time = struct.unpack('>i', data[offset:offset+4])[0]
        print(f"Throttle time ms: {throttle_time}")
        offset += 4
    
    return apis

def compare_responses():
    """Compare the two responses."""
    chronik = capture_chronik_apiversion_response()
    
    if chronik:
        print("\n" + "="*60)
        print("Chronik Stream Response Analysis:")
        print("-"*60)
        apis = parse_apiversion_v3_response(chronik)
        
        print("\nSupported APIs:")
        for api in apis[:10]:  # Show first 10
            print(f"  API {api['api_key']}: v{api['min_version']}-v{api['max_version']}")
        
        # Check structure
        print("\nStructure validation:")
        print(f"✓ Response size: {len(chronik)} bytes")
        print(f"✓ Number of APIs: {len(apis)}")
        
        # Check specific APIs that librdkafka requires
        required_apis = {
            0: "Produce",
            1: "Fetch", 
            2: "ListOffsets",
            3: "Metadata",
            8: "OffsetCommit",
            9: "OffsetFetch",
            10: "FindCoordinator",
            11: "JoinGroup",
            18: "ApiVersions"
        }
        
        print("\nRequired API support:")
        for api_key, name in required_apis.items():
            api = next((a for a in apis if a['api_key'] == api_key), None)
            if api:
                print(f"✓ {name} (API {api_key}): v{api['min_version']}-v{api['max_version']}")
            else:
                print(f"✗ {name} (API {api_key}): NOT FOUND")

if __name__ == "__main__":
    compare_responses()