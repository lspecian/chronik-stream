#!/usr/bin/env python3
"""Test basic produce functionality."""

import socket
import struct
import time

def test_produce():
    """Test basic produce request."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(5)
    
    try:
        # Connect
        sock.connect(('localhost', 9092))
        print("Connected to localhost:9092")
        
        # Create minimal produce request
        # API key (0 = Produce)
        request = struct.pack('>h', 0)
        # API version (0 = simplest)
        request += struct.pack('>h', 0)
        # Correlation ID
        request += struct.pack('>i', 12345)
        # Client ID (-1 = null)
        request += struct.pack('>h', -1)
        
        # Produce request body v0:
        # Required acks
        request += struct.pack('>h', 1)
        # Timeout
        request += struct.pack('>i', 1000)
        # Topics array
        request += struct.pack('>i', 1)  # 1 topic
        # Topic name
        topic_name = b'test'
        request += struct.pack('>h', len(topic_name))
        request += topic_name
        # Partitions array
        request += struct.pack('>i', 1)  # 1 partition
        # Partition index
        request += struct.pack('>i', 0)
        # Message set size
        msg = b'Hello Chronik!'
        # Simple message format (v0)
        message_set = b''
        # Offset
        message_set += struct.pack('>q', 0)
        # Message size
        msg_bytes = struct.pack('>i', 0) + b'\x00' + struct.pack('>i', -1) + struct.pack('>i', len(msg)) + msg
        message_set += struct.pack('>i', len(msg_bytes))
        message_set += msg_bytes
        
        request += struct.pack('>i', len(message_set))
        request += message_set
        
        # Send with length prefix
        full_request = struct.pack('>i', len(request)) + request
        sock.sendall(full_request)
        print(f"Sent produce request ({len(full_request)} bytes)")
        
        # Read response
        resp_len_bytes = sock.recv(4)
        if len(resp_len_bytes) < 4:
            print("Failed to read response length")
            return
            
        resp_len = struct.unpack('>i', resp_len_bytes)[0]
        print(f"Response length: {resp_len}")
        
        response = b''
        while len(response) < resp_len:
            chunk = sock.recv(min(4096, resp_len - len(response)))
            if not chunk:
                break
            response += chunk
            
        print(f"Received {len(response)} bytes")
        
        # Parse response
        if len(response) >= 4:
            correlation_id = struct.unpack('>i', response[0:4])[0]
            print(f"Correlation ID: {correlation_id}")
            
            # Check if we got a response
            if len(response) > 4:
                # Topic count
                offset = 4
                topic_count = struct.unpack('>i', response[offset:offset+4])[0]
                print(f"Topics: {topic_count}")
                
                if topic_count == 0:
                    print("No topics in response - likely topic doesn't exist")
                else:
                    print("Got topic response!")
        
    except Exception as e:
        print(f"Error: {e}")
    finally:
        sock.close()

if __name__ == "__main__":
    test_produce()