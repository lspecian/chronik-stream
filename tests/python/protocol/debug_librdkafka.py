#!/usr/bin/env python3
"""
Debug proxy for capturing librdkafka/confluent-kafka-go protocol interactions.
This helps identify the exact bytes causing memory corruption.
"""

import socket
import struct
import threading
import time
import sys
from datetime import datetime

class KafkaProtocolProxy:
    def __init__(self, listen_port=9093, server_port=9092):
        self.listen_port = listen_port
        self.server_port = server_port
        self.api_names = {
            0: 'Produce', 1: 'Fetch', 2: 'ListOffsets', 3: 'Metadata',
            8: 'OffsetCommit', 9: 'OffsetFetch', 10: 'FindCoordinator',
            11: 'JoinGroup', 12: 'Heartbeat', 13: 'LeaveGroup', 14: 'SyncGroup',
            15: 'DescribeGroups', 16: 'ListGroups', 17: 'SaslHandshake',
            18: 'ApiVersions', 19: 'CreateTopics', 20: 'DeleteTopics',
            32: 'DescribeConfigs', 33: 'AlterConfigs'
        }
        
    def hex_dump(self, data, prefix="", max_bytes=256):
        """Create a hex dump of the data."""
        result = []
        for i in range(0, min(len(data), max_bytes), 16):
            hex_str = ' '.join(f'{b:02x}' for b in data[i:i+16])
            ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in data[i:i+16])
            result.append(f"{prefix}{i:04x}: {hex_str:<48} {ascii_str}")
        if len(data) > max_bytes:
            result.append(f"{prefix}... ({len(data) - max_bytes} more bytes)")
        return '\n'.join(result)
    
    def parse_request_header(self, data):
        """Parse Kafka request header."""
        if len(data) < 8:
            return None
        
        api_key = struct.unpack('>h', data[0:2])[0]
        api_version = struct.unpack('>h', data[2:4])[0]
        correlation_id = struct.unpack('>i', data[4:8])[0]
        
        # Try to parse client ID
        offset = 8
        client_id = "unknown"
        if len(data) > offset + 2:
            client_id_len = struct.unpack('>h', data[offset:offset+2])[0]
            offset += 2
            if client_id_len > 0 and len(data) >= offset + client_id_len:
                client_id = data[offset:offset+client_id_len].decode('utf-8', errors='replace')
        
        return {
            'api_key': api_key,
            'api_name': self.api_names.get(api_key, f'Unknown({api_key})'),
            'api_version': api_version,
            'correlation_id': correlation_id,
            'client_id': client_id
        }
    
    def parse_response_header(self, data, is_v1_plus=False):
        """Parse Kafka response header."""
        if len(data) < 4:
            return None
        
        correlation_id = struct.unpack('>i', data[0:4])[0]
        
        # For ApiVersions v1+, there might be additional fields
        offset = 4
        throttle_time_ms = None
        if is_v1_plus and len(data) >= 8:
            # Some responses have throttle_time_ms
            throttle_time_ms = struct.unpack('>i', data[4:8])[0]
            offset = 8
        
        return {
            'correlation_id': correlation_id,
            'throttle_time_ms': throttle_time_ms,
            'offset': offset
        }
    
    def handle_connection(self, client_sock, addr):
        """Handle a client connection."""
        print(f"\n{'='*80}")
        print(f"[{datetime.now().strftime('%H:%M:%S.%f')}] New connection from {addr}")
        print(f"{'='*80}")
        
        server_sock = None
        try:
            # Connect to chronik server
            server_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            server_sock.connect(('localhost', self.server_port))
            print(f"Connected to chronik server on port {self.server_port}")
            
            request_count = 0
            while True:
                # Read request length (4 bytes)
                length_data = client_sock.recv(4)
                if not length_data:
                    print(f"Client disconnected")
                    break
                
                if len(length_data) < 4:
                    print(f"Incomplete length data: {len(length_data)} bytes")
                    break
                
                request_length = struct.unpack('>i', length_data)[0]
                request_count += 1
                
                print(f"\n{'='*60}")
                print(f"REQUEST #{request_count} - Length: {request_length} bytes")
                
                # Read request body
                request = b''
                while len(request) < request_length:
                    chunk = client_sock.recv(min(4096, request_length - len(request)))
                    if not chunk:
                        break
                    request += chunk
                
                # Parse and display request
                req_info = self.parse_request_header(request)
                if req_info:
                    print(f"API: {req_info['api_name']} (key={req_info['api_key']}, version={req_info['api_version']})")
                    print(f"Correlation ID: {req_info['correlation_id']}")
                    print(f"Client ID: {req_info['client_id']}")
                    
                    # Special logging for ApiVersions
                    if req_info['api_key'] == 18:
                        print("*** ApiVersions Request - Critical for librdkafka ***")
                
                print("\nRequest hex dump:")
                print(self.hex_dump(request, "  REQ: "))
                
                # Forward to server
                server_sock.sendall(length_data + request)
                
                # Read response length
                resp_length_data = server_sock.recv(4)
                if not resp_length_data:
                    print("ERROR: No response from server")
                    break
                
                resp_length = struct.unpack('>i', resp_length_data)[0]
                print(f"\nRESPONSE - Length: {resp_length} bytes")
                
                # Read response body
                response = b''
                while len(response) < resp_length:
                    chunk = server_sock.recv(min(4096, resp_length - len(response)))
                    if not chunk:
                        break
                    response += chunk
                
                # Parse response header
                is_api_versions = req_info and req_info['api_key'] == 18
                is_v1_plus = req_info and req_info['api_version'] >= 1
                
                resp_info = self.parse_response_header(response, is_v1_plus and is_api_versions)
                if resp_info:
                    print(f"Response Correlation ID: {resp_info['correlation_id']}")
                    if resp_info['correlation_id'] != req_info['correlation_id']:
                        print(f"WARNING: Correlation ID mismatch! Request: {req_info['correlation_id']}, Response: {resp_info['correlation_id']}")
                    if resp_info['throttle_time_ms'] is not None:
                        print(f"Throttle Time: {resp_info['throttle_time_ms']}ms")
                
                print("\nResponse hex dump:")
                print(self.hex_dump(response, "  RESP: "))
                
                # Check for potential issues
                if is_api_versions:
                    print("\n*** Analyzing ApiVersions Response ***")
                    if len(response) < 8:
                        print("ERROR: Response too short for ApiVersions")
                    else:
                        # Check structure based on version
                        if req_info['api_version'] == 3:
                            # v3 should NOT have throttle_time_ms
                            print("ApiVersions v3 format check:")
                            print(f"  - First 4 bytes (correlation_id): {response[0:4].hex()}")
                            print(f"  - Next byte (tagged fields): {response[4:5].hex()}")
                            if response[4] != 0:
                                print(f"  WARNING: Tagged fields not 0: {response[4]}")
                            print(f"  - Next 2 bytes (error_code): {response[5:7].hex()}")
                            error_code = struct.unpack('>h', response[5:7])[0]
                            print(f"  - Error code value: {error_code}")
                
                # Forward response to client
                try:
                    client_sock.sendall(resp_length_data + response)
                    print(f"Forwarded {len(response)} bytes to client")
                except Exception as e:
                    print(f"ERROR forwarding to client: {e}")
                    # This might be where librdkafka crashes
                    print("*** CLIENT MAY HAVE CRASHED AT THIS POINT ***")
                    break
                
        except Exception as e:
            print(f"ERROR: {e}")
            import traceback
            traceback.print_exc()
        finally:
            if server_sock:
                server_sock.close()
            client_sock.close()
            print(f"Connection closed")
    
    def start(self):
        """Start the proxy server."""
        server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(('0.0.0.0', self.listen_port))
        server.listen(5)
        
        print(f"Kafka Protocol Debug Proxy")
        print(f"Listening on port {self.listen_port}")
        print(f"Forwarding to chronik on port {self.server_port}")
        print(f"Configure your client to connect to localhost:{self.listen_port}")
        print("")
        
        try:
            while True:
                client_sock, addr = server.accept()
                thread = threading.Thread(target=self.handle_connection, args=(client_sock, addr))
                thread.daemon = True
                thread.start()
        except KeyboardInterrupt:
            print("\nShutting down proxy...")
        finally:
            server.close()

if __name__ == "__main__":
    proxy = KafkaProtocolProxy(listen_port=9093, server_port=9092)
    proxy.start()