#!/usr/bin/env python3
"""Final comprehensive compression test"""
from kafka import KafkaProducer, KafkaConsumer
import time

def test_compression(name, compression_type):
    """Test a specific compression type"""
    print(f"\n{'='*60}")
    print(f"Testing: {name.upper()}")
    print(f"{'='*60}")

    topic = f'final-{name}'

    # Produce
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        compression_type=compression_type,
        api_version=(0, 10, 0)
    )

    print(f"Producing 10 messages with {name}...")
    for i in range(10):
        msg = f"Message {i}"
        producer.send(topic, msg.encode('utf-8'))

    producer.flush()
    producer.close()
    time.sleep(2)

    # Consume
    print(f"Consuming from {topic}...")
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers='localhost:9092',
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000,
        api_version=(0, 10, 0)
    )

    messages = []
    for msg in consumer:
        try:
            decoded = msg.value.decode('utf-8')
            messages.append(decoded)
            print(f"  ✓ Offset {msg.offset} (p{msg.partition}): {decoded}")
        except Exception as e:
            print(f"  ✗ Offset {msg.offset} (p{msg.partition}): DECODE ERROR - {e}")
            return False

    consumer.close()

    # Count unique messages (avoid counting duplicates from multiple partitions)
    unique = len(set(messages))

    if unique >= 10:
        print(f"✅ SUCCESS: {unique} unique messages")
        return True
    else:
        print(f"❌ FAILED: Only {unique} unique messages")
        return False

print("╔════════════════════════════════════════════════════════════╗")
print("║     FINAL COMPRESSION TEST - CLEAN ENVIRONMENT             ║")
print("╚════════════════════════════════════════════════════════════╝")

results = {}
results['uncompressed'] = test_compression('uncompressed', None)
results['gzip'] = test_compression('gzip', 'gzip')
results['snappy'] = test_compression('snappy', 'snappy')
results['lz4'] = test_compression('lz4', 'lz4')

print(f"\n{'='*60}")
print("FINAL RESULTS")
print(f"{'='*60}")
for name, success in results.items():
    status = "✅ PASS" if success else "❌ FAIL"
    print(f"  {name:15s}: {status}")
print(f"{'='*60}")

passed = sum(1 for v in results.values() if v)
total = len(results)
print(f"\nPassed: {passed}/{total}")

if passed == total:
    print("\n🎉 ALL COMPRESSION TESTS PASSED!")
else:
    print(f"\n⚠️  {total - passed} test(s) failed")
