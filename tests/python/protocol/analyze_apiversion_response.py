#!/usr/bin/env python3
"""
Analyze the ApiVersions response bytes to understand the librdkafka crash.
"""

def decode_compact_array_length(byte_val):
    """Decode a compact array length (zigzag encoding)."""
    # In Kafka compact arrays, length is encoded as n+1
    # 0x14 = 20 in decimal, which means 19 items
    return byte_val - 1

def analyze_response():
    # The response bytes from the log
    response_hex = "00 00 00 01 00 00 00 14 00 00 00 00 00 09 00 00 01 00 00 00 0d 00 00 02 00 00 00 04 00 00 03 00"
    bytes_list = [int(b, 16) for b in response_hex.split()]
    
    print("ApiVersions v3 Response Analysis")
    print("=" * 50)
    
    offset = 0
    
    # Correlation ID (4 bytes)
    corr_id = (bytes_list[0] << 24) | (bytes_list[1] << 16) | (bytes_list[2] << 8) | bytes_list[3]
    print(f"Correlation ID: {corr_id}")
    offset += 4
    
    # Tagged fields (compact bytes)
    tagged_fields = bytes_list[offset]
    print(f"Tagged fields: {tagged_fields} (should be 0)")
    offset += 1
    
    # Error code (2 bytes) - WAIT, this should be AFTER tagged fields for v3!
    # Let me re-analyze...
    
    # Actually for ApiVersions v3:
    # - Correlation ID (4 bytes)
    # - Tagged fields (1 byte)
    # - Error code (2 bytes) 
    # - API versions array (compact array)
    # - Tagged fields at end
    
    error_code = (bytes_list[offset] << 8) | bytes_list[offset + 1]
    print(f"Error code: {error_code} (should be 0)")
    offset += 2
    
    # Compact array length
    array_len_encoded = bytes_list[offset]
    array_len = decode_compact_array_length(array_len_encoded)
    print(f"API array length (encoded): 0x{array_len_encoded:02x} = {array_len_encoded}")
    print(f"API array length (actual): {array_len} APIs")
    offset += 1
    
    print("\nAPI Entries:")
    print("-" * 40)
    
    # Parse first few API entries
    for i in range(min(5, array_len)):
        if offset + 5 >= len(bytes_list):
            print(f"  Entry {i}: Not enough bytes")
            break
            
        api_key = (bytes_list[offset] << 8) | bytes_list[offset + 1]
        min_ver = bytes_list[offset + 2]
        max_ver = bytes_list[offset + 3]
        # Tagged fields for each API
        api_tagged = bytes_list[offset + 4]
        
        print(f"  Entry {i}: API={api_key}, MinVer={min_ver}, MaxVer={max_ver}, Tagged={api_tagged}")
        offset += 5
    
    print(f"\nBytes consumed: {offset} out of {len(bytes_list)}")
    
    # Check what librdkafka might be expecting
    print("\n" + "=" * 50)
    print("Potential Issues for librdkafka:")
    print("-" * 40)
    
    if array_len_encoded == 0x14:
        print("✓ Compact array length encoding looks correct (0x14 = 20 = 19+1)")
    else:
        print(f"✗ Unexpected array length encoding: 0x{array_len_encoded:02x}")
    
    # The issue might be the API entries structure
    print("\nExpected API entry structure for v3:")
    print("  - API key (2 bytes)")
    print("  - Min version (1 byte)")  
    print("  - Max version (1 byte)")
    print("  - Tagged fields (1 byte)")
    print("  Total: 5 bytes per entry")
    
    expected_size = 7 + (array_len * 5) + 1  # header + APIs + final tagged
    print(f"\nExpected total size: {expected_size} bytes")
    print(f"Actual bytes shown: {len(bytes_list)} bytes")

if __name__ == "__main__":
    analyze_response()