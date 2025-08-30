#!/usr/bin/env python3
"""Debug Metadata response encoding."""

import socket
import struct

def encode_unsigned_varint(value):
    """Encode an unsigned varint."""
    result = bytearray()
    while (value & ~0x7F) != 0:
        result.append((value & 0x7F) | 0x80)
        value >>= 7
    result.append(value & 0x7F)
    return bytes(result)

def encode_compact_string(s):
    """Encode a compact string (varint length + string bytes)."""
    if s is None:
        return encode_unsigned_varint(0)
    encoded = s.encode('utf-8')
    return encode_unsigned_varint(len(encoded) + 1) + encoded

def build_metadata_v9_request():
    """Build a Metadata v9 request with flexible encoding."""
    correlation_id = 12345
    
    # Request header for v9+ (flexible)
    header = bytearray()
    header.extend(struct.pack('>h', 3))  # API key (Metadata)
    header.extend(struct.pack('>h', 9))  # API version 9
    header.extend(struct.pack('>i', correlation_id))  # Correlation ID
    header.extend(encode_compact_string("debug-client"))  # Client ID (compact)
    header.extend(encode_unsigned_varint(0))  # Tagged fields
    
    # Request body for Metadata v9
    body = bytearray()
    body.extend(encode_unsigned_varint(0))  # Empty topics array (get all)
    body.extend([0])  # allow_auto_topic_creation = false
    body.extend([0])  # include_cluster_authorized_operations = false
    body.extend([0])  # include_topic_authorized_operations = false
    body.extend(encode_unsigned_varint(0))  # Tagged fields
    
    # Full request
    request = header + body
    return struct.pack('>i', len(request)) + request, correlation_id

def debug_response():
    """Debug the raw response bytes."""
    print("\n=== Debugging Metadata v9 Response ===")
    
    try:
        # Connect to server
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        sock.connect(('localhost', 9092))
        
        # Send request
        request, expected_corr_id = build_metadata_v9_request()
        print(f"Sending request with correlation ID: {expected_corr_id}")
        sock.sendall(request)
        
        # Read response
        size_bytes = sock.recv(4)
        response_size = struct.unpack('>i', size_bytes)[0]
        print(f"Response size: {response_size} bytes")
        
        response = b""
        while len(response) < response_size:
            chunk = sock.recv(min(4096, response_size - len(response)))
            if not chunk:
                break
            response += chunk
        
        # Print first 200 bytes in hex
        print(f"\nFirst 200 bytes of response (hex):")
        for i in range(0, min(200, len(response)), 16):
            hex_part = response[i:i+16].hex(' ')
            ascii_part = ''.join(chr(b) if 32 <= b < 127 else '.' for b in response[i:i+16])
            print(f"{i:04x}: {hex_part:<48} {ascii_part}")
        
        # Parse just the beginning
        pos = 0
        corr_id = struct.unpack('>i', response[pos:pos+4])[0]
        print(f"\nCorrelation ID (offset 0-3): {corr_id} (0x{corr_id:08x})")
        pos += 4
        
        # Next byte should be tagged fields count (varint)
        tagged_byte = response[pos]
        print(f"Tagged fields byte (offset {pos}): 0x{tagged_byte:02x}")
        pos += 1
        
        # Then throttle time
        throttle = struct.unpack('>i', response[pos:pos+4])[0]
        print(f"Throttle time (offset {pos}-{pos+3}): {throttle} (0x{throttle:08x})")
        pos += 4
        
        # Then broker array length (varint)
        broker_length_byte = response[pos]
        print(f"Broker array length byte (offset {pos}): 0x{broker_length_byte:02x}")
        
        # Decode as varint
        value = 0
        shift = 0
        start_pos = pos
        while True:
            byte = response[pos]
            pos += 1
            value |= (byte & 0x7F) << shift
            if (byte & 0x80) == 0:
                break
            shift += 7
        
        print(f"Broker array varint value: {value}")
        if value > 0:
            actual_count = value - 1  # Compact arrays use +1
            print(f"Actual broker count: {actual_count}")
        else:
            print("Broker array is empty/null")
        
        sock.close()
        
    except Exception as e:
        print(f"❌ Exception: {e}")
        import traceback
        traceback.print_exc()

if __name__ == "__main__":
    debug_response()