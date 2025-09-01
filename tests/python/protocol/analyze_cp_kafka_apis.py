#!/usr/bin/env python3
"""
Analyze which APIs CP Kafka returns to understand what we need to add.
"""

import socket
import struct

def get_apiversion_response(host, port):
    """Get ApiVersions v0 response for simplicity."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Send ApiVersions v0 request
    correlation_id = 1
    client_id = b'test-client'
    
    request_body = b''
    request_body += struct.pack('>h', 18)  # API key
    request_body += struct.pack('>h', 0)   # API version
    request_body += struct.pack('>i', correlation_id)
    request_body += struct.pack('>h', len(client_id))
    request_body += client_id
    
    request = struct.pack('>i', len(request_body)) + request_body
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    
    response = b''
    while len(response) < size:
        chunk = sock.recv(min(4096, size - len(response)))
        if not chunk:
            break
        response += chunk
    
    sock.close()
    return response

def parse_apis(response, name):
    """Parse and list all APIs."""
    print(f"\n=== {name} APIs ===")
    
    offset = 4  # Skip correlation ID
    
    # For CP Kafka v0: skip error code (comes first in some versions)
    # Check if this looks like an error code or array length
    first_int = struct.unpack('>i', response[offset:offset+4])[0]
    
    if first_int == 0:  # Likely error code
        # CP Kafka format: correlation_id, error_code, then array
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        array_len = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
    else:
        # Chronik format: correlation_id, array, then error_code
        array_len = first_int
        offset += 4
    
    print(f"Total APIs: {array_len}")
    print("\nAPI List:")
    print("-" * 50)
    
    api_dict = {}
    for i in range(array_len):
        api_key = struct.unpack('>h', response[offset:offset+2])[0]
        min_ver = struct.unpack('>h', response[offset+2:offset+4])[0]
        max_ver = struct.unpack('>h', response[offset+4:offset+6])[0]
        offset += 6
        
        api_name = API_NAMES.get(api_key, f"Unknown({api_key})")
        api_dict[api_key] = (api_name, min_ver, max_ver)
        print(f"  {api_key:3d}: {api_name:30s} v{min_ver}-v{max_ver}")
    
    return api_dict

# Kafka API names
API_NAMES = {
    0: "Produce",
    1: "Fetch",
    2: "ListOffsets",
    3: "Metadata",
    4: "LeaderAndIsr",
    5: "StopReplica",
    6: "UpdateMetadata",
    7: "ControlledShutdown",
    8: "OffsetCommit",
    9: "OffsetFetch",
    10: "FindCoordinator",
    11: "JoinGroup",
    12: "Heartbeat",
    13: "LeaveGroup",
    14: "SyncGroup",
    15: "DescribeGroups",
    16: "ListGroups",
    17: "SaslHandshake",
    18: "ApiVersions",
    19: "CreateTopics",
    20: "DeleteTopics",
    21: "DeleteRecords",
    22: "InitProducerId",
    23: "OffsetForLeaderEpoch",
    24: "AddPartitionsToTxn",
    25: "AddOffsetsToTxn",
    26: "EndTxn",
    27: "WriteTxnMarkers",
    28: "TxnOffsetCommit",
    29: "DescribeAcls",
    30: "CreateAcls",
    31: "DeleteAcls",
    32: "DescribeConfigs",
    33: "AlterConfigs",
    34: "AlterReplicaLogDirs",
    35: "DescribeLogDirs",
    36: "SaslAuthenticate",
    37: "CreatePartitions",
    38: "CreateDelegationToken",
    39: "RenewDelegationToken",
    40: "ExpireDelegationToken",
    41: "DescribeDelegationToken",
    42: "DeleteGroups",
    43: "ElectLeaders",
    44: "IncrementalAlterConfigs",
    45: "AlterPartitionReassignments",
    46: "ListPartitionReassignments",
    47: "OffsetDelete",
    48: "DescribeClientQuotas",
    49: "AlterClientQuotas",
    50: "DescribeUserScramCredentials",
    51: "AlterUserScramCredentials",
    52: "Vote",
    53: "BeginQuorumEpoch",
    54: "EndQuorumEpoch",
    55: "DescribeQuorum",
    56: "AlterPartition",
    57: "UpdateFeatures",
    58: "Envelope",
    59: "FetchSnapshot",
    60: "DescribeCluster",
    61: "DescribeProducers",
    62: "BrokerRegistration",
    63: "BrokerHeartbeat",
    64: "UnregisterBroker",
    65: "DescribeTransactions",
    66: "ListTransactions",
    67: "AllocateProducerIds",
    68: "ConsumerGroupHeartbeat",
}

def main():
    print("="*60)
    print("Analyzing API Support: CP Kafka vs Chronik")
    print("="*60)
    
    # Get CP Kafka APIs
    cp_response = get_apiversion_response('localhost', 29092)
    cp_apis = parse_apis(cp_response, "CP Kafka 7.5.0")
    
    # Get Chronik APIs
    chronik_response = get_apiversion_response('localhost', 9092)
    chronik_apis = parse_apis(chronik_response, "Chronik Stream")
    
    # Compare
    print("\n" + "="*60)
    print("COMPARISON")
    print("="*60)
    
    cp_keys = set(cp_apis.keys())
    chronik_keys = set(chronik_apis.keys())
    
    print(f"\nCP Kafka has {len(cp_keys)} APIs")
    print(f"Chronik has {len(chronik_keys)} APIs")
    
    missing = cp_keys - chronik_keys
    if missing:
        print(f"\n⚠️  APIs in CP Kafka but NOT in Chronik ({len(missing)}):")
        for api_key in sorted(missing):
            name, min_ver, max_ver = cp_apis[api_key]
            print(f"  {api_key:3d}: {name:30s} v{min_ver}-v{max_ver}")
    
    extra = chronik_keys - cp_keys
    if extra:
        print(f"\n⚠️  APIs in Chronik but NOT in CP Kafka ({len(extra)}):")
        for api_key in sorted(extra):
            name, min_ver, max_ver = chronik_apis[api_key]
            print(f"  {api_key:3d}: {name:30s} v{min_ver}-v{max_ver}")
    
    common = cp_keys & chronik_keys
    print(f"\n✓ Common APIs ({len(common)}):")
    differences = []
    for api_key in sorted(common):
        cp_name, cp_min, cp_max = cp_apis[api_key]
        ch_name, ch_min, ch_max = chronik_apis[api_key]
        if cp_min != ch_min or cp_max != ch_max:
            differences.append((api_key, cp_name, cp_min, cp_max, ch_min, ch_max))
            print(f"  {api_key:3d}: {cp_name:30s} CP:v{cp_min}-v{cp_max} vs Chronik:v{ch_min}-v{ch_max} ⚠️")
        else:
            print(f"  {api_key:3d}: {cp_name:30s} v{cp_min}-v{cp_max}")
    
    if differences:
        print(f"\n⚠️  Version differences found for {len(differences)} APIs")

if __name__ == "__main__":
    main()