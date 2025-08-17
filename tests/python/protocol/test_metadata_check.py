#!/usr/bin/env python3
"""Test metadata API to verify topic creation"""

import struct
import socket

def encode_string(s):
    """Encode a string as Kafka protocol string (length + data)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def send_metadata_request(host, port, topics=None):
    """Send a Metadata request"""
    
    # Build metadata request body
    body = b''
    
    # API version 9 fields
    if topics is None:
        # Request all topics
        body += struct.pack('>b', 0)  # topics = null (request all)
    else:
        body += struct.pack('>i', len(topics))  # topic count
        for topic in topics:
            body += encode_string(topic)
    
    body += struct.pack('>b', 1)  # allow_auto_topic_creation = true
    body += struct.pack('>b', 1)  # include_cluster_authorized_operations = true
    body += struct.pack('>b', 1)  # include_topic_authorized_operations = true
    
    # Request header
    header = b''
    header += struct.pack('>h', 3)  # API key (Metadata)
    header += struct.pack('>h', 9)  # API version
    header += struct.pack('>i', 999)  # Correlation ID
    header += encode_string('test-client')  # Client ID
    
    # Full request
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    # Send request
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    # Read response
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    
    # Parse response header
    correlation_id = struct.unpack('>i', response_data[:4])[0]
    
    # Parse response body
    pos = 4
    throttle_time_ms = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    # Skip broker info for now
    broker_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    brokers = []
    for _ in range(broker_count):
        node_id = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        host_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        host = response_data[pos:pos+host_len].decode('utf-8')
        pos += host_len
        port = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        rack_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        if rack_len > 0:
            pos += rack_len
        brokers.append((node_id, host, port))
    
    # Skip cluster_id
    cluster_id_len = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    if cluster_id_len > 0:
        pos += cluster_id_len
    
    # controller_id
    controller_id = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    # Topics
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    topics = []
    for _ in range(topic_count):
        error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        topic_name = response_data[pos:pos+topic_name_len].decode('utf-8')
        pos += topic_name_len
        
        is_internal = struct.unpack('>b', response_data[pos:pos+1])[0]
        pos += 1
        
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        partitions = []
        for _ in range(partition_count):
            p_error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            partition_index = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            leader_id = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            leader_epoch = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            
            # replica array
            replica_count = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            replicas = []
            for _ in range(replica_count):
                replicas.append(struct.unpack('>i', response_data[pos:pos+4])[0])
                pos += 4
            
            # isr array
            isr_count = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            isrs = []
            for _ in range(isr_count):
                isrs.append(struct.unpack('>i', response_data[pos:pos+4])[0])
                pos += 4
            
            # offline replicas
            offline_count = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            for _ in range(offline_count):
                pos += 4
            
            partitions.append({
                'error_code': p_error_code,
                'partition': partition_index,
                'leader': leader_id,
                'replicas': replicas,
                'isr': isrs
            })
        
        # Skip topic_authorized_operations
        authorized_ops = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        topics.append({
            'error_code': error_code,
            'name': topic_name,
            'is_internal': is_internal,
            'partitions': partitions
        })
    
    sock.close()
    return correlation_id, brokers, topics

# Test metadata after creating topic
print("Test 1: Get metadata for all topics")
corr_id, brokers, topics = send_metadata_request('localhost', 9092)
print(f"Correlation ID: {corr_id}")
print(f"Brokers: {brokers}")
print(f"Topics found: {len(topics)}")
for topic in topics:
    print(f"  - {topic['name']} (internal: {topic['is_internal']}, error: {topic['error_code']})")
    for partition in topic['partitions']:
        print(f"    Partition {partition['partition']}: leader={partition['leader']}, replicas={partition['replicas']}, isr={partition['isr']}")

print("\nTest 2: Get metadata for specific topic")
corr_id, brokers, topics = send_metadata_request('localhost', 9092, ['test-topic'])
print(f"Correlation ID: {corr_id}")
print(f"Topics found: {len(topics)}")
for topic in topics:
    print(f"  - {topic['name']} (error: {topic['error_code']})")
    if topic['error_code'] == 0:
        print("  ✓ Topic exists in metadata")
    else:
        print("  ✗ Topic not found in metadata")