import socket
import struct

# Connect to the server
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9092))

# Create a metadata request (API key 3, version 5)
api_key = 3
api_version = 5
correlation_id = 1
client_id = 'test-client'

# Build request
request = bytearray()
request.extend(struct.pack('>h', api_key))  # API key
request.extend(struct.pack('>h', api_version))  # API version
request.extend(struct.pack('>i', correlation_id))  # Correlation ID
request.extend(struct.pack('>h', len(client_id)))  # Client ID length
request.extend(client_id.encode('utf-8'))  # Client ID
request.extend(struct.pack('>i', -1))  # -1 for all topics
request.extend(struct.pack('>b', 1))  # allow_auto_topic_creation (bool)

# Send request with length prefix
length_prefix = struct.pack('>i', len(request))
sock.send(length_prefix + request)

# Read response
response_length_bytes = sock.recv(4)
response_length = struct.unpack('>i', response_length_bytes)[0]
print(f'Response length: {response_length}')

response = sock.recv(response_length)
print(f'Response bytes: {response.hex()}')

# Parse correlation ID from response
correlation_id_response = struct.unpack('>i', response[0:4])[0]
print(f'Correlation ID in response: {correlation_id_response}')

# Parse the rest of the response
offset = 4
throttle_time = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f'Throttle time: {throttle_time}ms')

# Brokers
broker_count = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f'Broker count: {broker_count}')

for i in range(broker_count):
    node_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    host_len = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    host = response[offset:offset+host_len].decode('utf-8')
    offset += host_len
    port = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    rack_len = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    if rack_len >= 0:
        rack = response[offset:offset+rack_len].decode('utf-8')
        offset += rack_len
    else:
        rack = None
    print(f'  Broker {node_id}: {host}:{port} (rack: {rack})')

# Cluster ID
cluster_id_len = struct.unpack('>h', response[offset:offset+2])[0]
offset += 2
if cluster_id_len >= 0:
    cluster_id = response[offset:offset+cluster_id_len].decode('utf-8')
    offset += cluster_id_len
else:
    cluster_id = None
print(f'Cluster ID: {cluster_id}')

# Controller ID
controller_id = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f'Controller ID: {controller_id}')

# Topics
topic_count = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f'Topic count: {topic_count}')

sock.close()
