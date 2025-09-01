#!/usr/bin/env python3
"""
Analyze the fixed ApiVersions response bytes.
"""

def analyze_response():
    # The response bytes from the fixed version
    response_hex = "00 00 14 00 00 00 09 00 00 01 00 0d 00 00 02 00 04 00 00 03 00 0c 00 00 08 00 08 00 00 09 00 08"
    bytes_list = [int(b, 16) for b in response_hex.split()]
    
    print("Fixed ApiVersions v3 Response Analysis")
    print("=" * 50)
    
    offset = 0
    
    # Tagged fields (compact bytes) - should be first after correlation ID
    tagged_fields = bytes_list[offset]
    print(f"Tagged fields: 0x{tagged_fields:02x} (should be 0)")
    offset += 1
    
    # Error code (2 bytes)
    error_code = (bytes_list[offset] << 8) | bytes_list[offset + 1]
    print(f"Error code: {error_code} (should be 0)")
    offset += 2
    
    # Compact array length
    array_len_encoded = bytes_list[offset]
    array_len = array_len_encoded - 1 if array_len_encoded > 0 else 0
    print(f"API array length (encoded): 0x{array_len_encoded:02x} = {array_len_encoded}")
    print(f"API array length (actual): {array_len} APIs")
    offset += 1
    
    print("\nAPI Entries (now with INT8 for min/max):")
    print("-" * 40)
    
    # Parse API entries - now with correct INT8 format
    for i in range(min(5, array_len)):
        if offset + 3 >= len(bytes_list):
            print(f"  Entry {i}: Not enough bytes (need 4, have {len(bytes_list) - offset})")
            break
            
        api_key = (bytes_list[offset] << 8) | bytes_list[offset + 1]
        min_ver = bytes_list[offset + 2]  # INT8 now
        max_ver = bytes_list[offset + 3]  # INT8 now
        # Tagged fields would be next
        
        print(f"  Entry {i}: API={api_key}, MinVer={min_ver}, MaxVer={max_ver}")
        offset += 4  # 2 bytes api_key + 1 byte min + 1 byte max
        
        # Check for tagged fields (should be 0)
        if offset < len(bytes_list):
            tagged = bytes_list[offset]
            print(f"         Tagged fields: 0x{tagged:02x}")
            offset += 1
    
    print(f"\nBytes consumed: {offset} out of {len(bytes_list)}")
    
    print("\n" + "=" * 50)
    print("Structure Check:")
    print("-" * 40)
    
    # Expected size calculation
    # Tagged (1) + Error (2) + ArrayLen (1) + APIs (19 * 5) + Final Tagged (1)
    expected_size = 1 + 2 + 1 + (19 * 5) + 1
    print(f"Expected total size: {expected_size} bytes")
    print(f"Actual bytes shown: {len(bytes_list)} bytes (first 32 only)")
    
    # Check the structure
    print("\nStructure looks:")
    if array_len_encoded == 0x14:
        print("✓ Array length correct (0x14 = 20 = 19+1)")
    else:
        print(f"✗ Wrong array length: 0x{array_len_encoded:02x}")
    
    # Check first API entry
    if len(bytes_list) >= 8:
        first_api = (bytes_list[4] << 8) | bytes_list[5]
        first_min = bytes_list[6]
        first_max = bytes_list[7]
        if first_api == 0 and first_min == 0 and first_max == 9:
            print("✓ First API entry correct (Produce: 0, min=0, max=9)")
        else:
            print(f"✗ First API wrong: API={first_api}, min={first_min}, max={first_max}")

if __name__ == "__main__":
    analyze_response()