#!/usr/bin/env python3
"""Test consumer group functionality"""

import socket
import struct
import time
import sys
import uuid

def encode_string(s):
    """Encode string in Kafka wire format"""
    if s is None:
        return struct.pack('>h', -1)
    data = s.encode('utf-8')
    return struct.pack('>h', len(data)) + data

def encode_bytes(b):
    """Encode bytes in Kafka wire format"""
    if b is None:
        return struct.pack('>i', -1)
    return struct.pack('>i', len(b)) + b

def send_find_coordinator(host, port, group_id):
    """Send FindCoordinator request (API key 10)"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    api_key = 10
    api_version = 0
    correlation_id = 1
    client_id = "test-consumer"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body
    body = encode_string(group_id)
    
    # Send request
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    # Parse response
    pos = 0
    correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    error_code = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    
    # Read coordinator info
    node_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    
    # Read host
    host_len = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    coordinator_host = response[pos:pos+host_len].decode('utf-8')
    pos += host_len
    
    # Read port
    coordinator_port = struct.unpack('>i', response[pos:pos+4])[0]
    
    sock.close()
    
    if error_code == 0:
        print(f"  Coordinator: {coordinator_host}:{coordinator_port} (node {node_id})")
        return coordinator_host, coordinator_port, node_id
    else:
        print(f"  Error finding coordinator: {error_code}")
        return None, None, None

def send_join_group(host, port, group_id, member_id=None):
    """Send JoinGroup request (API key 11)"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    api_key = 11
    api_version = 0
    correlation_id = 2
    client_id = "test-consumer"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body
    body = encode_string(group_id)
    body += struct.pack('>i', 30000)  # session_timeout_ms
    
    if member_id:
        body += encode_string(member_id)
    else:
        body += encode_string("")  # empty member_id for new member
    
    body += encode_string("consumer")  # protocol_type
    
    # Group protocols (simplified - range assignor)
    protocol_count = 1
    body += struct.pack('>i', protocol_count)
    
    # Protocol: range
    body += encode_string("range")
    # Protocol metadata (topics to subscribe)
    metadata = struct.pack('>h', 0)  # version
    metadata += struct.pack('>i', 1)  # topic count
    metadata += encode_string("test-topic")
    metadata += struct.pack('>i', -1)  # user data length (-1 = null)
    body += encode_bytes(metadata)
    
    # Send request
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    # Parse response
    pos = 0
    correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    error_code = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    generation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    
    # Read protocol
    protocol_len = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    if protocol_len > 0:
        protocol = response[pos:pos+protocol_len].decode('utf-8')
        pos += protocol_len
    else:
        protocol = None
    
    # Read leader
    leader_len = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    if leader_len > 0:
        leader = response[pos:pos+leader_len].decode('utf-8')
        pos += leader_len
    else:
        leader = None
    
    # Read member_id
    member_id_len = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    if member_id_len > 0:
        returned_member_id = response[pos:pos+member_id_len].decode('utf-8')
        pos += member_id_len
    else:
        returned_member_id = None
    
    sock.close()
    
    if error_code == 0:
        print(f"  Joined group: generation={generation_id}, member={returned_member_id}, leader={leader}, protocol={protocol}")
        return returned_member_id, generation_id, leader == returned_member_id
    else:
        print(f"  Error joining group: {error_code}")
        return None, None, False

def send_sync_group(host, port, group_id, member_id, generation_id, is_leader):
    """Send SyncGroup request (API key 14)"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    api_key = 14
    api_version = 0
    correlation_id = 3
    client_id = "test-consumer"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body
    body = encode_string(group_id)
    body += struct.pack('>i', generation_id)
    body += encode_string(member_id)
    
    # Group assignments (only leader sends assignments)
    if is_leader:
        assignment_count = 1
        body += struct.pack('>i', assignment_count)
        
        # Assignment for self
        body += encode_string(member_id)
        
        # Assignment data (topics and partitions)
        assignment = struct.pack('>h', 0)  # version
        assignment += struct.pack('>i', 1)  # topic count
        assignment += encode_string("test-topic")
        assignment += struct.pack('>i', 1)  # partition count
        assignment += struct.pack('>i', 0)  # partition 0
        assignment += encode_bytes(b"")  # user data
        
        body += encode_bytes(assignment)
    else:
        body += struct.pack('>i', 0)  # no assignments
    
    # Send request
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    # Parse response
    pos = 0
    correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    error_code = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    
    # Read assignment
    assignment_len = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    if assignment_len > 0:
        assignment_data = response[pos:pos+assignment_len]
        # Parse assignment (simplified)
        print(f"  Sync group successful, got assignment ({assignment_len} bytes)")
    else:
        print(f"  Sync group successful, no assignment")
    
    sock.close()
    return error_code == 0

def send_heartbeat(host, port, group_id, member_id, generation_id):
    """Send Heartbeat request (API key 12)"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    api_key = 12
    api_version = 0
    correlation_id = 4
    client_id = "test-consumer"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body
    body = encode_string(group_id)
    body += struct.pack('>i', generation_id)
    body += encode_string(member_id)
    
    # Send request
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    # Parse response
    pos = 0
    correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    error_code = struct.unpack('>h', response[pos:pos+2])[0]
    
    sock.close()
    
    if error_code == 0:
        return True
    else:
        print(f"  Heartbeat error: {error_code}")
        return False

