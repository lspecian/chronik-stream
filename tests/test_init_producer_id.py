#!/usr/bin/env python3
"""Test InitProducerId API directly"""

import socket
import struct
import sys

def encode_request_header(api_key, api_version, correlation_id, client_id):
    """Encode Kafka request header"""
    header = struct.pack('>h', api_key)  # API key
    header += struct.pack('>h', api_version)  # API version
    header += struct.pack('>i', correlation_id)  # Correlation ID
    header += struct.pack('>h', len(client_id))  # Client ID length
    header += client_id.encode('utf-8')  # Client ID
    return header

def encode_init_producer_id_request(transactional_id=None, transaction_timeout_ms=60000):
    """Encode InitProducerIdRequest v0"""
    request = b''
    
    # Transactional ID (nullable string)
    if transactional_id:
        request += struct.pack('>h', len(transactional_id))
        request += transactional_id.encode('utf-8')
    else:
        request += struct.pack('>h', -1)  # Null string
    
    # Transaction timeout
    request += struct.pack('>i', transaction_timeout_ms)
    
    return request

def test_init_producer_id():
    """Test InitProducerId API call"""
    try:
        # Connect to Kafka
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))
        
        print("Connected to Kafka server at localhost:9092")
        
        # Create InitProducerIdRequest
        header = encode_request_header(
            api_key=22,  # InitProducerId
            api_version=0,
            correlation_id=1,
            client_id='test-client'
        )
        
        request_body = encode_init_producer_id_request(
            transactional_id='test-transaction-1',
            transaction_timeout_ms=30000
        )
        
        # Send request
        message = header + request_body
        size_bytes = struct.pack('>i', len(message))
        sock.sendall(size_bytes + message)
        
        print("Sent InitProducerIdRequest")
        
        # Read response
        size_data = sock.recv(4)
        if len(size_data) < 4:
            print("Failed to read response size")
            return False
        
        response_size = struct.unpack('>i', size_data)[0]
        response_data = sock.recv(response_size)
        
        # Parse response header
        correlation_id = struct.unpack('>i', response_data[:4])[0]
        print(f"Response correlation ID: {correlation_id}")
        
        # Parse response body (v0)
        offset = 4
        throttle_time = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        error_code = struct.unpack('>h', response_data[offset:offset+2])[0]
        offset += 2
        producer_id = struct.unpack('>q', response_data[offset:offset+8])[0]
        offset += 8
        producer_epoch = struct.unpack('>h', response_data[offset:offset+2])[0]
        
        print(f"Response: throttle_time={throttle_time}ms, error={error_code}, producer_id={producer_id}, producer_epoch={producer_epoch}")
        
        if error_code == 0:
            print("\n✅ InitProducerId SUCCESS!")
            print(f"   Producer ID: {producer_id}")
            print(f"   Producer Epoch: {producer_epoch}")
            return True
        else:
            print(f"\n❌ InitProducerId failed with error code: {error_code}")
            return False
        
        sock.close()
        
    except Exception as e:
        print(f"Test failed: {e}")
        return False

if __name__ == "__main__":
    success = test_init_producer_id()
    sys.exit(0 if success else 1)
