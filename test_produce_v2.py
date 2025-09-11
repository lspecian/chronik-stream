#!/usr/bin/env python3
"""Test ProduceResponse v2 with explicit API version."""

import os
import sys
import time
import json
from kafka import KafkaProducer
from kafka.errors import KafkaTimeoutError

def test_produce_v2():
    """Test producing with API version 2 to check log_append_time field."""
    
    print("Testing ProduceResponse v2...")
    print("-" * 60)
    
    try:
        # Create producer with explicit API version 2
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            api_version=(0, 10, 0),  # This forces ProduceRequest/Response v2
            request_timeout_ms=5000,
            max_block_ms=5000
        )
        print("✓ Producer created with API version (0, 10, 0) - uses v2")
        
        # Send a test message
        message = {'test': 'ProduceResponse v2 test', 'timestamp': time.time()}
        print(f"\nSending message: {message}")
        
        future = producer.send('test-topic', message)
        
        # Wait for acknowledgment
        try:
            result = future.get(timeout=5)
            print(f"✓ Message sent successfully!")
            print(f"  Topic: {result.topic}")
            print(f"  Partition: {result.partition}")
            print(f"  Offset: {result.offset}")
            if hasattr(result, 'timestamp'):
                print(f"  Timestamp: {result.timestamp}")
            print("\n✅ ProduceResponse v2 is working correctly!")
            return True
            
        except KafkaTimeoutError as e:
            print(f"✗ Timeout waiting for acknowledgment: {e}")
            print("\n❌ ProduceResponse v2 is NOT working - missing log_append_time field?")
            return False
            
    except Exception as e:
        print(f"✗ Error: {e}")
        return False
    finally:
        try:
            producer.close()
        except:
            pass

if __name__ == "__main__":
    # Set environment to see debug output
    os.environ['RUST_LOG'] = 'debug'
    
    success = test_produce_v2()
    sys.exit(0 if success else 1)