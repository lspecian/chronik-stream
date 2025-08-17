#!/usr/bin/env python3
"""Simple test to check if produce handler stores data"""

import struct
import socket
import subprocess
import time

def encode_string(s):
    """Encode a string as Kafka protocol string (length + data)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def encode_bytes(b):
    """Encode bytes as Kafka protocol bytes (length + data)"""
    if b is None:
        return struct.pack('>i', -1)
    return struct.pack('>i', len(b)) + b

def create_magic_v0_messages(messages):
    """Create Kafka magic v0 messages (simpler format)"""
    # For v0, we just concatenate individual messages
    result = b''
    
    for key, value in messages:
        # Single message format (magic v0)
        msg = b''
        
        # CRC (we'll use 0 for simplicity)
        msg += struct.pack('>i', 0)
        
        # Magic byte
        msg += struct.pack('>b', 0)  # magic v0
        
        # Attributes
        msg += struct.pack('>b', 0)  # no compression
        
        # Key
        if key:
            key_bytes = key.encode('utf-8')
            msg += struct.pack('>i', len(key_bytes))
            msg += key_bytes
        else:
            msg += struct.pack('>i', -1)  # null key
        
        # Value
        if value:
            value_bytes = value.encode('utf-8')
            msg += struct.pack('>i', len(value_bytes))
            msg += value_bytes
        else:
            msg += struct.pack('>i', -1)  # null value
        
        # Message set entry: offset + message size + message
        entry = b''
        entry += struct.pack('>q', 0)  # offset (0 for produce)
        entry += struct.pack('>i', len(msg))  # message size
        entry += msg
        
        result += entry
    
    return result

# First, create topic
print("Creating topic 'simple-test'...")
subprocess.run(['python', 'test_create_topic.py'], capture_output=True)

# Send produce request with v0 format
print("\nSending produce request with magic v0 format...")

# Create messages
messages_data = create_magic_v0_messages([("key1", "Hello from v0")])

# Build produce request body (v0 API)
body = b''
body += struct.pack('>h', 1)  # acks
body += struct.pack('>i', 30000)  # timeout
body += struct.pack('>i', 1)  # topic count

# Topic
body += encode_string('test-topic')
body += struct.pack('>i', 1)  # partition count

# Partition
body += struct.pack('>i', 0)  # partition index
body += encode_bytes(messages_data)  # message set

# Request header
header = b''
header += struct.pack('>h', 0)  # API key (Produce)  
header += struct.pack('>h', 0)  # API version 0 (uses v0 message format)
header += struct.pack('>i', 999)  # Correlation ID
header += encode_string('simple-producer')  # Client ID

# Full request
request = header + body
request_with_length = struct.pack('>i', len(request)) + request

# Send request
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9092))
sock.send(request_with_length)

# Read response with timeout
sock.settimeout(5.0)
try:
    response_length_data = sock.recv(4)
    if len(response_length_data) == 4:
        response_length = struct.unpack('>i', response_length_data)[0]
        response_data = sock.recv(response_length)
        
        # Parse correlation ID
        correlation_id = struct.unpack('>i', response_data[:4])[0]
        print(f"Got response, correlation ID: {correlation_id}")
        
        # Very basic error checking - look for error codes
        if len(response_data) > 10:
            # Try to find error code (usually after topic name)
            for i in range(4, min(len(response_data)-2, 50), 2):
                potential_error = struct.unpack('>h', response_data[i:i+2])[0]
                if potential_error == 0:
                    print("✓ Success response detected")
                    break
                elif potential_error == 3:
                    print("✗ UNKNOWN_TOPIC_OR_PARTITION error")
                    break
    else:
        print("✗ Failed to read response length")
except socket.timeout:
    print("✗ Response timeout")
except Exception as e:
    print(f"✗ Error: {e}")
finally:
    sock.close()

# Check if data was written
print("\nChecking storage directory...")
time.sleep(1)
result = subprocess.run(['docker', 'exec', 'chronik-stream-ingest-1', 'find', '/home/chronik/data', '-name', '*.segment', '-o', '-name', '*.log'], 
                       capture_output=True, text=True)
if result.stdout.strip():
    print("✓ Found segment/log files:")
    print(result.stdout)
else:
    print("✗ No segment/log files found")

# Check recent logs
print("\nRecent logs:")
result = subprocess.run(['docker', 'logs', '--tail', '10', 'chronik-stream-ingest-1'], 
                       capture_output=True, text=True)
print(result.stdout)