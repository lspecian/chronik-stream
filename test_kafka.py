#!/usr/bin/env python3
"""Test Chronik Stream Kafka compatibility"""

import socket
import struct
import time

def send_kafka_api_versions_request():
    """Send an API versions request to test Kafka protocol compatibility"""
    
    # Connect to the ingest service
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.connect(('localhost', 9092))
        print("✓ Connected to ingest service on port 9092")
        
        # Build API versions request (v3)
        # Format: RequestHeader + ApiVersionsRequest
        
        # RequestHeader:
        # - api_key: int16 (18 for ApiVersions)
        # - api_version: int16 (3)
        # - correlation_id: int32 (42)
        # - client_id: string (length + data)
        
        client_id = b"test-client"
        client_id_len = len(client_id)
        
        # Build the request
        request = struct.pack('>hhih', 18, 3, 42, client_id_len) + client_id
        
        # Add empty tagged fields (compact format)
        request += b'\x00'  # TAG_BUFFER
        
        # Calculate message size (excluding the size field itself)
        message_size = len(request)
        
        # Send size + request
        full_message = struct.pack('>i', message_size) + request
        sock.send(full_message)
        print(f"✓ Sent API versions request ({len(full_message)} bytes)")
        
        # Read response size
        size_data = sock.recv(4)
        if len(size_data) == 4:
            response_size = struct.unpack('>i', size_data)[0]
            print(f"✓ Response size: {response_size} bytes")
            
            # Read response
            response = sock.recv(response_size)
            if len(response) > 0:
                # Parse correlation ID from response
                correlation_id = struct.unpack('>i', response[0:4])[0]
                print(f"✓ Received response with correlation ID: {correlation_id}")
                
                if correlation_id == 42:
                    print("✓ Kafka protocol working correctly!")
                    return True
                else:
                    print("✗ Unexpected correlation ID")
            else:
                print("✗ Empty response")
        else:
            print("✗ Failed to read response size")
            
    except Exception as e:
        print(f"✗ Error: {e}")
    finally:
        sock.close()
    
    return False

def test_metadata_request():
    """Test metadata request"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.connect(('localhost', 9092))
        
        # Metadata request (API key 3)
        client_id = b"test-client"
        client_id_len = len(client_id)
        
        # Build request with no topics (get all topics)
        request = struct.pack('>hhih', 3, 9, 43, client_id_len) + client_id
        request += b'\x00'  # TAG_BUFFER
        request += b'\x00'  # topics array (null)
        request += b'\x01'  # allow_auto_topic_creation = true
        request += b'\x01'  # include_cluster_authorized_operations = true  
        request += b'\x01'  # include_topic_authorized_operations = true
        request += b'\x00'  # TAG_BUFFER
        
        message_size = len(request)
        full_message = struct.pack('>i', message_size) + request
        sock.send(full_message)
        
        print("\n✓ Sent metadata request")
        
        # Read response
        size_data = sock.recv(4)
        if len(size_data) == 4:
            response_size = struct.unpack('>i', size_data)[0]
            response = sock.recv(response_size)
            
            if len(response) > 0:
                correlation_id = struct.unpack('>i', response[0:4])[0]
                print(f"✓ Received metadata response (correlation ID: {correlation_id})")
                return True
                
    except Exception as e:
        print(f"✗ Metadata request error: {e}")
    finally:
        sock.close()
    
    return False

if __name__ == "__main__":
    print("Testing Chronik Stream Kafka Compatibility\n")
    
    # Test 1: API Versions
    api_versions_ok = send_kafka_api_versions_request()
    
    # Test 2: Metadata
    metadata_ok = test_metadata_request()
    
    print("\n" + "="*50)
    if api_versions_ok and metadata_ok:
        print("✓ Kafka protocol is working!")
    else:
        print("✗ Some Kafka protocol tests failed")