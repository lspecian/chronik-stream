import socket
import struct

def test_describe_configs():
    """Test DescribeConfigs request (API key 32)"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("localhost", 9092))
    
    # DescribeConfigs request v0
    api_key = 32
    api_version = 0
    correlation_id = 1
    client_id = "kafkactl"
    
    request = bytearray()
    request.extend(struct.pack(">h", api_key))
    request.extend(struct.pack(">h", api_version))
    request.extend(struct.pack(">i", correlation_id))
    request.extend(struct.pack(">h", len(client_id)))
    request.extend(client_id.encode("utf-8"))
    # DescribeConfigs v0 has resources array, let's send empty array
    request.extend(struct.pack(">i", 0))  # 0 resources
    
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
    if len(response) >= 4:
        correlation_id_resp = struct.unpack(">i", response[0:4])[0]
        print(f"Correlation ID: expected={correlation_id}, got={correlation_id_resp}")
        
        if correlation_id != correlation_id_resp:
            print("ERROR: Correlation ID mismatch!")
        else:
            print("SUCCESS: Correlation ID matches")
            
        # If there's more data, it might be an error code
        if len(response) >= 6:
            error_code = struct.unpack(">h", response[4:6])[0]
            print(f"Error code: {error_code}")
    else:
        print("Response too short to parse")
    
    sock.close()

print("Testing DescribeConfigs...")
test_describe_configs()