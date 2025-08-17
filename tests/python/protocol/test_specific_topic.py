import socket
import struct

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9092))

# Send metadata v1 request for specific topic
request = b''
request += struct.pack('>h', 3)   # API Key (Metadata)
request += struct.pack('>h', 1)   # API Version 1
request += struct.pack('>i', 300) # Correlation ID
request += struct.pack('>h', 8)   # Client ID length
request += b'test-cli'            # Client ID
request += struct.pack('>i', 1)   # Topics count: 1
request += struct.pack('>h', 9)   # Topic name length
request += b'test-topic'          # Topic name

# Send
length_prefix = struct.pack('>i', len(request))
sock.sendall(length_prefix + request)
print(f"Sent metadata v1 request for specific topic")

# Read response
length_data = sock.recv(4)
response_length = struct.unpack('>i', length_data)[0]
print(f"Response length: {response_length}")

response = sock.recv(response_length)
print(f"Response first 32 bytes hex: {response[:32].hex()}")

# Parse
offset = 0
corr_id = struct.unpack('>i', response[offset:offset+4])[0]
print(f"Correlation ID = {corr_id}")
offset += 4

val1 = struct.unpack('>i', response[offset:offset+4])[0]
print(f"Offset 4: {val1} (should be broker count for v1)")
offset += 4

if val1 in [0, 1]:
    val2 = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"Offset 8: {val2}")

sock.close()