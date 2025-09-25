#!/usr/bin/env python3
"""Test kafka-python metadata auto-negotiation."""

from kafka import KafkaProducer, KafkaConsumer
from kafka.protocol.metadata import MetadataRequest, MetadataResponse
from kafka.protocol.api import Request
import struct

def test_kafka_metadata():
    """Test metadata via kafka-python client."""

    # Try to create a simple consumer (which will trigger metadata)
    try:
        print("Creating KafkaConsumer...")
        consumer = KafkaConsumer(
            bootstrap_servers=['localhost:9094'],
            api_version='auto',  # Let it auto-negotiate
            request_timeout_ms=5000,
            metadata_max_age_ms=5000
        )

        print("Consumer created successfully!")

        # Get cluster metadata
        metadata = consumer._client.cluster
        print(f"\nBrokers: {metadata.brokers()}")
        print(f"Topics: {metadata.topics()}")

        # Try to list topics
        topics = consumer.topics()
        print(f"\nAvailable topics: {topics}")

        consumer.close()
        print("\nSUCCESS: kafka-python can now decode Chronik's metadata responses!")

    except Exception as e:
        print(f"FAILED: {e}")
        import traceback
        traceback.print_exc()

if __name__ == "__main__":
    test_kafka_metadata()