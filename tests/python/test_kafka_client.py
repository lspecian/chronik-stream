#!/usr/bin/env python3
"""
Test using kafka-python library to connect to real Kafka.
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient
import json

def test_real_kafka():
    """Test connection to real Kafka."""
    print("Testing connection to real Kafka on port 9093...")
    
    try:
        # Create admin client to get metadata
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9093',
            client_id='test-python-client',
            request_timeout_ms=5000
        )
        
        # Get cluster metadata
        metadata = admin._client.cluster
        print(f"Connected to Kafka cluster with {len(metadata.brokers())} broker(s)")
        
        for broker in metadata.brokers():
            print(f"  Broker {broker.nodeId}: {broker.host}:{broker.port}")
        
        # List topics
        topics = admin.list_topics()
        print(f"\nFound {len(topics)} topic(s):")
        for topic in topics:
            print(f"  - {topic}")
        
        admin.close()
        return True
        
    except Exception as e:
        print(f"Failed to connect to Kafka: {e}")
        return False

def test_chronik():
    """Test connection to Chronik."""
    print("\nTesting connection to Chronik on port 9092...")
    
    try:
        # Create admin client
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='test-python-client',
            request_timeout_ms=5000
        )
        
        # Get cluster metadata
        metadata = admin._client.cluster
        print(f"Connected to Chronik with {len(metadata.brokers())} broker(s)")
        
        for broker in metadata.brokers():
            print(f"  Broker {broker.nodeId}: {broker.host}:{broker.port}")
        
        admin.close()
        return True
        
    except Exception as e:
        print(f"Failed to connect to Chronik: {e}")
        return False

if __name__ == "__main__":
    kafka_ok = test_real_kafka()
    chronik_ok = test_chronik()
    
    print("\n" + "="*60)
    print("RESULTS:")
    print(f"  Real Kafka: {'✓ OK' if kafka_ok else '✗ Failed'}")
    print(f"  Chronik:    {'✓ OK' if chronik_ok else '✗ Failed'}")
    print("="*60)