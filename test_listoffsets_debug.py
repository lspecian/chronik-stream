#!/usr/bin/env python3
import socket
import struct
import time

def send_request(sock, request_bytes):
    # Send length prefix + request
    sock.send(struct.pack('>I', len(request_bytes)) + request_bytes)

    # Read response length
    length_bytes = sock.recv(4)
    if len(length_bytes) < 4:
        print(f"ERROR: Got only {len(length_bytes)} bytes for length")
        return None
    response_len = struct.unpack('>I', length_bytes)[0]

    # Read response
    response = b''
    while len(response) < response_len:
        chunk = sock.recv(min(4096, response_len - len(response)))
        if not chunk:
            break
        response += chunk

    return response

def encode_string(s):
    if s is None:
        return struct.pack('>h', -1)
    data = s.encode('utf-8')
    return struct.pack('>h', len(data)) + data

def main():
    # Connect to Chronik
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))

    # API Versions request first
    print("Sending ApiVersions request...")
    api_versions_req = struct.pack('>hhhh',
        18,  # API key (ApiVersions)
        0,   # API version
        1,   # Correlation ID
        0    # Client ID length (null)
    )

    response = send_request(sock, api_versions_req)
    if response:
        print(f"ApiVersions response: {len(response)} bytes")
        # Parse correlation ID
        correlation_id = struct.unpack('>i', response[:4])[0]
        print(f"  Correlation ID: {correlation_id}")

    # Send ListOffsets request for earliest offset
    print("\nSending ListOffsets request (API v0, timestamp=-2 for EARLIEST)...")

    # Build request body
    body = b''
    body += struct.pack('>i', -1)  # Replica ID (-1 for consumers)
    body += struct.pack('>i', 1)   # Topics array length
    body += encode_string('test-topic')  # Topic name
    body += struct.pack('>i', 1)   # Partitions array length
    body += struct.pack('>i', 0)   # Partition index
    body += struct.pack('>q', -2)  # Timestamp (-2 = EARLIEST)
    body += struct.pack('>i', 1)   # Max number of offsets (v0 only)

    # Build request header (v0)
    header = struct.pack('>hhi',
        2,   # API key (ListOffsets)
        0,   # API version
        2    # Correlation ID
    )
    header += struct.pack('>h', -1)  # Client ID (null)

    request = header + body
    print(f"Request size: {len(request)} bytes")
    print(f"Request hex: {request.hex()}")

    response = send_request(sock, request)
    if response:
        print(f"\nListOffsets response: {len(response)} bytes")
        print(f"Response hex: {response.hex()}")

        # Parse response
        pos = 0
        correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"  Correlation ID: {correlation_id}")

        # Topics array
        topic_count = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"  Topic count: {topic_count}")

        for i in range(topic_count):
            # Topic name
            name_len = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            if name_len > 0:
                topic_name = response[pos:pos+name_len].decode('utf-8')
                pos += name_len
            else:
                topic_name = None
            print(f"    Topic: {topic_name}")

            # Partitions array
            partition_count = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            print(f"    Partition count: {partition_count}")

            for j in range(partition_count):
                partition_index = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                error_code = struct.unpack('>h', response[pos:pos+2])[0]
                pos += 2

                # For v0, we get an array of offsets
                offset_count = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                print(f"      Partition {partition_index}: error={error_code}, offset_count={offset_count}")

                for k in range(offset_count):
                    offset = struct.unpack('>q', response[pos:pos+8])[0]
                    pos += 8
                    print(f"        Offset[{k}]: {offset}")

    # Now send ListOffsets v1 request
    print("\n\nSending ListOffsets request (API v1, timestamp=-2 for EARLIEST)...")

    # Build request body for v1
    body = b''
    body += struct.pack('>i', -1)  # Replica ID
    body += struct.pack('>i', 1)   # Topics array length
    body += encode_string('test-topic')  # Topic name
    body += struct.pack('>i', 1)   # Partitions array length
    body += struct.pack('>i', 0)   # Partition index
    body += struct.pack('>q', -2)  # Timestamp (-2 = EARLIEST)

    # Build request header (v1)
    header = struct.pack('>hhi',
        2,   # API key (ListOffsets)
        1,   # API version (v1 this time)
        3    # Correlation ID
    )
    header += struct.pack('>h', -1)  # Client ID (null)

    request = header + body
    print(f"Request size: {len(request)} bytes")

    response = send_request(sock, request)
    if response:
        print(f"\nListOffsets v1 response: {len(response)} bytes")

        # Parse v1 response
        pos = 0
        correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"  Correlation ID: {correlation_id}")

        # Topics array
        topic_count = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"  Topic count: {topic_count}")

        for i in range(topic_count):
            # Topic name
            name_len = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            if name_len > 0:
                topic_name = response[pos:pos+name_len].decode('utf-8')
                pos += name_len
            else:
                topic_name = None
            print(f"    Topic: {topic_name}")

            # Partitions array
            partition_count = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            print(f"    Partition count: {partition_count}")

            for j in range(partition_count):
                partition_index = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                error_code = struct.unpack('>h', response[pos:pos+2])[0]
                pos += 2
                timestamp = struct.unpack('>q', response[pos:pos+8])[0]
                pos += 8
                offset = struct.unpack('>q', response[pos:pos+8])[0]
                pos += 8
                print(f"      Partition {partition_index}: error={error_code}, timestamp={timestamp}, offset={offset}")

    sock.close()

if __name__ == '__main__':
    main()