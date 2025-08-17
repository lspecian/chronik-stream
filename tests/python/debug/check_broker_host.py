#!/usr/bin/env python3
"""Check which handler is being used by looking at broker host."""

import socket
import struct

def check_broker_host():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send metadata request
    request = b''
    request += struct.pack('>h', 3)      # Metadata
    request += struct.pack('>h', 1)      # Version 1
    request += struct.pack('>i', 5555)   # Correlation ID
    request += struct.pack('>h', -1)     # Client ID null
    request += struct.pack('>i', -1)     # Topics all
    
    full_request = struct.pack('>I', len(request)) + request
    sock.send(full_request)
    
    # Read response
    resp_len = struct.unpack('>I', sock.recv(4))[0]
    response = sock.recv(resp_len)
    
    # Find the host string
    # It should be at a specific offset
    # Let's search for it
    if b'localhost' in response:
        print("Found 'localhost' - this means BASE HANDLER is being used!")
    elif b'0.0.0.0' in response:
        print("Found '0.0.0.0' - this means KAFKA HANDLER is being used!")
    else:
        print("Neither host found!")
        
    # Also decode to verify
    print(f"\nRaw response ({len(response)} bytes):")
    print(f"Hex: {response.hex()}")
    
    sock.close()

check_broker_host()