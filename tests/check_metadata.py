#!/usr/bin/env python3
"""Check metadata responses from all 3 nodes."""

from kafka.admin import KafkaAdminClient
from kafka.structs import TopicPartition

print("Checking metadata for 'raft-test' from all 3 nodes...\n")

for node_port in [9092, 9093, 9094]:
    print(f"Node on port {node_port}:")
    admin = KafkaAdminClient(bootstrap_servers=f'localhost:{node_port}')

    cluster_metadata = admin._client.cluster
    for partition_id in range(3):  # 3 partitions
        tp = TopicPartition('raft-test', partition_id)
        leader = cluster_metadata.leader_for_partition(tp)
        print(f"  Partition {partition_id}: leader_node_id={leader}")

    admin.close()
    print()
