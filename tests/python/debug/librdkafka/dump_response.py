#!/usr/bin/env python3
import socket
import struct

# Send Metadata v12 request
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9092))

# Build Metadata v12 request
request = bytearray()
request.extend(struct.pack('>h', 3))  # API key: Metadata
request.extend(struct.pack('>h', 12))  # API version: 12
request.extend(struct.pack('>i', 3))  # Correlation ID: 3
request.extend(struct.pack('>h', 11))  # Client ID: normal string "test-client" (length 11)
request.extend(b'test-client')
request.append(0x00)  # Tagged fields: empty
request.append(0x00)  # Topics: null (-1 as varint = 0)
request.append(0x00)  # Include cluster authorized operations: false
request.append(0x01)  # Include topic authorized operations: true
request.append(0x00)  # Tagged fields: empty

# Send with length prefix
full_request = struct.pack('>i', len(request)) + request
sock.send(full_request)

# Read response
length_bytes = sock.recv(4)
response_length = struct.unpack('>i', length_bytes)[0]
response = sock.recv(response_length)

print(f"Response length: {response_length} bytes")
print(f"\nFirst 100 bytes:")
for i in range(0, min(100, len(response)), 16):
    hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
    ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in response[i:i+16])
    print(f"{i:04x}: {hex_str:<48} {ascii_str}")

print(f"\nByte at position 30: 0x{response[30]:02x} ({response[30]}) = '{chr(response[30]) if 32 <= response[30] < 127 else '.'}'")

# Parse the response structure
pos = 0
corr_id = struct.unpack('>i', response[pos:pos+4])[0]
pos += 4
print(f"\nCorrelation ID: {corr_id}")

throttle = struct.unpack('>i', response[pos:pos+4])[0]
pos += 4
print(f"Throttle time: {throttle}")

# Read broker array
broker_count_byte = response[pos]
print(f"\nBroker count byte: 0x{broker_count_byte:02x} (should be 0x02 for 1 broker)")
pos += 1

if broker_count_byte == 0x02:  # Correct compact varint for 1 broker
    # Read first broker
    broker_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    print(f"Broker ID: {broker_id}")
    
    # Read host (compact string)
    host_len = response[pos]
    pos += 1
    if host_len > 0:
        actual_len = host_len - 1
        host = response[pos:pos+actual_len].decode('utf-8')
        pos += actual_len
        print(f"Host: {host}")
    
    # Read port
    port = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    print(f"Port: {port}")
    
    # Read rack (compact string, nullable)
    rack_len = response[pos]
    pos += 1
    if rack_len == 0:
        print("Rack: null")
    
    # Tagged fields
    tagged = response[pos]
    pos += 1
    print(f"Broker tagged fields: 0x{tagged:02x}")

print(f"\nCurrent position: {pos}")
print(f"Bytes around position 30: {response[28:33].hex()}")

sock.close()