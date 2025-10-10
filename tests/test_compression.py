#!/usr/bin/env python3
"""
Test script for compression support in Chronik
Tests all compression codecs: none, gzip, snappy, lz4, zstd
"""
import sys
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import time

def test_compression(compression_type):
    """Test a specific compression type"""
    print(f"\n{'='*60}")
    print(f"Testing compression: {compression_type}")
    print(f"{'='*60}")

    topic = f"test-compression-{compression_type}"

    try:
        # Create producer with compression
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            compression_type=compression_type,
            api_version=(0, 10, 0)
        )

        # Send test messages
        test_messages = [
            f"Test message {i} with {compression_type} compression - " + ("x" * 100)
            for i in range(10)
        ]

        print(f"Sending {len(test_messages)} messages with {compression_type} compression...")
        for i, msg in enumerate(test_messages):
            future = producer.send(topic, msg.encode('utf-8'))
            try:
                record_metadata = future.get(timeout=10)
                print(f"  ✓ Message {i} sent to partition {record_metadata.partition} at offset {record_metadata.offset}")
            except KafkaError as e:
                print(f"  ✗ Message {i} failed: {e}")
                return False

        producer.flush()
        producer.close()

        # Wait a moment for messages to be persisted
        time.sleep(1)

        # Try to consume messages
        print(f"\nConsuming messages from topic {topic}...")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000,
            api_version=(0, 10, 0)
        )

        consumed_count = 0
        for message in consumer:
            consumed_count += 1
            print(f"  ✓ Consumed message at offset {message.offset}: {message.value[:50].decode('utf-8')}...")

        consumer.close()

        if consumed_count == len(test_messages):
            print(f"\n✅ SUCCESS: {compression_type} compression works! ({consumed_count}/{len(test_messages)} messages)")
            return True
        else:
            print(f"\n❌ FAILED: Only consumed {consumed_count}/{len(test_messages)} messages")
            return False

    except Exception as e:
        print(f"\n❌ ERROR: {compression_type} compression failed with exception: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    """Test all compression types"""
    print("""
╔════════════════════════════════════════════════════════════╗
║       Chronik Compression Support Test Suite              ║
╚════════════════════════════════════════════════════════════╝
""")

    compression_types = ['none', 'gzip', 'snappy', 'lz4', 'zstd']
    results = {}

    for comp_type in compression_types:
        results[comp_type] = test_compression(comp_type)
        time.sleep(1)  # Brief pause between tests

    # Print summary
    print(f"\n{'='*60}")
    print("COMPRESSION SUPPORT SUMMARY")
    print(f"{'='*60}")

    for comp_type, success in results.items():
        status = "✅ SUPPORTED" if success else "❌ NOT SUPPORTED"
        print(f"  {comp_type:10s} : {status}")

    print(f"{'='*60}")

    supported_count = sum(1 for v in results.values() if v)
    total_count = len(results)

    print(f"\nSupported codecs: {supported_count}/{total_count}")

    if supported_count == total_count:
        print("\n🎉 ALL COMPRESSION CODECS SUPPORTED!")
        return 0
    else:
        print(f"\n⚠️  {total_count - supported_count} codec(s) not supported")
        return 1

if __name__ == '__main__':
    sys.exit(main())
