#!/usr/bin/env python3
"""
Phase 7 Baseline Test - Option 4: WAL-Only Metadata
Tests basic replication with 1000 messages, acks=all across 3 nodes
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.structs import TopicPartition
import time
import uuid

def main():
    topic = f'baseline-test-{uuid.uuid4().hex[:8]}'
    print('=' * 60)
    print('Phase 7 Baseline Test')
    print('=' * 60)
    print(f'Topic: {topic}')
    print(f'Messages: 1000')
    print(f'Acks: all')
    print(f'Partitions: 3')
    print(f'Replication: 3')
    print()

    # Produce 1000 messages with acks=all
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
        acks='all',
        api_version=(0, 10, 0),
        request_timeout_ms=10000
    )

    print('Producing 1000 messages with acks=all...')
    start_time = time.time()
    for i in range(1000):
        future = producer.send(topic, f'message-{i}'.encode(), partition=i % 3)
        if i % 100 == 0:
            print(f'  Produced {i} messages...')

    producer.flush()
    producer.close()
    produce_time = time.time() - start_time
    print(f'✓ Produced 1000 messages in {produce_time:.2f}s ({1000/produce_time:.0f} msg/s)')
    print()

    # Give replication a moment
    time.sleep(2)

    # Verify replication on all 3 nodes
    print('Verifying replication across all 3 nodes...')
    all_good = True
    for port in [9092, 9093, 9094]:
        try:
            consumer = KafkaConsumer(
                bootstrap_servers=[f'localhost:{port}'],
                api_version=(0, 10, 0),
                auto_offset_reset='earliest',
                consumer_timeout_ms=5000
            )

            # Check all 3 partitions
            partitions = [TopicPartition(topic, p) for p in range(3)]
            consumer.assign(partitions)

            # Get end offsets
            end_offsets = consumer.end_offsets(partitions)
            total_messages = sum(end_offsets.values())

            consumer.close()

            print(f'  Node {port}: {total_messages}/1000 messages')
            if total_messages == 1000:
                print(f'    ✓ All 1000 messages replicated')
            else:
                print(f'    ✗ Expected 1000, got {total_messages}')
                all_good = False
        except Exception as e:
            print(f'  Node {port}: ERROR - {e}')
            all_good = False

    print()
    print('=' * 60)
    if all_good:
        print('✓ BASELINE TEST PASSED')
    else:
        print('✗ BASELINE TEST FAILED')
    print('=' * 60)
    return 0 if all_good else 1

if __name__ == '__main__':
    exit(main())
