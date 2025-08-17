#!/usr/bin/env python3
"""Test partition assignment strategies in consumer groups"""

import socket
import struct
import time
import threading
import random

def encode_string(s):
    """Encode a string as Kafka protocol string (length + data)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def encode_bytes(b):
    """Encode bytes as Kafka protocol bytes (length + data)"""
    if b is None:
        return struct.pack('>i', -1)
    return struct.pack('>i', len(b)) + b

def create_subscription_metadata(topics, user_data=None):
    """Create subscription metadata for consumer protocol"""
    metadata = b''
    
    # Version
    metadata += struct.pack('>h', 0)
    
    # Topics array
    metadata += struct.pack('>i', len(topics))
    for topic in topics:
        metadata += encode_string(topic)
    
    # User data
    if user_data:
        metadata += struct.pack('>i', len(user_data))
        metadata += user_data
    else:
        metadata += struct.pack('>i', -1)
    
    return metadata

def decode_assignment(assignment_bytes):
    """Decode partition assignment from SyncGroup response"""
    if len(assignment_bytes) < 2:
        return {}
    
    pos = 0
    # Version
    version = struct.unpack('>h', assignment_bytes[pos:pos+2])[0]
    pos += 2
    
    # Topic count
    topic_count = struct.unpack('>i', assignment_bytes[pos:pos+4])[0]
    pos += 4
    
    assignments = {}
    for _ in range(topic_count):
        # Topic name length
        topic_len = struct.unpack('>h', assignment_bytes[pos:pos+2])[0]
        pos += 2
        
        # Topic name
        topic = assignment_bytes[pos:pos+topic_len].decode('utf-8')
        pos += topic_len
        
        # Partition count
        partition_count = struct.unpack('>i', assignment_bytes[pos:pos+4])[0]
        pos += 4
        
        # Partitions
        partitions = []
        for _ in range(partition_count):
            partition = struct.unpack('>i', assignment_bytes[pos:pos+4])[0]
            pos += 4
            partitions.append(partition)
        
        assignments[topic] = sorted(partitions)
    
    return assignments

def join_group(host, port, group_id, member_id, protocol, topics):
    """Send JoinGroup request and return response"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Create subscription metadata
    metadata = create_subscription_metadata(topics)
    
    # Build request body
    body = b''
    body += encode_string(group_id)
    body += struct.pack('>i', 30000)  # session timeout
    body += struct.pack('>i', 60000)  # rebalance timeout
    body += encode_string(member_id)
    body += encode_string(None)  # group instance id
    body += encode_string('consumer')  # protocol type
    
    # Protocols array
    body += struct.pack('>i', 1)  # 1 protocol
    body += encode_string(protocol)  # protocol name
    body += encode_bytes(metadata)  # protocol metadata
    
    # Build header
    header = b''
    header += struct.pack('>h', 11)  # API key (JoinGroup)
    header += struct.pack('>h', 5)   # API version
    header += struct.pack('>i', random.randint(1, 1000000))  # Correlation ID
    header += encode_string('partition-test')  # Client ID
    
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    sock.send(request_with_length)
    
    # Read response
    sock.settimeout(5.0)
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    
    # Parse response
    pos = 4  # Skip correlation ID
    
    # Throttle time
    throttle_time = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    # Error code
    error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    
    # Generation ID
    generation_id = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    # Protocol type
    protocol_type_len = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    if protocol_type_len > 0:
        protocol_type = response_data[pos:pos+protocol_type_len].decode('utf-8')
        pos += protocol_type_len
    else:
        protocol_type = None
    
    # Protocol name
    protocol_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    if protocol_name_len > 0:
        protocol_name = response_data[pos:pos+protocol_name_len].decode('utf-8')
        pos += protocol_name_len
    else:
        protocol_name = None
    
    # Leader
    leader_len = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    leader = response_data[pos:pos+leader_len].decode('utf-8')
    pos += leader_len
    
    # Member ID
    member_id_len = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    member_id = response_data[pos:pos+member_id_len].decode('utf-8')
    pos += member_id_len
    
    # Members array (only for leader)
    members_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    members = []
    for _ in range(members_count):
        # Member ID
        m_id_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        m_id = response_data[pos:pos+m_id_len].decode('utf-8')
        pos += m_id_len
        
        # Group instance ID
        gi_id_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        if gi_id_len > 0:
            pos += gi_id_len  # Skip
        
        # Metadata
        metadata_len = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        if metadata_len > 0:
            m_metadata = response_data[pos:pos+metadata_len]
            pos += metadata_len
            members.append((m_id, m_metadata))
    
    sock.close()
    
    return {
        'error_code': error_code,
        'generation_id': generation_id,
        'protocol_name': protocol_name,
        'leader': leader,
        'member_id': member_id,
        'members': members,
        'is_leader': member_id == leader
    }

