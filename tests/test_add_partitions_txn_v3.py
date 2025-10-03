#!/usr/bin/env python3
"""Test AddPartitionsToTxn v3 API to verify hybrid encoding fix"""

import socket
import struct
import sys

def encode_request_header_v1(api_key, api_version, correlation_id, client_id):
    """Encode Kafka request header v1 (non-flexible)"""
    header = struct.pack('>h', api_key)  # API key
    header += struct.pack('>h', api_version)  # API version
    header += struct.pack('>i', correlation_id)  # Correlation ID
    header += struct.pack('>h', len(client_id))  # Client ID length
    header += client_id.encode('utf-8')  # Client ID
    return header

def encode_varint(value):
    """Encode unsigned varint"""
    result = b''
    while value > 0x7F:
        result += bytes([(value & 0x7F) | 0x80])
        value >>= 7
    result += bytes([value & 0x7F])
    return result

def encode_compact_string(s):
    """Encode compact string: varint length (+1) + string"""
    if s is None:
        return encode_varint(0)  # Null
    encoded = s.encode('utf-8')
    return encode_varint(len(encoded) + 1) + encoded

def encode_add_partitions_txn_v3(transactional_id, producer_id, producer_epoch, topics):
    """
    Encode AddPartitionsToTxnRequest v3 with HYBRID encoding:
    - Request-level fields: NON-FLEXIBLE (regular string, i64, i16)
    - Topics array and structures: COMPACT encoding (varint lengths, compact strings)
    """
    request = b''

    # Transactional ID - NON-FLEXIBLE string (v3 uses regular string encoding)
    txn_id = transactional_id.encode('utf-8')
    request += struct.pack('>h', len(txn_id))  # String length as i16
    request += txn_id

    # Producer ID - i64
    request += struct.pack('>q', producer_id)

    # Producer epoch - i16
    request += struct.pack('>h', producer_epoch)

    # Topics array - COMPACT encoding (v3+ uses compact arrays for topics)
    request += encode_varint(len(topics) + 1)  # Compact array length

    for topic_name, partitions in topics:
        # Topic name - COMPACT string
        request += encode_compact_string(topic_name)

        # Partitions array - COMPACT encoding
        request += encode_varint(len(partitions) + 1)

        for partition in partitions:
            # Partition index - i32
            request += struct.pack('>i', partition)

            # Partition tagged fields - empty
            request += encode_varint(0)

        # Topic tagged fields - empty
        request += encode_varint(0)

    # Request-level tagged fields - empty (v3 is not flexible version)
    # v3 does NOT have request-level tagged fields

    return request

def test_add_partitions_txn_v3():
    """Test AddPartitionsToTxn v3 API call"""
    try:
        # Connect to Kafka
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))

        print("Connected to Kafka server at localhost:9092")

        # First, get a producer ID via InitProducerId
        print("\n1. Initializing producer...")

        header = encode_request_header_v1(
            api_key=22,  # InitProducerId
            api_version=0,
            correlation_id=1,
            client_id='test-txn-client'
        )

        init_request = b''
        # Transactional ID
        txn_id = 'test-txn-v3'.encode('utf-8')
        init_request += struct.pack('>h', len(txn_id))
        init_request += txn_id
        # Transaction timeout
        init_request += struct.pack('>i', 60000)

        message = header + init_request
        size_bytes = struct.pack('>i', len(message))
        sock.sendall(size_bytes + message)

        # Read InitProducerIdResponse
        size_data = sock.recv(4)
        response_size = struct.unpack('>i', size_data)[0]
        response_data = sock.recv(response_size)

        # Parse response
        offset = 4  # Skip correlation ID
        throttle_time = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        error_code = struct.unpack('>h', response_data[offset:offset+2])[0]
        offset += 2
        producer_id = struct.unpack('>q', response_data[offset:offset+8])[0]
        offset += 8
        producer_epoch = struct.unpack('>h', response_data[offset:offset+2])[0]

        print(f"   Producer ID: {producer_id}, Epoch: {producer_epoch}, Error: {error_code}")

        if error_code != 0:
            print(f"❌ InitProducerId failed with error {error_code}")
            return False

        # Now test AddPartitionsToTxn v3
        print("\n2. Adding partitions to transaction (v3)...")

        header = encode_request_header_v1(
            api_key=24,  # AddPartitionsToTxn
            api_version=3,  # Version 3 - hybrid encoding
            correlation_id=2,
            client_id='test-txn-client'
        )

        topics = [
            ('test-topic', [0, 1]),  # Topic with 2 partitions
        ]

        add_partitions_request = encode_add_partitions_txn_v3(
            transactional_id='test-txn-v3',
            producer_id=producer_id,
            producer_epoch=producer_epoch,
            topics=topics
        )

        # Print hex dump of the request for debugging
        print(f"   Request size: {len(add_partitions_request)} bytes")
        print(f"   Request hex: {add_partitions_request.hex()}")

        message = header + add_partitions_request
        size_bytes = struct.pack('>i', len(message))
        sock.sendall(size_bytes + message)

        print("   Sent AddPartitionsToTxnRequest v3")

        # Read response
        size_data = sock.recv(4)
        if len(size_data) < 4:
            print("❌ Failed to read response size")
            return False

        response_size = struct.unpack('>i', size_data)[0]
        response_data = sock.recv(response_size)

        # Parse response header
        correlation_id = struct.unpack('>i', response_data[:4])[0]
        print(f"   Response correlation ID: {correlation_id}")

        # Parse response body
        offset = 4
        throttle_time = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4

        print(f"   Throttle time: {throttle_time}ms")
        print(f"   Response hex: {response_data.hex()}")

        print("\n✅ AddPartitionsToTxn v3 SUCCESS!")
        print("   No decode errors - hybrid encoding working correctly")

        sock.close()
        return True

    except Exception as e:
        print(f"\n❌ Test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    success = test_add_partitions_txn_v3()
    sys.exit(0 if success else 1)
