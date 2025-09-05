#!/usr/bin/env python3
"""
Analyze the record batch bytes from librdkafka
"""

def analyze_record_batch():
    # The records data from the debug log (98 bytes)
    records_hex = """
    00 00 00 00 00 00 00 00 00 00 00 56 00 00 00 00 
    02 61 a9 dc 8a 00 00 00 00 00 00 00 00 01 99 0d 
    ef 39 9f 00 00 01 99 0d ef 39 9f ff ff ff ff ff 
    ff ff ff ff ff ff ff ff ff ff 00 00 00 01 48 00 
    00 00 10 74 65 73 74 2d 6b 65 79 2c 48 65 6c 6c 
    6f 20 66 72 6f 6d 20 6c 69 62 72 64 6b 61 66 6b 
    61 21
    """
    
    # Clean and convert to bytes
    hex_str = records_hex.replace('\n', ' ').strip()
    records_bytes = bytes.fromhex(''.join(hex_str.split()))
    
    print(f"Total bytes: {len(records_bytes)}")
    print(f"Hex: {records_bytes.hex()}")
    print()
    
    # Parse record batch v2 header
    offset = 0
    
    # Base offset (int64)
    base_offset = int.from_bytes(records_bytes[offset:offset+8], 'big')
    print(f"Base offset: {base_offset}")
    offset += 8
    
    # Batch length (int32) 
    batch_length = int.from_bytes(records_bytes[offset:offset+4], 'big')
    print(f"Batch length: {batch_length}")
    offset += 4
    
    # Partition leader epoch (int32)
    leader_epoch = int.from_bytes(records_bytes[offset:offset+4], 'big', signed=True)
    print(f"Partition leader epoch: {leader_epoch}")
    offset += 4
    
    # Magic byte (int8)
    magic = records_bytes[offset]
    print(f"Magic byte: {magic}")
    offset += 1
    
    # CRC32C (int32)
    crc = int.from_bytes(records_bytes[offset:offset+4], 'big')
    print(f"CRC32C: 0x{crc:08x}")
    offset += 4
    
    # Attributes (int16)
    attributes = int.from_bytes(records_bytes[offset:offset+2], 'big')
    print(f"Attributes: 0x{attributes:04x}")
    print(f"  Compression: {attributes & 0x07}")
    print(f"  Timestamp type: {(attributes >> 3) & 0x01}")
    print(f"  Is transactional: {(attributes >> 4) & 0x01}")
    print(f"  Is control batch: {(attributes >> 5) & 0x01}")
    offset += 2
    
    # Last offset delta (int32)
    last_offset_delta = int.from_bytes(records_bytes[offset:offset+4], 'big', signed=True)
    print(f"Last offset delta: {last_offset_delta}")
    offset += 4
    
    # Base timestamp (int64)
    base_timestamp = int.from_bytes(records_bytes[offset:offset+8], 'big')
    print(f"Base timestamp: {base_timestamp}")
    offset += 8
    
    # Max timestamp (int64)
    max_timestamp = int.from_bytes(records_bytes[offset:offset+8], 'big')
    print(f"Max timestamp: {max_timestamp}")
    offset += 8
    
    # Producer ID (int64)
    producer_id = int.from_bytes(records_bytes[offset:offset+8], 'big', signed=True)
    print(f"Producer ID: {producer_id}")
    offset += 8
    
    # Producer epoch (int16)
    producer_epoch = int.from_bytes(records_bytes[offset:offset+2], 'big', signed=True)
    print(f"Producer epoch: {producer_epoch}")
    offset += 2
    
    # Base sequence (int32)
    base_sequence = int.from_bytes(records_bytes[offset:offset+4], 'big', signed=True)
    print(f"Base sequence: {base_sequence}")
    offset += 4
    
    # Records count (int32)
    records_count = int.from_bytes(records_bytes[offset:offset+4], 'big')
    print(f"Records count: {records_count}")
    offset += 4
    
    print(f"\nRemaining bytes for records: {len(records_bytes) - offset}")
    print(f"Record data: {records_bytes[offset:].hex()}")
    
    # Parse the record
    print("\nParsing record:")
    rec_offset = offset
    
    # Length (varint)
    length = 0
    shift = 0
    while rec_offset < len(records_bytes):
        b = records_bytes[rec_offset]
        rec_offset += 1
        length |= (b & 0x7F) << shift
        if (b & 0x80) == 0:
            break
        shift += 7
    print(f"  Record length (varint): {length}")
    
    # Attributes (int8)
    rec_attributes = records_bytes[rec_offset]
    print(f"  Record attributes: 0x{rec_attributes:02x}")
    rec_offset += 1
    
    # Timestamp delta (varint)
    ts_delta = 0
    shift = 0
    while rec_offset < len(records_bytes):
        b = records_bytes[rec_offset]
        rec_offset += 1
        ts_delta |= (b & 0x7F) << shift
        if (b & 0x80) == 0:
            break
        shift += 7
    print(f"  Timestamp delta (varint): {ts_delta}")
    
    # Offset delta (varint)
    off_delta = 0
    shift = 0
    while rec_offset < len(records_bytes):
        b = records_bytes[rec_offset]
        rec_offset += 1
        off_delta |= (b & 0x7F) << shift
        if (b & 0x80) == 0:
            break
        shift += 7
    print(f"  Offset delta (varint): {off_delta}")
    
    # Key length (varint)
    key_len_raw = 0
    shift = 0
    while rec_offset < len(records_bytes):
        b = records_bytes[rec_offset]
        rec_offset += 1
        key_len_raw |= (b & 0x7F) << shift
        if (b & 0x80) == 0:
            break
        shift += 7
    # Zigzag decode
    key_len = (key_len_raw >> 1) ^ (-(key_len_raw & 1))
    print(f"  Key length (zigzag varint): {key_len}")
    
    # Key data
    if key_len > 0:
        key_data = records_bytes[rec_offset:rec_offset+key_len]
        rec_offset += key_len
        print(f"  Key: {key_data.decode('utf-8', errors='replace')}")
    
    # Value length (varint)
    val_len_raw = 0
    shift = 0
    while rec_offset < len(records_bytes):
        b = records_bytes[rec_offset]
        rec_offset += 1
        val_len_raw |= (b & 0x7F) << shift
        if (b & 0x80) == 0:
            break
        shift += 7
    # Zigzag decode
    val_len = (val_len_raw >> 1) ^ (-(val_len_raw & 1))
    print(f"  Value length (zigzag varint): {val_len}")
    
    # Value data
    if val_len > 0:
        val_data = records_bytes[rec_offset:rec_offset+val_len]
        rec_offset += val_len
        print(f"  Value: {val_data.decode('utf-8', errors='replace')}")
    
    # Headers count (varint)
    headers_count = 0
    shift = 0
    while rec_offset < len(records_bytes):
        b = records_bytes[rec_offset]
        rec_offset += 1
        headers_count |= (b & 0x7F) << shift
        if (b & 0x80) == 0:
            break
        shift += 7
    print(f"  Headers count (varint): {headers_count}")
    
    print(f"\nBytes consumed: {rec_offset}/{len(records_bytes)}")

if __name__ == "__main__":
    analyze_record_batch()