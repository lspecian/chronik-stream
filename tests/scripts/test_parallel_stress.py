#!/usr/bin/env python3
"""
Parallelized aggressive stress test for Chronik Raft cluster.
Creates topics concurrently across multiple threads to maximize load.
"""

import sys
import time
from kafka import KafkaProducer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import TopicAlreadyExistsError, KafkaError
from concurrent.futures import ThreadPoolExecutor, as_completed
import threading

# ANSI color codes
GREEN = '\033[0;32m'
RED = '\033[0;31m'
YELLOW = '\033[1;33m'
BLUE = '\033[0;34m'
CYAN = '\033[0;36m'
RESET = '\033[0m'

results_lock = threading.Lock()
success_count = 0
failed_count = 0

def create_and_produce_topic(args):
    """Create a topic and produce a message to it."""
    bootstrap_server, topic_num, total_topics = args

    global success_count, failed_count

    topic_name = f"stress-parallel-{topic_num}-{int(time.time())}"

    try:
        # Create admin client
        admin = KafkaAdminClient(
            bootstrap_servers=bootstrap_server,
            client_id=f"stress-admin-{topic_num}",
            request_timeout_ms=30000,
            api_version=(0, 10, 0)
        )

        # Create topic with 3 partitions, replication factor 3
        topic = NewTopic(
            name=topic_name,
            num_partitions=3,
            replication_factor=3
        )

        admin.create_topics([topic], timeout_ms=30000)
        admin.close()

        # Give a moment for topic creation to propagate
        time.sleep(0.5)

        # Produce a test message
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_server,
            client_id=f"stress-producer-{topic_num}",
            request_timeout_ms=30000,
            api_version=(0, 10, 0)
        )

        message = f"Parallel stress test message #{topic_num}"
        future = producer.send(topic_name, message.encode('utf-8'))
        future.get(timeout=30)

        producer.close()

        with results_lock:
            success_count += 1
            print(f"  {GREEN}✓{RESET} Test {success_count + failed_count}/{total_topics}: SUCCESS (created and produced to {topic_name})")

        return True, topic_name

    except TopicAlreadyExistsError:
        with results_lock:
            success_count += 1
            print(f"  {GREEN}✓{RESET} Test {success_count + failed_count}/{total_topics}: SUCCESS (topic {topic_name} already exists)")
        return True, topic_name

    except Exception as e:
        with results_lock:
            failed_count += 1
            print(f"  {RED}✗{RESET} Test {success_count + failed_count}/{total_topics}: FAILED ({topic_name}) - {str(e)[:100]}")
        return False, topic_name

def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <bootstrap_server> <num_topics> [num_workers]")
        print(f"Example: {sys.argv[0]} localhost:9092 1000 10")
        sys.exit(1)

    bootstrap_server = sys.argv[1]
    num_topics = int(sys.argv[2])
    num_workers = int(sys.argv[3]) if len(sys.argv) > 3 else 10  # Default: 10 parallel workers

    print(f"\n{YELLOW}🔥 AGGRESSIVE PARALLEL STRESS TEST 🔥{RESET}")
    print(f"Bootstrap server: {bootstrap_server}")
    print(f"Number of topics: {num_topics}")
    print(f"Parallel workers: {num_workers}")
    print(f"Total Raft groups: {num_topics * 3} (3 partitions × {num_topics} topics)")
    print(f"\n{CYAN}Kafka guideline: 100 × 3 brokers × 3 replication = 900 partitions{RESET}")
    print(f"{CYAN}This test: {num_topics * 3} partitions = {int((num_topics * 3 / 900) * 100)}% of guideline{RESET}\n")

    start_time = time.time()

    # Create thread pool and submit all tasks
    with ThreadPoolExecutor(max_workers=num_workers) as executor:
        # Submit all topic creation tasks
        futures = []
        for i in range(num_topics):
            future = executor.submit(create_and_produce_topic, (bootstrap_server, i, num_topics))
            futures.append(future)

        # Wait for all to complete
        for future in as_completed(futures):
            try:
                future.result()
            except Exception as e:
                print(f"  {RED}✗{RESET} Unexpected error: {e}")

    end_time = time.time()
    duration = end_time - start_time

    # Final summary
    print(f"\n{'━' * 60}")
    print(f"{BLUE}Summary:{RESET}")
    print(f"  Total: {num_topics}")
    print(f"  {GREEN}✓ Success: {success_count}{RESET}")
    print(f"  {RED}✗ Failed: {failed_count}{RESET}")
    print(f"  Success rate: {(success_count / num_topics * 100):.1f}%")
    print(f"  Duration: {duration:.1f}s")
    print(f"  Throughput: {(num_topics / duration):.1f} topics/sec")
    print(f"{'━' * 60}\n")

    # Exit with error if any failures
    sys.exit(0 if failed_count == 0 else 1)

if __name__ == '__main__':
    main()
