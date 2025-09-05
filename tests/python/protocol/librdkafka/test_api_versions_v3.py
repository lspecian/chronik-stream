#!/usr/bin/env python3
"""Test script to understand ApiVersions v3 encoding"""

def encode_varint(value):
    """Encode unsigned varint"""
    result = []
    while value > 0x7f:
        result.append((value & 0x7f) | 0x80)
        value >>= 7
    result.append(value)
    return bytes(result)

def encode_api_versions_v3():
    """Create ApiVersions v3 response exactly as Kafka does"""
    
    response = bytearray()
    
    # Response header (NOT flexible for ApiVersions)
    # Correlation ID (INT32)
    response.extend((1).to_bytes(4, 'big'))
    
    # Response body (flexible/compact encoding)
    # Error code (INT16)
    response.extend((0).to_bytes(2, 'big'))
    
    # API versions array (COMPACT_ARRAY)
    # For compact arrays, we use length + 1
    num_apis = 60
    response.extend(encode_varint(num_apis + 1))
    
    # Each API: api_key(INT16), min_version(INT16), max_version(INT16), tagged_fields(UNSIGNED_VARINT)
    apis = [
        (0, 0, 9),   # Produce
        (1, 0, 13),  # Fetch
        (2, 0, 7),   # ListOffsets
        (3, 0, 12),  # Metadata
        (4, 0, 7),   # LeaderAndIsr
        (5, 0, 4),   # StopReplica
        (6, 0, 7),   # UpdateMetadata
        (7, 0, 3),   # ControlledShutdown
        (8, 0, 8),   # OffsetCommit
        (9, 0, 8),   # OffsetFetch
        (10, 0, 4),  # FindCoordinator
        (11, 0, 9),  # JoinGroup
        (12, 0, 4),  # Heartbeat
        (13, 0, 5),  # LeaveGroup
        (14, 0, 5),  # SyncGroup
        (15, 0, 3),  # DescribeGroups
        (16, 0, 4),  # ListGroups
        (17, 0, 1),  # SaslHandshake
        (18, 0, 3),  # ApiVersions
        (19, 0, 8),  # CreateTopics
        (20, 0, 6),  # DeleteTopics
        (21, 0, 2),  # DeleteRecords
        (22, 0, 4),  # InitProducerId
        (23, 0, 4),  # OffsetForLeaderEpoch
        (24, 0, 3),  # AddPartitionsToTxn
        (25, 0, 3),  # AddOffsetsToTxn
        (26, 0, 3),  # EndTxn
        (27, 0, 1),  # WriteTxnMarkers
        (28, 0, 3),  # TxnOffsetCommit
        (29, 0, 3),  # DescribeAcls
        (30, 0, 3),  # CreateAcls
        (31, 0, 3),  # DeleteAcls
        (32, 0, 5),  # DescribeConfigs
        (33, 0, 2),  # AlterConfigs
        (34, 0, 2),  # AlterReplicaLogDirs
        (35, 0, 4),  # DescribeLogDirs
        (36, 0, 2),  # SaslAuthenticate
        (37, 0, 4),  # CreatePartitions
        (38, 0, 3),  # CreateDelegationToken
        (39, 0, 2),  # RenewDelegationToken
        (40, 0, 2),  # ExpireDelegationToken
        (41, 0, 3),  # DescribeDelegationToken
        (42, 0, 2),  # DeleteGroups
        (43, 0, 2),  # ElectLeaders
        (44, 0, 1),  # IncrementalAlterConfigs
        (45, 0, 2),  # AlterPartitionReassignments
        (46, 0, 2),  # ListPartitionReassignments
        (47, 0, 1),  # OffsetDelete
        (48, 0, 1),  # DescribeClientQuotas
        (49, 0, 1),  # AlterClientQuotas
        (50, 0, 0),  # DescribeUserScramCredentials
        (51, 0, 0),  # AlterUserScramCredentials
        (56, 0, 0),  # AlterPartition
        (57, 0, 0),  # UpdateFeatures
        (60, 0, 0),  # DescribeCluster
        (61, 0, 0),  # DescribeProducers
        (65, 0, 0),  # DescribeTransactions
        (66, 0, 0),  # ListTransactions
        (67, 0, 0),  # AllocateProducerIds
        (68, 0, 0),  # ConsumerGroupHeartbeat
    ]
    
    for api_key, min_v, max_v in apis:
        response.extend(api_key.to_bytes(2, 'big'))
        response.extend(min_v.to_bytes(2, 'big'))
        response.extend(max_v.to_bytes(2, 'big'))
        response.extend(encode_varint(0))  # No tagged fields per API
    
    # According to Kafka spec, ApiVersions v3 has NO throttle_time_ms
    # and NO tagged fields at the end of the response
    
    # But librdkafka expects throttle_time_ms somewhere...
    # Let's try adding it at the end as INT32
    # response.extend((0).to_bytes(4, 'big'))  # throttle_time_ms
    
    return bytes(response)

def main():
    response = encode_api_versions_v3()
    
    print(f"ApiVersions v3 response ({len(response)} bytes):")
    print("Hex:", response.hex())
    print("\nBreakdown:")
    print(f"  Correlation ID: {response[0:4].hex()} = {int.from_bytes(response[0:4], 'big')}")
    print(f"  Error code: {response[4:6].hex()} = {int.from_bytes(response[4:6], 'big')}")
    print(f"  Array length varint: {response[6:7].hex()} = {response[6]} (means {response[6]-1} APIs)")
    print(f"  First API: {response[7:15].hex()}")
    print(f"    - api_key: {int.from_bytes(response[7:9], 'big')}")
    print(f"    - min_version: {int.from_bytes(response[9:11], 'big')}")
    print(f"    - max_version: {int.from_bytes(response[11:13], 'big')}")
    print(f"    - tagged_fields: {response[13]}")
    
    # Check what we're sending
    print("\n\nWhat Chronik sends (from debug output):")
    chronik_bytes = bytes([0, 0, 0, 0, 0, 0, 61, 0, 0, 0, 0, 0, 9, 0])
    print(f"  Correlation ID: missing! (should be 4 bytes)")
    print(f"  Error code: {chronik_bytes[0:2].hex()} = {int.from_bytes(chronik_bytes[0:2], 'big')}")
    print(f"  throttle_time: {chronik_bytes[2:6].hex()} = {int.from_bytes(chronik_bytes[2:6], 'big')}")
    print(f"  Array length: {chronik_bytes[6]} = 61 (means 60 APIs)")
    
    print("\n\nThe issue: Chronik is NOT including correlation ID in the response body!")
    print("The correlation ID should be in the HEADER, not the body.")
    print("But our debug output shows the body starting with error_code, not the full response.")

if __name__ == "__main__":
    main()