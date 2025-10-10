#!/usr/bin/env python3
"""
Final test for compression support - all codecs
"""
from kafka import KafkaProducer, KafkaConsumer
import time

def test_codec(codec_name):
    """Test a specific compression codec"""
    print(f"\n{'='*60}")
    print(f"Testing: {codec_name.upper()}")
    print(f"{'='*60}")

    topic = f"compression-final-{codec_name}"

    try:
        # Create producer
        if codec_name == "none":
            producer = KafkaProducer(
                bootstrap_servers='localhost:9092',
                api_version=(0, 10, 0),
                linger_ms=0  # Send immediately
            )
        else:
            producer = KafkaProducer(
                bootstrap_servers='localhost:9092',
                compression_type=codec_name,
                api_version=(0, 10, 0) if codec_name != 'zstd' else (2, 1, 0),
                linger_ms=0  # Send immediately
            )

        # Send messages
        messages = [f"{codec_name} message {i}" for i in range(5)]

        print(f"Sending {len(messages)} messages...")
        for i, msg in enumerate(messages):
            producer.send(topic, msg.encode('utf-8'))

        producer.flush()
        producer.close()
        print("✓ All messages sent successfully")

        time.sleep(2)

        # Consume
        print("Consuming...")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000,
            api_version=(0, 10, 0) if codec_name != 'zstd' else (2, 1, 0)
        )

        consumed = []
        for msg in consumer:
            consumed.append(msg.value.decode('utf-8'))

        consumer.close()

        print(f"✓ Consumed {len(consumed)}/{len(messages)} messages")

        if len(consumed) == len(messages):
            print(f"✅ {codec_name.upper()}: WORKING!")
            return True
        else:
            print(f"⚠️  {codec_name.upper()}: Partial success ({len(consumed)}/{len(messages)} messages)")
            return False

    except Exception as e:
        print(f"❌ {codec_name.upper()}: {e}")
        return False

def main():
    codecs = [
        ('gzip', True),  # (name, should_work)
        ('snappy', True),
    ]

    # Test if client has lz4 and zstd support
    try:
        import lz4
        codecs.append(('lz4', True))
    except ImportError:
        print("⚠️  lz4 library not installed, skipping lz4 test")

    try:
        import zstandard
        codecs.append(('zstd', True))
    except ImportError:
        print("⚠️  zstandard library not installed, skipping zstd test")

    print("""
╔════════════════════════════════════════════════════════════╗
║       Chronik Compression Support - Final Test             ║
╚════════════════════════════════════════════════════════════╝
    """)

    results = {}
    for codec, _ in codecs:
        results[codec] = test_codec(codec)
        time.sleep(1)

    # Add "none" test
    results['none'] = test_codec('none')

    # Summary
    print(f"\n{'='*60}")
    print("SUMMARY")
    print(f"{'='*60}")
    for codec, status in results.items():
        icon = "✅" if status else "❌"
        print(f"  {icon} {codec.upper():10s}")

    supported = sum(1 for v in results.values() if v)
    total = len(results)
    print(f"\n{supported}/{total} codecs working")

if __name__ == '__main__':
    main()
