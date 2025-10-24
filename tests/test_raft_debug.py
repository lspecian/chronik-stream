#!/usr/bin/env python3
"""Debug Raft cluster produce errors."""

from kafka import KafkaProducer
from kafka.admin import KafkaAdminClient
import time

print("Fetching metadata for raft-test topic...")
admin = KafkaAdminClient(bootstrap_servers='localhost:9092')

# Get topic metadata
from kafka.structs import TopicPartition
cluster_metadata = admin._client.cluster
for partition_id in cluster_metadata.partitions_for_topic('raft-test') or []:
    tp = TopicPartition('raft-test', partition_id)
    leader = cluster_metadata.leader_for_partition(tp)
    print(f"  Partition {partition_id}: leader_node_id={leader}")

admin.close()

print("\nProducing single message with debug...")
producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    acks='all',
    retries=0,  # Fail fast
    api_version=(0, 10, 0)
)

try:
    future = producer.send('raft-test', b'debug message')
    result = future.get(timeout=10)
    print(f"✅ Success: partition={result.partition}, offset={result.offset}")
except Exception as e:
    print(f"❌ Error: {e}")
    print(f"   Error type: {type(e).__name__}")

producer.close()
