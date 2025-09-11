#!/usr/bin/env python3
"""Test Fetch API with simpler client setup"""

import sys
import time
import uuid
from kafka import KafkaProducer, SimpleClient
from kafka.protocol.fetch import FetchRequest_v0, FetchResponse_v0
from kafka.protocol.produce import ProduceRequest_v2, ProduceResponse_v2
from kafka.protocol.metadata import MetadataRequest_v0
import struct

def test_direct_fetch():
    """Test Fetch API using low-level protocol"""
    broker = 'localhost:9092'
    topic = f'test-fetch-direct-{uuid.uuid4().hex[:8]}'
    
    print(f"Testing direct Fetch API with topic: {topic}")
    
    # First produce some messages
    try:
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0)
        )
        
        # Send test messages
        for i in range(3):
            message = f"Test message {i}".encode('utf-8')
            future = producer.send(topic, value=message, partition=0)
            metadata = future.get(timeout=10)
            print(f"✓ Sent message {i} to partition {metadata.partition} at offset {metadata.offset}")
        
        producer.flush()
        producer.close()
        print("✓ Messages produced successfully")
        
    except Exception as e:
        print(f"✗ Failed to produce messages: {e}")
        return False
    
    # Give server a moment to process
    time.sleep(1)
    
    # Now try to fetch directly
    try:
        client = SimpleClient(broker)
        
        # Get metadata first to ensure topic exists
        metadata_req = MetadataRequest_v0([topic])
        metadata_resp = client.send(metadata_req)
        print(f"✓ Got metadata response with {len(metadata_resp[0].topics)} topics")
        
        # Build and send Fetch request
        fetch_req = FetchRequest_v0(
            replica_id=-1,
            max_wait_time=1000,  # 1 second
            min_bytes=1,
            topics=[(topic, [(0, 0, 1024)])]  # Fetch partition 0 from offset 0
        )
        
        print("Sending Fetch request...")
        fetch_resp = client.send(fetch_req)
        
        if fetch_resp:
            print(f"✓ Got Fetch response")
            for topic_data in fetch_resp.topics:
                print(f"  Topic: {topic_data[0]}")
                for partition_data in topic_data[1]:
                    partition, error, highwater, messages = partition_data
                    print(f"    Partition {partition}: error={error}, highwater={highwater}, messages={len(messages)}")
                    if messages:
                        print("✓ Successfully fetched messages!")
                        return True
                    else:
                        print("✗ No messages in Fetch response")
                        return False
        else:
            print("✗ No Fetch response received")
            return False
            
    except Exception as e:
        print(f"✗ Failed during fetch: {e}")
        import traceback
        traceback.print_exc()
        return False
    finally:
        try:
            client.close()
        except:
            pass

def main():
    print("=" * 60)
    print("Direct Fetch API Test")
    print("=" * 60)
    
    success = test_direct_fetch()
    
    print("=" * 60)
    if success:
        print("✓ Direct Fetch test PASSED")
        sys.exit(0)
    else:
        print("✗ Direct Fetch test FAILED")
        sys.exit(1)

if __name__ == "__main__":
    main()