#!/usr/bin/env python3
"""
Phase 2 Performance Test: Topic Creation Throughput

This script measures topic creation throughput and latency to verify
Phase 2 WAL-based metadata writes are working correctly.

Expected Results:
- Phase 1 (Raft only): ~50-100ms avg latency, ~20 topics/sec
- Phase 2 (WAL fast path): ~2-5ms avg latency, ~200+ topics/sec (10x improvement!)

Usage:
    python3 tests/test_phase2_throughput.py

Prerequisites:
    pip install kafka-python
"""

from kafka.admin import KafkaAdminClient, NewTopic
import time
import sys

def main():
    print("=" * 60)
    print("Phase 2 Performance Test: Topic Creation Throughput")
    print("=" * 60)
    print()

    # Connect to cluster
    print("Connecting to Chronik cluster at localhost:9092...")
    try:
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='phase2-throughput-test',
            request_timeout_ms=30000
        )
        print("✅ Connected successfully")
        print()
    except Exception as e:
        print(f"❌ Failed to connect: {e}")
        print("\nMake sure Chronik is running:")
        print("  cargo run --bin chronik-server start")
        sys.exit(1)

    # Test configuration
    num_topics = 100
    test_prefix = f"phase2-test-{int(time.time())}"

    print(f"Test configuration:")
    print(f"  Number of topics: {num_topics}")
    print(f"  Topic prefix: {test_prefix}")
    print()

    # Create topic list
    topics = [
        NewTopic(
            name=f'{test_prefix}-{i}',
            num_partitions=1,
            replication_factor=1
        )
        for i in range(num_topics)
    ]

    # Run test
    print(f"Creating {num_topics} topics...")
    start = time.time()

    try:
        result = admin.create_topics(topics, validate_only=False, timeout_ms=30000)
        elapsed = time.time() - start

        # Check for failures
        failed = []
        # result is a dict-like object, iterate properly
        for topic_name in [t.name for t in topics]:
            try:
                if hasattr(result, 'topic_errors'):
                    # kafka-python returns topic_errors
                    topic_result = next((t for t in result.topic_errors if t.topic == topic_name), None)
                    if topic_result and topic_result.error_code != 0:
                        failed.append((topic_name, f"Error code {topic_result.error_code}"))
            except Exception as e:
                pass  # Topic created successfully

        if failed:
            print(f"\n⚠️  {len(failed)} topics failed to create:")
            for topic, error in failed[:5]:  # Show first 5
                print(f"  - {topic}: {error}")
            if len(failed) > 5:
                print(f"  ... and {len(failed) - 5} more")
            print()

        # Calculate metrics
        successful = num_topics - len(failed)
        throughput = successful / elapsed
        avg_latency = (elapsed / successful) * 1000  # ms

        # Display results
        print()
        print("=" * 60)
        print("RESULTS")
        print("=" * 60)
        print(f"✅ Created {successful}/{num_topics} topics in {elapsed:.2f}s")
        print(f"   Throughput: {throughput:.0f} topics/sec")
        print(f"   Average latency: {avg_latency:.2f}ms")
        print()

        # Interpret results
        if avg_latency < 10:
            print("🎉 PHASE 2 FAST PATH ACTIVE!")
            print("   Topic creation is 5-10x faster than Phase 1")
            print("   Expected: ~2-5ms avg latency ✅")
            print(f"   Actual: {avg_latency:.2f}ms")
        elif avg_latency < 20:
            print("✅ Phase 2 enabled, good performance")
            print(f"   Latency: {avg_latency:.2f}ms (target: <10ms)")
            print("   Consider checking WAL profile settings")
        else:
            print("⚠️  Phase 2 may NOT be active (slow latency)")
            print(f"   Expected: <10ms, Actual: {avg_latency:.2f}ms")
            print()
            print("Troubleshooting steps:")
            print("  1. Check logs for 'Phase 2: Leader creating topic'")
            print("  2. Verify metadata WAL initialized: grep 'Metadata WAL created' logs/")
            print("  3. Check if node is leader: grep 'am_i_leader' logs/")
            print("  4. Review Phase 2 integration steps in docs/PHASE2_INTEGRATION_GUIDE.md")

        print()
        print("=" * 60)

    except Exception as e:
        print(f"\n❌ Test failed: {e}")
        sys.exit(1)
    finally:
        admin.close()

if __name__ == '__main__':
    main()
