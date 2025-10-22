#!/usr/bin/env python3
"""
Simple test to verify wait_for_topic_leaders() callback delay fix.

This test:
1. Creates a topic
2. Immediately tries to produce (tests if metadata API waited properly)
3. Reports success/failure

Expected: 100% success rate with 500ms callback delay
"""

import sys
import time
from kafka import KafkaProducer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import NotLeaderForPartitionError, KafkaError

GREEN = '\033[0;32m'
RED = '\033[0;31m'
YELLOW = '\033[1;33m'
NC = '\033[0m'

def test_immediate_produce(bootstrap_server, num_tests=20):
    """Test producing immediately after topic creation"""

    print(f"{YELLOW}Testing immediate produce after topic creation...{NC}")
    print(f"Bootstrap server: {bootstrap_server}")
    print(f"Number of tests: {num_tests}\n")

    successes = 0
    failures = 0

    for i in range(num_tests):
        topic_name = f"test-immediate-{i}-{int(time.time())}"

        try:
            # Create topic
            admin = KafkaAdminClient(bootstrap_servers=bootstrap_server)
            admin.create_topics([NewTopic(
                name=topic_name,
                num_partitions=3,
                replication_factor=3
            )])
            admin.close()

            # IMMEDIATELY try to produce (no delay)
            producer = KafkaProducer(
                bootstrap_servers=bootstrap_server,
                retries=0,  # No retries - we want to catch the error immediately
                max_in_flight_requests_per_connection=1
            )

            try:
                future = producer.send(topic_name, b'test message')
                result = future.get(timeout=2)

                successes += 1
                print(f"  {GREEN}✓{NC} Test {i+1}/{num_tests}: SUCCESS (produced to {topic_name})")

            except NotLeaderForPartitionError as e:
                failures += 1
                print(f"  {RED}✗{NC} Test {i+1}/{num_tests}: FAILED - NotLeaderForPartitionError")
            except Exception as e:
                failures += 1
                print(f"  {RED}✗{NC} Test {i+1}/{num_tests}: FAILED - {type(e).__name__}: {e}")
            finally:
                producer.close()

        except Exception as e:
            failures += 1
            print(f"  {RED}✗{NC} Test {i+1}/{num_tests}: FAILED (setup) - {e}")

        time.sleep(0.5)  # Small delay between tests

    print(f"\n{YELLOW}Results:{NC}")
    print(f"  Successes: {GREEN}{successes}/{num_tests}{NC} ({100*successes/num_tests:.1f}%)")
    print(f"  Failures:  {RED}{failures}/{num_tests}{NC} ({100*failures/num_tests:.1f}%)")

    if successes == num_tests:
        print(f"\n{GREEN}✓ PERFECT! 100% success rate - callback delay fix works!{NC}")
        return 0
    elif successes >= num_tests * 0.9:
        print(f"\n{YELLOW}⚠ GOOD but not perfect - {100*successes/num_tests:.1f}% success{NC}")
        return 1
    else:
        print(f"\n{RED}✗ FAILED - Only {100*successes/num_tests:.1f}% success{NC}")
        return 1

if __name__ == "__main__":
    bootstrap_server = sys.argv[1] if len(sys.argv) > 1 else "localhost:9092"
    num_tests = int(sys.argv[2]) if len(sys.argv) > 2 else 20

    sys.exit(test_immediate_produce(bootstrap_server, num_tests))