def send_offset_commit(host, port, group_id, member_id, generation_id, topic, partition, offset):
    """Send OffsetCommit request (API key 8)"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    api_key = 8
    api_version = 2
    correlation_id = 5
    client_id = "test-consumer"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body
    body = encode_string(group_id)
    body += struct.pack('>i', generation_id)
    body += encode_string(member_id)
    body += struct.pack('>q', -1)  # retention_time (-1 = use broker default)
    
    # Topics
    body += struct.pack('>i', 1)  # topic count
    body += encode_string(topic)
    
    # Partitions
    body += struct.pack('>i', 1)  # partition count
    body += struct.pack('>i', partition)
    body += struct.pack('>q', offset)
    body += encode_string("")  # metadata
    
    # Send request
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    # Parse response (simplified)
    pos = 4  # skip correlation_id
    # Skip to partition error code
    pos += 4  # topic count
    pos += 2  # topic name length
    topic_name_len = struct.unpack('>h', response[pos-2:pos])[0]
    pos += topic_name_len  # topic name
    pos += 4  # partition count
    pos += 4  # partition id
    error_code = struct.unpack('>h', response[pos:pos+2])[0]
    
    sock.close()
    
    if error_code == 0:
        print(f"  Committed offset {offset} for {topic}:{partition}")
        return True
    else:
        print(f"  Offset commit error: {error_code}")
        return False

def send_offset_fetch(host, port, group_id, topic, partition):
    """Send OffsetFetch request (API key 9)"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    api_key = 9
    api_version = 1
    correlation_id = 6
    client_id = "test-consumer"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body
    body = encode_string(group_id)
    
    # Topics
    body += struct.pack('>i', 1)  # topic count
    body += encode_string(topic)
    
    # Partitions
    body += struct.pack('>i', 1)  # partition count
    body += struct.pack('>i', partition)
    
    # Send request
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    # Parse response (simplified)
    pos = 4  # skip correlation_id
    # Skip to offset
    pos += 4  # topic count
    pos += 2  # topic name length
    topic_name_len = struct.unpack('>h', response[pos-2:pos])[0]
    pos += topic_name_len  # topic name
    pos += 4  # partition count
    pos += 4  # partition id
    offset = struct.unpack('>q', response[pos:pos+8])[0]
    pos += 8
    # Skip metadata
    metadata_len = struct.unpack('>h', response[pos:pos+2])[0]
    pos += 2
    if metadata_len > 0:
        pos += metadata_len
    error_code = struct.unpack('>h', response[pos:pos+2])[0]
    
    sock.close()
    
    if error_code == 0:
        print(f"  Fetched offset {offset} for {topic}:{partition}")
        return offset
    else:
        print(f"  Offset fetch error: {error_code}")
        return -1

if __name__ == "__main__":
    host = "localhost"
    port = 9092
    group_id = f"test-group-{uuid.uuid4().hex[:8]}"
    
    print("=== Testing Consumer Group Coordination ===")
    print(f"Group ID: {group_id}")
    print()
    
    # Test 1: Find coordinator
    print("1. Finding coordinator...")
    coord_host, coord_port, node_id = send_find_coordinator(host, port, group_id)
    if not coord_host:
        print("Failed to find coordinator!")
        sys.exit(1)
    
    # Test 2: Join group
    print("\n2. Joining consumer group...")
    member_id, generation_id, is_leader = send_join_group(coord_host, coord_port, group_id)
    if not member_id:
        print("Failed to join group!")
        sys.exit(1)
    
    # Test 3: Sync group
    print("\n3. Syncing group (getting partition assignment)...")
    if not send_sync_group(coord_host, coord_port, group_id, member_id, generation_id, is_leader):
        print("Failed to sync group!")
        sys.exit(1)
    
    # Test 4: Send heartbeat
    print("\n4. Sending heartbeat...")
    if send_heartbeat(coord_host, coord_port, group_id, member_id, generation_id):
        print("  Heartbeat successful")
    
    # Test 5: Commit offset
    print("\n5. Committing offset...")
    send_offset_commit(coord_host, coord_port, group_id, member_id, generation_id, "test-topic", 0, 100)
    
    # Test 6: Fetch committed offset
    print("\n6. Fetching committed offset...")
    send_offset_fetch(coord_host, coord_port, group_id, "test-topic", 0)
    
    print("\nConsumer group test complete!")
    print("Check ./data/metadata/metadata.json for persisted group data.")