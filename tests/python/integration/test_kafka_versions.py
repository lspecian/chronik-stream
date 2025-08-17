import socket
import struct
import sys

def test_metadata_v0():
    """Test metadata request version 0 (what kafkactl might use initially)"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("localhost", 9092))
    
    # Metadata request v0
    api_key = 3
    api_version = 0
    correlation_id = 1
    client_id = "kafkactl"
    
    request = bytearray()
    request.extend(struct.pack(">h", api_key))
    request.extend(struct.pack(">h", api_version))
    request.extend(struct.pack(">i", correlation_id))
    request.extend(struct.pack(">h", len(client_id)))
    request.extend(client_id.encode("utf-8"))
    # v0 has no topics array - it always returns all topics
    
    length_prefix = struct.pack(">i", len(request))
    sock.send(length_prefix + request)
    
    # Read response
    response_length_bytes = sock.recv(4)
    if len(response_length_bytes) < 4:
        print("Failed to read response length")
        return
        
    response_length = struct.unpack(">i", response_length_bytes)[0]
    print(f"Response length: {response_length}")
    
    response = sock.recv(response_length)
    print(f"Response bytes: {response.hex()}")
    
    # Parse correlation ID
    correlation_id_resp = struct.unpack(">i", response[0:4])[0]
    print(f"Correlation ID: expected={correlation_id}, got={correlation_id_resp}")
    
    if correlation_id != correlation_id_resp:
        print("ERROR: Correlation ID mismatch!")
    else:
        print("SUCCESS: Correlation ID matches")
    
    sock.close()

def test_metadata_v5():
    """Test metadata request version 5"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("localhost", 9092))
    
    # Metadata request v5
    api_key = 3
    api_version = 5
    correlation_id = 2
    client_id = "kafkactl"
    
    request = bytearray()
    request.extend(struct.pack(">h", api_key))
    request.extend(struct.pack(">h", api_version))
    request.extend(struct.pack(">i", correlation_id))
    request.extend(struct.pack(">h", len(client_id)))
    request.extend(client_id.encode("utf-8"))
    request.extend(struct.pack(">i", -1))  # -1 for all topics
    request.extend(struct.pack(">b", 1))   # allow_auto_topic_creation
    
    length_prefix = struct.pack(">i", len(request))
    sock.send(length_prefix + request)
    
    # Read response
    response_length_bytes = sock.recv(4)
    if len(response_length_bytes) < 4:
        print("Failed to read response length")
        return
        
    response_length = struct.unpack(">i", response_length_bytes)[0]
    print(f"Response length: {response_length}")
    
    response = sock.recv(response_length)
    print(f"Response bytes: {response.hex()}")
    
    # Parse correlation ID
    correlation_id_resp = struct.unpack(">i", response[0:4])[0]
    print(f"Correlation ID: expected={correlation_id}, got={correlation_id_resp}")
    
    if correlation_id != correlation_id_resp:
        print("ERROR: Correlation ID mismatch!")
    else:
        print("SUCCESS: Correlation ID matches")
    
    sock.close()

print("Testing Metadata v0...")
test_metadata_v0()
print("\nTesting Metadata v5...")
test_metadata_v5()