import socket
import struct

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9092))

# Send API versions v0 request  
request = b''
request += struct.pack('>h', 18)  # API Key (ApiVersions)
request += struct.pack('>h', 0)   # API Version 0
request += struct.pack('>i', 100) # Correlation ID
request += struct.pack('>h', 8)   # Client ID length
request += b'test-cli'            # Client ID

# Send
length_prefix = struct.pack('>i', len(request))
sock.sendall(length_prefix + request)
print(f"Sent API versions v0 request")

# Read response
length_data = sock.recv(4)
response_length = struct.unpack('>i', length_data)[0]
print(f"Response length: {response_length}")

response = sock.recv(response_length)
print(f"Response first 20 bytes hex: {response[:20].hex()}")

# Parse
corr_id = struct.unpack('>i', response[0:4])[0]
error_code = struct.unpack('>h', response[4:6])[0]
api_count = struct.unpack('>i', response[6:10])[0]
print(f"Correlation ID = {corr_id}, Error = {error_code}, API count = {api_count}")

sock.close()