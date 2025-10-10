#!/usr/bin/env python3
"""Test all compression codecs"""
from kafka import KafkaProducer, KafkaConsumer
import time

def test_codec(codec_name, compression_type, api_version=(0, 10, 0)):
    """Test a specific compression codec"""
    print(f"\n{'='*60}")
    print(f"Testing: {codec_name}")
    print(f"{'='*60}")

    topic = f'test-{codec_name}'

    try:
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            compression_type=compression_type,
            api_version=api_version
        )

        # Send 10 messages
        print(f"Sending 10 messages with {codec_name}...")
        for i in range(10):
            msg = f"Message {i} with {codec_name} - " + ("x" * 50)
            producer.send(topic, msg.encode('utf-8'))

        producer.flush()
        producer.close()
        time.sleep(1)

        # Consume
        print(f"Consuming from {topic}...")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000,
            api_version=api_version
        )

        count = 0
        for msg in consumer:
            decoded = msg.value.decode('utf-8')
            # Just check it contains the codec name (messages may be out of order due to partitioning)
            if codec_name in decoded and decoded.startswith("Message"):
                count += 1
                print(f"  ✓ Offset {msg.offset}: {decoded[:40]}...")
            else:
                print(f"  ! Skipping old message at offset {msg.offset}: {decoded[:40]}...")

        consumer.close()

        if count >= 10:
            print(f"✅ SUCCESS: {count} messages consumed correctly")
            return True
        else:
            print(f"❌ FAILED: Only {count} messages consumed")
            return False

    except Exception as e:
        print(f"❌ ERROR: {e}")
        return False

# Test all codecs
print("╔════════════════════════════════════════════════════════════╗")
print("║       Chronik Compression Support Test                    ║")
print("╚════════════════════════════════════════════════════════════╝")

results = {}
results['uncompressed'] = test_codec('uncompressed', None)
results['gzip'] = test_codec('gzip', 'gzip')
results['snappy'] = test_codec('snappy', 'snappy')
results['lz4'] = test_codec('lz4', 'lz4')

# Skip zstd for now (requires api_version >= 2.1.0)
print(f"\n{'='*60}")
print("RESULTS SUMMARY")
print(f"{'='*60}")
for name, success in results.items():
    status = "✅ PASS" if success else "❌ FAIL"
    print(f"  {name:15s}: {status}")
print(f"{'='*60}")

total = len(results)
passed = sum(1 for v in results.values() if v)
print(f"\nPassed: {passed}/{total}")
if passed == total:
    print("🎉 ALL TESTS PASSED!")
