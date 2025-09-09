#!/usr/bin/env python3
"""
Test script to verify advertised address configuration works correctly.
"""

import sys
import time
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import NoBrokersAvailable
import json

def test_connection(bootstrap_servers='localhost:9092'):
    """Test Kafka client connection with advertised address."""
    print(f"Testing connection to {bootstrap_servers}...")
    
    try:
        # Create producer with explicit API version
        producer = KafkaProducer(
            bootstrap_servers=[bootstrap_servers],
            api_version=(0, 10, 0),
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            request_timeout_ms=10000,
            metadata_max_age_ms=5000
        )
        
        print("✓ Producer created successfully")
        
        # Send a test message
        future = producer.send('test-topic', value={"test": "message", "timestamp": time.time()})
        
        # Wait for message to be sent
        result = future.get(timeout=10)
        print(f"✓ Message sent successfully: {result}")
        
        # Flush to ensure message is sent
        producer.flush()
        print("✓ Producer flushed successfully")
        
        # Create consumer
        consumer = KafkaConsumer(
            'test-topic',
            bootstrap_servers=[bootstrap_servers],
            api_version=(0, 10, 0),
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000
        )
        
        print("✓ Consumer created successfully")
        
        # Try to consume the message
        message_found = False
        for message in consumer:
            print(f"✓ Message consumed: {message.value}")
            message_found = True
            break
        
        if message_found:
            print("\n✅ SUCCESS: Chronik Stream is working correctly with advertised address!")
        else:
            print("\n⚠️  WARNING: Connection works but no messages were consumed")
        
        # Close connections
        producer.close()
        consumer.close()
        
        return True
        
    except NoBrokersAvailable as e:
        print(f"\n❌ ERROR: No brokers available - {e}")
        print("\nThis usually means:")
        print("1. Chronik Stream is not running")
        print("2. The advertised address is incorrect (e.g., 0.0.0.0)")
        print("3. The port is blocked by a firewall")
        return False
        
    except Exception as e:
        print(f"\n❌ ERROR: {type(e).__name__}: {e}")
        return False

def main():
    print("=" * 60)
    print("Chronik Stream Advertised Address Test")
    print("=" * 60)
    
    # Test default connection
    if test_connection():
        print("\n" + "=" * 60)
        print("Test completed successfully!")
        print("=" * 60)
        sys.exit(0)
    else:
        print("\n" + "=" * 60)
        print("Test failed!")
        print("\nTo fix this, start Chronik Stream with:")
        print("  cargo run --bin chronik-server -- --advertised-addr localhost")
        print("Or set environment variable:")
        print("  CHRONIK_ADVERTISED_ADDR=localhost cargo run --bin chronik-server")
        print("=" * 60)
        sys.exit(1)

if __name__ == "__main__":
    main()