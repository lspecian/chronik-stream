#!/usr/bin/env python3
import socket
import struct

def test_metadata_v5():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    # Build Metadata v5 request
    # API Key: 3 (Metadata)
    # API Version: 5
    # Correlation ID: 555
    # Client ID: "test"
    # Topics: null (all topics)
    # allow_auto_topic_creation: true

    request = bytearray()
    request.extend(struct.pack('>h', 3))      # API Key
    request.extend(struct.pack('>h', 5))      # API Version
    request.extend(struct.pack('>i', 555))    # Correlation ID
    request.extend(struct.pack('>h', 4))      # Client ID length
    request.extend(b'test')                   # Client ID
    request.extend(struct.pack('>i', -1))     # Topics: -1 = null (all topics)
    request.extend(struct.pack('>b', 1))      # allow_auto_topic_creation: true

    # Send with size prefix
    size = len(request)
    sock.send(struct.pack('>i', size))
    sock.send(request)

    # Read response
    size_bytes = sock.recv(4)
    response_size = struct.unpack('>i', size_bytes)[0]
    print(f"Response size: {response_size}")

    response = bytearray()
    while len(response) < response_size:
        chunk = sock.recv(min(4096, response_size - len(response)))
        if not chunk:
            break
        response.extend(chunk)

    print(f"Received {len(response)} bytes")
    print(f"Response hex: {response.hex()}")

    # Try to parse it
    offset = 0
    correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id}")

    # v5 should have throttle_time_ms
    throttle_time = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Throttle time: {throttle_time}ms")

    # Brokers array
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Broker count: {broker_count}")

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

        print(f"  Broker {i}: node_id={node_id}, host={host}, port={port}, rack={rack}")

    # cluster_id (nullable string)
    cluster_id_len = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    if cluster_id_len >= 0:
        cluster_id = response[offset:offset+cluster_id_len].decode('utf-8')
        offset += cluster_id_len
    else:
        cluster_id = None
    print(f"Cluster ID: {cluster_id}")

    # controller_id
    controller_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Controller ID: {controller_id}")

    print(f"\nParsed {offset} bytes out of {len(response)}")
    print(f"Remaining bytes at offset {offset}: {response[offset:offset+20].hex()}")

    sock.close()

if __name__ == "__main__":
    test_metadata_v5()