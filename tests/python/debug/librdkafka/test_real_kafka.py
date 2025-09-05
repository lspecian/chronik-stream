#!/usr/bin/env python3
"""Test against real Kafka to see ApiVersions v3 response format"""

import socket
import struct
import binascii

def test_real_kafka():
    # This will fail if no Kafka is running, but we'll capture the response format
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        
        # Try common Kafka ports
        for port in [9092, 9093, 29092]:
            try:
                print(f"Trying port {port}...")
                sock.connect(('localhost', port))
                print(f"Connected to port {port}")
                break
            except:
                continue
        else:
            print("No Kafka broker found on common ports")
            # Let's just document what the response SHOULD be
            print("\nExpected ApiVersions v3 response structure:")
            print("1. Size (INT32)")
            print("2. Correlation ID (INT32)")
            print("3. Error Code (INT16)")
            print("4. API count (COMPACT_VARINT)")
            print("5. For each API:")
            print("   - API key (INT16)")
            print("   - Min version (INT8)")  # <-- INT8 for v3+
            print("   - Max version (INT8)")  # <-- INT8 for v3+
            print("   - Tagged fields (VARINT)")
            print("6. Throttle time ms (INT32)")  # <-- At position after APIs
            print("7. Tagged fields (VARINT)")
            return
            
        # Build ApiVersions v3 request (matching librdkafka)
        request = bytearray()
        request.extend((0).to_bytes(4, 'big'))  # Size placeholder
        request.extend((18).to_bytes(2, 'big'))  # API key
        request.extend((3).to_bytes(2, 'big'))   # API version
        request.extend((1).to_bytes(4, 'big'))   # Correlation ID
        request.extend(b'\x00\x0f')              # Client ID length
        request.extend(b'test-librdkafka')       # Client ID
        # Client software name (compact string)
        request.append(19)  # length + 1
        request.extend(b'confluent-kafka-go')
        # Client software version (compact string)
        request.append(7)   # length + 1
        request.extend(b'2.11.1')
        request.append(0)   # Tagged fields
        
        # Update size
        size = len(request) - 4
        request[0:4] = size.to_bytes(4, 'big')
        
        sock.send(request)
        
        # Read response
        response_size_bytes = sock.recv(4)
        response_size = int.from_bytes(response_size_bytes, 'big')
        response = sock.recv(response_size)
        
        print(f"\nResponse size: {response_size}")
        print(f"Response hex: {binascii.hexlify(response).decode()}")
        
        # Parse response
        pos = 0
        correlation_id = int.from_bytes(response[pos:pos+4], 'big')
        pos += 4
        print(f"Correlation ID: {correlation_id}")
        
        error_code = int.from_bytes(response[pos:pos+2], 'big', signed=True)
        pos += 2
        print(f"Error code: {error_code}")
        
        if error_code == 0:
            # Parse API count (compact varint)
            api_count_byte = response[pos]
            api_count = (api_count_byte - 1) if api_count_byte > 0 else 0
            pos += 1
            print(f"API count: {api_count}")
            
            # Check first API encoding
            if api_count > 0:
                api_key = int.from_bytes(response[pos:pos+2], 'big')
                pos += 2
                print(f"First API key: {api_key}")
                
                # Check if INT8 or INT16
                next_byte = response[pos]
                next_two = int.from_bytes(response[pos:pos+2], 'big')
                print(f"Next byte: 0x{next_byte:02x} ({next_byte})")
                print(f"Next two bytes: 0x{next_two:04x} ({next_two})")
                
                # If INT8, next byte should be reasonable version (0-15)
                # If INT16, we'd see 0x00 followed by version
                if next_byte == 0:
                    print("Looks like INT16 encoding (leading zero)")
                elif next_byte < 16:
                    print("Looks like INT8 encoding (reasonable version number)")
                else:
                    print("Unclear encoding")
        
        sock.close()
        
    except Exception as e:
        print(f"Error: {e}")

if __name__ == "__main__":
    test_real_kafka()