#!/usr/bin/env python3
"""Decode Produce v9 response to verify structure"""

def decode_produce_v9_response():
    # Updated 52-byte response from server logs with missing fields added
    response_bytes = bytes([
        0x00, 0x00, 0x00, 0x00,  # throttle_time_ms (4 bytes)
        0x02,                     # topics array length (compact)
        0x0b,                     # topic name length (compact) 
        0x74, 0x65, 0x73, 0x74, 0x2d, 0x74, 0x6f, 0x70, 0x69, 0x63,  # "test-topic"
        0x02,                     # partitions array length (compact)
        0x00, 0x00, 0x00, 0x01,  # partition index (4 bytes)
        0x00, 0x00,              # error_code (2 bytes)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09,  # base_offset (8 bytes)
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,  # log_append_time (8 bytes)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  # log_start_offset (8 bytes)
        0x01,                     # record_errors array length (compact) = 0
        0x00,                     # error_message (compact string) = null
        0x00, 0x00, 0x00          # partition + topic + response tagged fields
    ])
    
    print(f"Total response body length: {len(response_bytes)} bytes")
    print(f"Response hex: {response_bytes.hex()}")
    
    pos = 0
    
    # throttle_time_ms (INT32)
    throttle_time = int.from_bytes(response_bytes[pos:pos+4], 'big')
    pos += 4
    print(f"throttle_time_ms: {throttle_time}")
    
    # topics array length (COMPACT_ARRAY)
    topics_len = response_bytes[pos] - 1  # Compact encoding: subtract 1
    pos += 1
    print(f"topics array length: {topics_len}")
    
    for topic_idx in range(topics_len):
        # topic name length (COMPACT_STRING)
        topic_name_len = response_bytes[pos] - 1
        pos += 1
        print(f"  topic name length: {topic_name_len}")
        
        # topic name
        topic_name = response_bytes[pos:pos+topic_name_len].decode('utf-8')
        pos += topic_name_len
        print(f"  topic name: '{topic_name}'")
        
        # partitions array length (COMPACT_ARRAY)
        partitions_len = response_bytes[pos] - 1
        pos += 1
        print(f"  partitions array length: {partitions_len}")
        
        for partition_idx in range(partitions_len):
            # partition index (INT32)
            partition = int.from_bytes(response_bytes[pos:pos+4], 'big')
            pos += 4
            print(f"    partition: {partition}")
            
            # error_code (INT16)
            error_code = int.from_bytes(response_bytes[pos:pos+2], 'big', signed=True)
            pos += 2
            print(f"    error_code: {error_code}")
            
            # base_offset (INT64)
            base_offset = int.from_bytes(response_bytes[pos:pos+8], 'big', signed=True)
            pos += 8
            print(f"    base_offset: {base_offset}")
            
            # log_append_time (INT64)
            log_append_time = int.from_bytes(response_bytes[pos:pos+8], 'big', signed=True)
            pos += 8  
            print(f"    log_append_time: {log_append_time}")
            
            # log_start_offset (INT64) 
            log_start_offset = int.from_bytes(response_bytes[pos:pos+8], 'big', signed=True)
            pos += 8
            print(f"    log_start_offset: {log_start_offset}")
            
            # record_errors array length (COMPACT_ARRAY) - v8+
            if pos < len(response_bytes):
                record_errors_len = response_bytes[pos] - 1 if response_bytes[pos] > 0 else 0  # Compact array
                pos += 1
                print(f"    record_errors length: {record_errors_len}")
                
                # error_message (COMPACT_STRING) - v8+, should be null
                if pos < len(response_bytes):
                    if response_bytes[pos] == 0:
                        print(f"    error_message: null (compact)")
                        pos += 1
                    else:
                        error_msg_len = response_bytes[pos] - 1
                        pos += 1
                        error_msg = response_bytes[pos:pos+error_msg_len].decode('utf-8') if error_msg_len > 0 else ""
                        pos += error_msg_len
                        print(f"    error_message: '{error_msg}'")
            
            # partition tagged fields  
            if pos < len(response_bytes):
                partition_tagged = response_bytes[pos]
                pos += 1
                print(f"    partition tagged fields: {partition_tagged}")
        
        # topic tagged fields
        if pos < len(response_bytes):
            topic_tagged = response_bytes[pos] 
            pos += 1
            print(f"  topic tagged fields: {topic_tagged}")
    
    # response tagged fields
    if pos < len(response_bytes):
        response_tagged = response_bytes[pos]
        pos += 1
        print(f"response tagged fields: {response_tagged}")
    
    print(f"Parsed {pos} bytes out of {len(response_bytes)} total")

if __name__ == "__main__":
    decode_produce_v9_response()