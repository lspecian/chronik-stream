#!/usr/bin/env python3
"""
Create 3000 topics using SINGLE admin client with batch creation.
Tests Chronik's ability to handle many topics efficiently.
"""
import time
from kafka.admin import KafkaAdminClient, NewTopic
from kafka.errors import TopicAlreadyExistsError

def main():
    print(f"Creating 3000 topics in batches of 100...")
    print(f"Started at: {time.strftime('%Y-%m-%d %H:%M:%S')}")

    start_time = time.time()

    # Create SINGLE admin client (reused for all requests)
    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
        request_timeout_ms=60000,
        api_version=(0, 10, 0)
    )

    created = 0
    existed = 0
    failed = 0
    batch_size = 100

    try:
        for batch_start in range(0, 3000, batch_size):
            batch_end = min(batch_start + batch_size, 3000)
            batch_topics = []

            # Prepare batch of topics
            for i in range(batch_start, batch_end):
                topic_name = f"stress-test-{i:04d}"
                new_topic = NewTopic(
                    name=topic_name,
                    num_partitions=3,
                    replication_factor=3
                )
                batch_topics.append(new_topic)

            # Create batch (single request for 100 topics)
            batch_start_time = time.time()
            try:
                admin.create_topics(batch_topics, timeout_ms=60000)
                batch_elapsed = time.time() - batch_start_time
                created += len(batch_topics)
                print(f"Batch {batch_start//batch_size + 1}/30: Created {len(batch_topics)} topics in {batch_elapsed:.2f}s")
            except TopicAlreadyExistsError:
                existed += len(batch_topics)
                print(f"Batch {batch_start//batch_size + 1}/30: Topics already exist")
            except Exception as e:
                failed += len(batch_topics)
                print(f"Batch {batch_start//batch_size + 1}/30: FAILED - {e}")

    finally:
        admin.close()

    elapsed_time = time.time() - start_time

    print(f"\n{'='*60}")
    print(f"RESULTS:")
    print(f"{'='*60}")
    print(f"Total time:        {elapsed_time:.2f} seconds")
    print(f"Topics created:    {created}")
    print(f"Topics existed:    {existed}")
    print(f"Topics failed:     {failed}")
    print(f"Creation rate:     {3000/elapsed_time:.1f} topics/second")
    print(f"Finished at:       {time.strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"{'='*60}")

if __name__ == '__main__':
    main()
