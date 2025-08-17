#!/usr/bin/env python3
"""Debug protocol interaction between kafkactl and chronik."""

import socket
import struct
import threading
import time

def parse_kafka_request(data):
    """Parse a Kafka request and return details."""
    if len(data) < 8:
        return "Incomplete request header"
    
    api_key = struct.unpack('>h', data[0:2])[0]
    api_version = struct.unpack('>h', data[2:4])[0]
    correlation_id = struct.unpack('>i', data[4:8])[0]
    
    # Client ID
    offset = 8
    client_id_len = struct.unpack('>h', data[offset:offset+2])[0]
    offset += 2
    
    client_id = None
    if client_id_len > 0:
        client_id = data[offset:offset+client_id_len].decode('utf-8', errors='replace')
        offset += client_id_len
    
    return {
        'api_key': api_key,
        'api_version': api_version,
        'correlation_id': correlation_id,
        'client_id': client_id,
        'remaining_bytes': len(data) - offset,
        'offset': offset
    }

def handle_connection(client_sock, addr):
    """Handle a client connection."""
    print(f"\n=== New connection from {addr} ===")
    
    try:
        # Connect to chronik
        chronik_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        chronik_sock.connect(('localhost', 9092))
        
        while True:
            # Read request length
            length_data = client_sock.recv(4)
            if not length_data:
                break
                
            request_length = struct.unpack('>i', length_data)[0]
            print(f"\nRequest length: {request_length} bytes")
            
            # Read request
            request = b''
            while len(request) < request_length:
                chunk = client_sock.recv(min(4096, request_length - len(request)))
                if not chunk:
                    break
                request += chunk
            
            # Parse and display request
            req_info = parse_kafka_request(request)
            print(f"Request: API={req_info['api_key']} (", end='')
            api_names = {0: 'Produce', 1: 'Fetch', 2: 'ListOffsets', 3: 'Metadata', 
                        8: 'OffsetCommit', 9: 'OffsetFetch', 10: 'FindCoordinator',
                        11: 'JoinGroup', 12: 'Heartbeat', 13: 'LeaveGroup', 14: 'SyncGroup',
                        15: 'DescribeGroups', 16: 'ListGroups', 17: 'SaslHandshake',
                        18: 'ApiVersions', 19: 'CreateTopics', 20: 'DeleteTopics'}
            print(f"{api_names.get(req_info['api_key'], 'Unknown')}) ", end='')
            print(f"Version={req_info['api_version']} CorrelationID={req_info['correlation_id']}")
            print(f"ClientID: {req_info['client_id']}")
            
            # Special handling for ApiVersions request
            if req_info['api_key'] == 18:  # ApiVersions
                print("*** ApiVersions request detected - this is often the first request ***")
            
            # Forward to chronik
            chronik_sock.sendall(length_data + request)
            
            # Read response length from chronik
            resp_length_data = chronik_sock.recv(4)
            if not resp_length_data:
                print("No response from chronik")
                break
                
            resp_length = struct.unpack('>i', resp_length_data)[0]
            print(f"Response length: {resp_length} bytes")
            
            # Read response
            response = b''
            while len(response) < resp_length:
                chunk = chronik_sock.recv(min(4096, resp_length - len(response)))
                if not chunk:
                    break
                response += chunk
            
            print(f"Response received: {len(response)} bytes")
            
            # Forward response to client
            client_sock.sendall(resp_length_data + response)
            
    except Exception as e:
        print(f"Error: {e}")
    finally:
        client_sock.close()
        chronik_sock.close()

def main():
    """Run the debug proxy."""
    server_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server_sock.bind(('0.0.0.0', 9094))
    server_sock.listen(5)
    
    print("Debug proxy listening on port 9094")
    print("Configure kafkactl to connect to localhost:9094")
    
    try:
        while True:
            client_sock, addr = server_sock.accept()
            thread = threading.Thread(target=handle_connection, args=(client_sock, addr))
            thread.daemon = True
            thread.start()
    except KeyboardInterrupt:
        print("\nShutting down...")
    finally:
        server_sock.close()

if __name__ == "__main__":
    main()