def sync_group(host, port, group_id, generation_id, member_id, assignments=None):
    """Send SyncGroup request and return response"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build request body
    body = b''
    body += encode_string(group_id)
    body += struct.pack('>i', generation_id)
    body += encode_string(member_id)
    body += encode_string(None)  # group instance id
    
    # Assignments array (only leader provides this)
    if assignments:
        body += struct.pack('>i', len(assignments))
        for member, assignment in assignments.items():
            body += encode_string(member)
            body += encode_bytes(assignment)
    else:
        body += struct.pack('>i', 0)  # Empty assignments
    
    # Build header
    header = b''
    header += struct.pack('>h', 14)  # API key (SyncGroup)
    header += struct.pack('>h', 3)   # API version
    header += struct.pack('>i', random.randint(1, 1000000))  # Correlation ID
    header += encode_string('partition-test')  # Client ID
    
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    sock.send(request_with_length)
    
    # Read response
    sock.settimeout(5.0)
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    
    # Parse response
    pos = 4  # Skip correlation ID
    
    # Throttle time
    throttle_time = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    # Error code
    error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    
    # Assignment
    assignment_len = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    assignment_bytes = response_data[pos:pos+assignment_len] if assignment_len > 0 else b''
    
    sock.close()
    
    return {
        'error_code': error_code,
        'assignment': decode_assignment(assignment_bytes) if assignment_bytes else {}
    }

def test_range_assignment():
    """Test range partition assignment strategy"""
    print("\n=== Testing Range Assignment Strategy ===\n")
    
    group_id = f"range-test-{int(time.time())}"
    topics = ['test-topic-1', 'test-topic-2']
    members = []
    
    # Join 3 consumers to the group
    for i in range(3):
        result = join_group('localhost', 9092, group_id, '', 'range', topics)
        
        if result['error_code'] != 0:
            print(f"✗ Consumer {i+1} failed to join: error {result['error_code']}")
            return
        
        members.append({
            'id': result['member_id'],
            'is_leader': result['is_leader'],
            'generation': result['generation_id']
        })
        
        print(f"✓ Consumer {i+1} joined: {result['member_id'][:8]}... (leader: {result['is_leader']})")
        
        # If this is the leader and we have all members
        if result['is_leader'] and i == 2:
            # Leader computes assignments (simplified - in reality the assignor does this)
            # For range assignment, partitions are assigned in contiguous blocks
            assignments = {}
            
            # Assuming each topic has 6 partitions
            partitions_per_topic = 6
            consumers_count = 3
            partitions_per_consumer = partitions_per_topic // consumers_count
            
            for idx, (m_id, m_metadata) in enumerate(result['members']):
                assignment_data = b''
                assignment_data += struct.pack('>h', 1)  # Version
                assignment_data += struct.pack('>i', len(topics))  # Topic count
                
                for topic in topics:
                    assignment_data += encode_string(topic)
                    
                    # Calculate partition range for this consumer
                    start = idx * partitions_per_consumer
                    end = start + partitions_per_consumer
                    if idx == consumers_count - 1:  # Last consumer gets remaining partitions
                        end = partitions_per_topic
                    
                    partitions = list(range(start, end))
                    assignment_data += struct.pack('>i', len(partitions))
                    for p in partitions:
                        assignment_data += struct.pack('>i', p)
                
                assignment_data += struct.pack('>i', 0)  # User data
                assignments[m_id] = assignment_data
            
            # Leader sends assignments
            leader_sync = sync_group('localhost', 9092, group_id, result['generation_id'], 
                                   result['member_id'], assignments)
            
            print(f"\nLeader assignment: {leader_sync['assignment']}")
    
    # Non-leaders sync to get their assignments
    for member in members:
        if not member['is_leader']:
            sync_result = sync_group('localhost', 9092, group_id, member['generation'], 
                                   member['id'])
            print(f"Member {member['id'][:8]}... assignment: {sync_result['assignment']}")

def test_roundrobin_assignment():
    """Test round-robin partition assignment strategy"""
    print("\n=== Testing Round-Robin Assignment Strategy ===\n")
    
    group_id = f"roundrobin-test-{int(time.time())}"
    topics = ['test-topic-a', 'test-topic-b', 'test-topic-c']
    
    # Join 2 consumers with round-robin protocol
    consumer1 = join_group('localhost', 9092, group_id, '', 'roundrobin', topics)
    print(f"Consumer 1: {consumer1['member_id'][:8]}... (leader: {consumer1['is_leader']})")
    
    consumer2 = join_group('localhost', 9092, group_id, '', 'roundrobin', topics)
    print(f"Consumer 2: {consumer2['member_id'][:8]}... (leader: {consumer2['is_leader']})")
    
    # The implementation should distribute partitions evenly across consumers

def test_sticky_assignment():
    """Test sticky/cooperative assignment strategy"""
    print("\n=== Testing Sticky Assignment Strategy ===\n")
    
    group_id = f"sticky-test-{int(time.time())}"
    topics = ['test-topic-x', 'test-topic-y']
    
    # This tests that partitions stick to their current owners during rebalance
    # when possible (cooperative rebalancing)
    
    consumer1 = join_group('localhost', 9092, group_id, '', 'cooperative-sticky', topics)
    print(f"Initial consumer: {consumer1['member_id'][:8]}...")
    
    # After initial assignment, adding new consumer should preserve most assignments
    time.sleep(1)
    
    consumer2 = join_group('localhost', 9092, group_id, '', 'cooperative-sticky', topics)
    print(f"New consumer joined: {consumer2['member_id'][:8]}...")
    
    print("\nWith sticky assignment, minimal partition movement should occur")

def main():
    print("Starting Partition Assignment Tests\n")
    print("Make sure Chronik is running on localhost:9092\n")
    
    try:
        test_range_assignment()
        test_roundrobin_assignment() 
        test_sticky_assignment()
        
        print("\n=== All Tests Completed ===")
        
    except Exception as e:
        print(f"\n✗ Test failed: {e}")
        print("Make sure Chronik is running on localhost:9092")

if __name__ == "__main__":
    main()