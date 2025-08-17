#!/usr/bin/env python3
"""
Capture the raw bytes of a Produce request from kafkactl to debug the panic issue.
"""

import socket
import struct
import sys
from datetime import datetime

def capture_kafka_requests(port=9092):
    """Listen on the Kafka port and capture raw request data."""
    
    # Create a TCP socket
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    
    try:
        server.bind(('0.0.0.0', port))
        server.listen(1)
        print(f"Listening on port {port}...")
        
        while True:
            client, address = server.accept()
            print(f"\nConnection from {address}")
            
            try:
                # Read the request
                data = client.recv(65536)
                
                if len(data) >= 4:
                    # Parse the length prefix
                    length = struct.unpack('>I', data[:4])[0]
                    print(f"Request length: {length} bytes")
                    
                    # Make sure we have the full request
                    full_data = data
                    while len(full_data) < length + 4:
                        more = client.recv(65536)
                        if not more:
                            break
                        full_data += more
                    
                    # Parse the request header
                    if len(full_data) >= 8:
                        api_key = struct.unpack('>h', full_data[4:6])[0]
                        api_version = struct.unpack('>h', full_data[6:8])[0]
                        correlation_id = struct.unpack('>i', full_data[8:12])[0]
                        
                        print(f"API Key: {api_key}")
                        print(f"API Version: {api_version}")
                        print(f"Correlation ID: {correlation_id}")
                        
                        # Save produce requests
                        if api_key == 0:  # Produce request
                            filename = f"produce_request_{datetime.now().strftime('%Y%m%d_%H%M%S')}.bin"
                            with open(filename, 'wb') as f:
                                f.write(full_data)
                            print(f"Saved produce request to {filename}")
                            
                            # Also save just the records data for analysis
                            # Skip the request header to get to the topic data
                            offset = 12  # After correlation ID
                            
                            # Skip client ID (nullable string)
                            if full_data[offset:offset+2] == b'\xff\xff':
                                offset += 2
                            else:
                                str_len = struct.unpack('>h', full_data[offset:offset+2])[0]
                                offset += 2 + str_len
                            
                            # For produce v3+, there's transactional ID
                            if api_version >= 3:
                                # Skip transactional ID (nullable string)  
                                if full_data[offset:offset+2] == b'\xff\xff':
                                    offset += 2
                                else:
                                    str_len = struct.unpack('>h', full_data[offset:offset+2])[0]
                                    offset += 2 + str_len
                            
                            # Skip acks (2 bytes) and timeout (4 bytes)
                            offset += 6
                            
                            # Topic array
                            topic_count = struct.unpack('>i', full_data[offset:offset+4])[0]
                            offset += 4
                            
                            print(f"Topic count: {topic_count}")
                            
                            for i in range(topic_count):
                                # Topic name
                                name_len = struct.unpack('>h', full_data[offset:offset+2])[0]
                                offset += 2
                                topic_name = full_data[offset:offset+name_len].decode('utf-8')
                                offset += name_len
                                print(f"Topic: {topic_name}")
                                
                                # Partition array
                                partition_count = struct.unpack('>i', full_data[offset:offset+4])[0]
                                offset += 4
                                print(f"Partition count: {partition_count}")
                                
                                for j in range(partition_count):
                                    # Partition index
                                    partition_idx = struct.unpack('>i', full_data[offset:offset+4])[0]
                                    offset += 4
                                    print(f"  Partition: {partition_idx}")
                                    
                                    # Records bytes length
                                    records_len = struct.unpack('>i', full_data[offset:offset+4])[0]
                                    offset += 4
                                    print(f"  Records length: {records_len}")
                                    
                                    if records_len > 0:
                                        # Extract the records bytes
                                        records_data = full_data[offset:offset+records_len]
                                        
                                        # Save the records data
                                        records_filename = f"records_data_{topic_name}_p{partition_idx}_{datetime.now().strftime('%Y%m%d_%H%M%S')}.bin"
                                        with open(records_filename, 'wb') as f:
                                            f.write(records_data)
                                        print(f"  Saved records to {records_filename}")
                                        
                                        # Print first few bytes in hex
                                        print(f"  First 64 bytes of records: {records_data[:64].hex()}")
                                        
                                        # Try to parse record batch header
                                        if len(records_data) >= 61:
                                            base_offset = struct.unpack('>q', records_data[0:8])[0]
                                            batch_length = struct.unpack('>i', records_data[8:12])[0]
                                            magic = records_data[16]
                                            print(f"  Base offset: {base_offset}")
                                            print(f"  Batch length: {batch_length}")
                                            print(f"  Magic: {magic}")
                                        
                                        offset += records_len
                        
                        # Send a basic response to prevent client timeout
                        # This is a minimal response that indicates an error
                        response = struct.pack('>i', correlation_id)  # Correlation ID
                        response += struct.pack('>i', 1)  # Topic array count
                        response += struct.pack('>h', len(b'test'))  # Topic name length
                        response += b'test'  # Topic name
                        response += struct.pack('>i', 1)  # Partition array count
                        response += struct.pack('>i', 0)  # Partition index
                        response += struct.pack('>h', 3)  # Error code (UNKNOWN_TOPIC_OR_PARTITION)
                        response += struct.pack('>q', -1)  # Base offset
                        
                        if api_version >= 2:
                            response += struct.pack('>q', -1)  # Log append time
                        if api_version >= 5:
                            response += struct.pack('>i', -1)  # Log start offset
                        
                        # Send response with length prefix
                        full_response = struct.pack('>i', len(response)) + response
                        client.send(full_response)
                    
                else:
                    print("Received incomplete data")
                    
            except Exception as e:
                print(f"Error handling client: {e}")
                import traceback
                traceback.print_exc()
            finally:
                client.close()
                
    except KeyboardInterrupt:
        print("\nShutting down...")
    finally:
        server.close()

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9092
    capture_kafka_requests(port)