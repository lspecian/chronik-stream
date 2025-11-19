#!/usr/bin/env python3
"""
Create 3000 topics in parallel to stress test Chronik cluster.
"""
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from kafka.admin import KafkaAdminClient, NewTopic
from kafka.errors import TopicAlreadyExistsError

def create_topic(topic_num):
    """Create a single topic."""
    topic_name = f"stress-test-{topic_num:04d}"
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
            request_timeout_ms=60000,
            api_version=(0, 10, 0)
        )

        new_topic = NewTopic(
            name=topic_name,
            num_partitions=3,
            replication_factor=3
        )

        start = time.time()
        admin.create_topics([new_topic], timeout_ms=60000)
        elapsed = time.time() - start
        admin.close()

        return {'topic': topic_name, 'status': 'created', 'time': elapsed}
    except TopicAlreadyExistsError:
        return {'topic': topic_name, 'status': 'exists', 'time': 0}
    except Exception as e:
        return {'topic': topic_name, 'status': 'failed', 'error': str(e), 'time': 0}

def main():
    print(f"Creating 3000 topics in parallel...")
    print(f"Started at: {time.strftime('%Y-%m-%d %H:%M:%S')}")

    start_time = time.time()

    # Use 100 concurrent threads for parallel creation
    with ThreadPoolExecutor(max_workers=100) as executor:
        futures = [executor.submit(create_topic, i) for i in range(3000)]

        created = 0
        existed = 0
        failed = 0
        total_time = 0

        for i, future in enumerate(as_completed(futures)):
            result = future.result()

            if result['status'] == 'created':
                created += 1
                total_time += result['time']
            elif result['status'] == 'exists':
                existed += 1
            else:
                failed += 1
                print(f"FAILED: {result['topic']} - {result.get('error', 'Unknown error')}")

            # Progress update every 100 topics
            if (i + 1) % 100 == 0:
                elapsed = time.time() - start_time
                rate = (i + 1) / elapsed
                print(f"Progress: {i+1}/3000 topics ({rate:.1f} topics/s) - "
                      f"Created: {created}, Existed: {existed}, Failed: {failed}")

    elapsed_time = time.time() - start_time
    avg_create_time = total_time / created if created > 0 else 0

    print(f"\n{'='*60}")
    print(f"RESULTS:")
    print(f"{'='*60}")
    print(f"Total time:        {elapsed_time:.2f} seconds")
    print(f"Topics created:    {created}")
    print(f"Topics existed:    {existed}")
    print(f"Topics failed:     {failed}")
    print(f"Creation rate:     {3000/elapsed_time:.1f} topics/second")
    print(f"Avg create time:   {avg_create_time*1000:.1f} ms")
    print(f"Finished at:       {time.strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"{'='*60}")

if __name__ == '__main__':
    main()
