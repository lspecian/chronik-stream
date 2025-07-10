#!/usr/bin/env python3
"""Create topic metadata directly in TiKV for testing."""

import asyncio
import json
import sys
from datetime import datetime
from tikv_client import TransactionClient

async def create_topic(topic_name, partitions=1, replication_factor=1):
    """Create topic metadata in TiKV."""
    
    # Connect to TiKV
    client = await TransactionClient.connect(["pd0:2379"])
    
    try:
        # Topic metadata
        topic_meta = {
            "name": topic_name,
            "id": f"topic-{topic_name}",
            "created_at": datetime.utcnow().isoformat(),
            "config": {
                "partition_count": partitions,
                "replication_factor": replication_factor,
                "retention_ms": 7 * 24 * 60 * 60 * 1000,  # 7 days
                "segment_ms": 60 * 60 * 1000,  # 1 hour
            }
        }
        
        # Create transaction
        txn = await client.begin()
        
        # Store topic metadata
        topic_key = f"metadata/topics/{topic_name}".encode()
        await txn.put(topic_key, json.dumps(topic_meta).encode())
        
        # Create partition metadata
        for partition in range(partitions):
            partition_meta = {
                "topic": topic_name,
                "partition": partition,
                "leader": 1,  # Node ID 1
                "replicas": [1],
                "isr": [1],
                "offset": 0,
                "log_start_offset": 0,
                "segment_count": 0,
            }
            
            partition_key = f"metadata/partitions/{topic_name}/{partition}".encode()
            await txn.put(partition_key, json.dumps(partition_meta).encode())
        
        # Commit transaction
        await txn.commit()
        
        print(f"Topic '{topic_name}' created successfully with {partitions} partitions")
        return True
        
    except Exception as e:
        print(f"Error creating topic: {e}")
        return False
    finally:
        await client.close()

def main():
    if len(sys.argv) < 2:
        print("Usage: create_topic_direct.py <topic_name> [partitions] [replication_factor]")
        sys.exit(1)
    
    topic_name = sys.argv[1]
    partitions = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    replication_factor = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    
    # Run async function
    success = asyncio.run(create_topic(topic_name, partitions, replication_factor))
    sys.exit(0 if success else 1)

if __name__ == "__main__":
    main()