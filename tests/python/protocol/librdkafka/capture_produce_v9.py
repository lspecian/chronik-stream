#!/usr/bin/env python3
"""Capture raw Produce v9 request from librdkafka"""

import socket
import struct
import sys

def read_exact(sock, n):
    """Read exactly n bytes from socket"""
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise EOFError("Connection closed")
        data += chunk
    return data

def main():
    # Listen on port 9092
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('0.0.0.0', 9092))
    server.listen(1)
    
    print("Waiting for connection on port 9092...")
    conn, addr = server.accept()
    print(f"Connection from {addr}")
    
    try:
        while True:
            # Read request size
            size_bytes = read_exact(conn, 4)
            size = struct.unpack('>I', size_bytes)[0]
            print(f"\nRequest size: {size}")
            
            # Read request
            request = read_exact(conn, size)
            
            # Parse header
            api_key = struct.unpack('>h', request[0:2])[0]
            api_version = struct.unpack('>h', request[2:4])[0]
            correlation_id = struct.unpack('>i', request[4:8])[0]
            
            print(f"API Key: {api_key}, Version: {api_version}, Correlation ID: {correlation_id}")
            
            # Look for Produce v9
            if api_key == 0 and api_version == 9:
                print("PRODUCE V9 REQUEST FOUND!")
                
                # Client ID (normal string in librdkafka)
                client_id_len = struct.unpack('>h', request[8:10])[0]
                if client_id_len > 0:
                    client_id = request[10:10+client_id_len].decode('utf-8')
                    body_start = 10 + client_id_len
                else:
                    client_id = None
                    body_start = 10
                
                print(f"Client ID: {client_id}")
                print(f"Body starts at byte {body_start}")
                
                # Dump the body
                body = request[body_start:]
                print(f"Body length: {len(body)} bytes")
                print("Body hex dump:")
                for i in range(0, min(len(body), 200), 16):
                    hex_str = ' '.join(f'{b:02x}' for b in body[i:i+16])
                    ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in body[i:i+16])
                    print(f"  {i:04x}: {hex_str:<48} {ascii_str}")
                
                # Try to parse the body
                pos = 0
                print("\nParsing body:")
                
                # Check if transactional_id is present
                if len(body) >= 2:
                    tid_len = struct.unpack('>h', body[0:2])[0]
                    print(f"  First i16 value: {tid_len} (0x{tid_len & 0xFFFF:04x})")
                    if tid_len == -1:
                        print("  -> Null transactional_id")
                        pos = 2
                    elif tid_len == 0:
                        print("  -> Empty transactional_id")
                        pos = 2
                    elif 0 < tid_len < 100:
                        tid = body[2:2+tid_len].decode('utf-8', errors='replace')
                        print(f"  -> Transactional ID: '{tid}'")
                        pos = 2 + tid_len
                    else:
                        print(f"  -> Invalid length, might not be transactional_id")
                        # Maybe there's no transactional_id?
                        pos = 0
                
                # Try to read acks
                if pos + 2 <= len(body):
                    acks = struct.unpack('>h', body[pos:pos+2])[0]
                    print(f"  Acks at position {pos}: {acks}")
                    pos += 2
                
                # Try to read timeout
                if pos + 4 <= len(body):
                    timeout = struct.unpack('>i', body[pos:pos+4])[0]
                    print(f"  Timeout at position {pos}: {timeout}ms")
                    pos += 4
                
                # Save to file
                with open('produce_v9_request.bin', 'wb') as f:
                    f.write(request)
                print("\nSaved to produce_v9_request.bin")
                
            # Send a simple response (ApiVersions or error)
            if api_key == 18:  # ApiVersions
                # Send supported versions response
                response = struct.pack('>i', correlation_id)  # correlation_id
                response += struct.pack('>h', 0)  # error_code
                response += struct.pack('>i', 1)  # array count
                response += struct.pack('>hhh', 0, 0, 9)  # Produce v0-9
                response += struct.pack('>i', 0)  # throttle_time
                
                response_with_size = struct.pack('>i', len(response)) + response
                conn.send(response_with_size)
            else:
                # Send error response
                response = struct.pack('>i', correlation_id)
                response += struct.pack('>h', 35)  # unsupported version
                
                response_with_size = struct.pack('>i', len(response)) + response
                conn.send(response_with_size)
                
    except Exception as e:
        print(f"Error: {e}")
    finally:
        conn.close()
        server.close()

if __name__ == "__main__":
    main()