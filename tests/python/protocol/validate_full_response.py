#!/usr/bin/env python3
"""
Validate the full ApiVersions v3 response from Chronik Stream.
"""

import socket
import struct

def get_apiversion_response():
    """Get ApiVersions response from Chronik Stream."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build ApiVersions v3 request
    correlation_id = 1
    client_id = b'test-client'
    
    # Request format for v3 (flexible):
    # api_key (2) + api_version (2) + correlation_id (4) + tagged_fields (1) + client_id (compact string) + tagged_fields (1)
    client_id_len = len(client_id) + 1  # Compact string encoding
    request_body = struct.pack('>hhiB', 18, 3, correlation_id, 0)  # header + tagged fields
    request_body += bytes([client_id_len]) + client_id  # compact string
    request_body += b'\x00'  # tagged fields
    
    request = struct.pack('>i', len(request_body)) + request_body
    
    print(f"Sending request ({len(request)} bytes): {request.hex()}")
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    response = sock.recv(size)
    
    sock.close()
    
    print(f"\nReceived response: {size} bytes (excluding 4-byte size)")
    return response

def validate_response(data):
    """Validate ApiVersions v3 response structure."""
    print("\n" + "="*60)
    print("ApiVersions v3 Response Validation")
    print("="*60)
    
    offset = 0
    errors = []
    
    # 1. Correlation ID (4 bytes)
    if len(data) < 4:
        errors.append(f"Response too short: {len(data)} bytes")
        return errors
        
    correlation_id = struct.unpack('>i', data[offset:offset+4])[0]
    print(f"✓ Correlation ID: {correlation_id}")
    offset += 4
    
    # 2. Tagged fields (compact bytes)
    tagged_fields = data[offset]
    print(f"✓ Tagged fields: 0x{tagged_fields:02x}")
    if tagged_fields != 0:
        errors.append(f"Tagged fields should be 0, got {tagged_fields}")
    offset += 1
    
    # 3. Error code (2 bytes)
    error_code = struct.unpack('>h', data[offset:offset+2])[0]
    print(f"✓ Error code: {error_code}")
    if error_code != 0:
        errors.append(f"Error code should be 0, got {error_code}")
    offset += 2
    
    # 4. API array length (compact)
    array_len_encoded = data[offset]
    array_len = array_len_encoded - 1 if array_len_encoded > 0 else 0
    print(f"✓ API array length: {array_len} (encoded as 0x{array_len_encoded:02x})")
    offset += 1
    
    # 5. Parse API entries
    apis = []
    for i in range(array_len):
        if offset + 4 > len(data):
            errors.append(f"Not enough data for API {i}: need 4 bytes, have {len(data) - offset}")
            break
            
        api_key = struct.unpack('>h', data[offset:offset+2])[0]
        offset += 2
        
        # CRITICAL: In v3+, min/max are INT8, not INT16!
        min_ver = data[offset]
        offset += 1
        max_ver = data[offset]
        offset += 1
        
        # Tagged fields for this API
        if offset >= len(data):
            errors.append(f"Missing tagged fields for API {i}")
            break
        api_tagged = data[offset]
        offset += 1
        
        apis.append({
            'api_key': api_key,
            'min_version': min_ver,
            'max_version': max_ver,
            'tagged': api_tagged
        })
        
        if api_tagged != 0:
            errors.append(f"API {api_key} has non-zero tagged fields: {api_tagged}")
    
    print(f"\n✓ Parsed {len(apis)} API entries")
    
    # 6. Final tagged fields
    if offset >= len(data):
        errors.append("Missing final tagged fields")
    else:
        final_tagged = data[offset]
        print(f"✓ Final tagged fields: 0x{final_tagged:02x}")
        if final_tagged != 0:
            errors.append(f"Final tagged fields should be 0, got {final_tagged}")
        offset += 1
    
    # 7. Throttle time ms (4 bytes) - required in v3+
    if offset + 4 > len(data):
        errors.append(f"Missing throttle_time_ms: need 4 bytes, have {len(data) - offset}")
    else:
        throttle_time = struct.unpack('>i', data[offset:offset+4])[0]
        print(f"✓ Throttle time ms: {throttle_time}")
        offset += 4
    
    # Check if we consumed all bytes
    if offset != len(data):
        errors.append(f"Extra bytes: consumed {offset}, but response is {len(data)} bytes")
    else:
        print(f"\n✓ All {len(data)} bytes consumed correctly")
    
    # Display API details
    print(f"\nAPI Details (showing first 10):")
    for api in apis[:10]:
        print(f"  API {api['api_key']:3d}: v{api['min_version']}-v{api['max_version']}")
    
    return errors

def main():
    try:
        response = get_apiversion_response()
        
        # Show hex dump
        print("\nFull response hex dump:")
        for i in range(0, len(response), 16):
            hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
            print(f"  {i:04x}: {hex_str}")
        
        # Validate
        errors = validate_response(response)
        
        if errors:
            print("\n" + "="*60)
            print("ERRORS FOUND:")
            for error in errors:
                print(f"  ✗ {error}")
            print("="*60)
        else:
            print("\n" + "="*60)
            print("✅ Response structure is VALID!")
            print("="*60)
            
    except Exception as e:
        print(f"Error: {e}")
        import traceback
        traceback.print_exc()

if __name__ == "__main__":
    main()