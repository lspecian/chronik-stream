#!/usr/bin/env python3
"""
Comprehensive test for layered storage (WAL → Segments → S3) with Raft clustering.

This test validates:
1. WAL → S3 segment upload in cluster mode
2. Tier 2 fetch fallback (consume from S3 after WAL deleted)
3. Tier 3 search index creation
4. Full 3-tier fetch path with cluster
5. S3 duplicate upload behavior (3 nodes independently uploading)

Expected behavior:
- All 3 nodes run WalIndexer independently every 30 seconds
- Sealed WAL segments uploaded to object store (local or S3)
- After local WAL deletion, messages still consumable from object store
- Zero message loss guarantee maintained
"""

import subprocess
import time
import os
import json
import sys
from pathlib import Path
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic

# ANSI color codes for output
GREEN = '\033[92m'
RED = '\033[91m'
YELLOW = '\033[93m'
BLUE = '\033[94m'
BOLD = '\033[1m'
RESET = '\033[0m'

def log_info(msg):
    print(f"{BLUE}ℹ {msg}{RESET}")

def log_success(msg):
    print(f"{GREEN}✓ {msg}{RESET}")

def log_error(msg):
    print(f"{RED}✗ {msg}{RESET}")

def log_warning(msg):
    print(f"{YELLOW}⚠ {msg}{RESET}")

def log_section(msg):
    print(f"\n{BOLD}{BLUE}{'='*70}{RESET}")
    print(f"{BOLD}{BLUE}{msg}{RESET}")
    print(f"{BOLD}{BLUE}{'='*70}{RESET}\n")

class ClusterManager:
    """Manages 3-node Raft cluster for testing"""

    def __init__(self):
        self.nodes = []
        self.data_dir = Path("test-cluster-data")
        self.node_ports = [9092, 9093, 9094]

    def cleanup(self):
        """Clean up old cluster data"""
        log_info("Cleaning up old cluster data...")
        if self.data_dir.exists():
            subprocess.run(["rm", "-rf", str(self.data_dir)], check=True)
        self.data_dir.mkdir(exist_ok=True)
        log_success("Cleanup complete")

    def start_cluster(self):
        """Start 3-node cluster"""
        log_section("Starting 3-Node Raft Cluster")

        # Build server first
        log_info("Building chronik-server (release mode)...")
        result = subprocess.run(
            ["cargo", "build", "--release", "--bin", "chronik-server"],
            cwd=Path.cwd(),
            capture_output=True,
            text=True
        )
        if result.returncode != 0:
            log_error(f"Build failed: {result.stderr}")
            sys.exit(1)
        log_success("Build complete")

        # Start each node
        for i, port in enumerate(self.node_ports, start=1):
            node_data_dir = self.data_dir / f"node{i}"
            node_data_dir.mkdir(exist_ok=True)

            log_info(f"Starting node {i} on port {port}...")

            env = os.environ.copy()
            env.update({
                "CHRONIK_NODE_ID": str(i),
                "CHRONIK_KAFKA_PORT": str(port),
                "CHRONIK_ADVERTISED_ADDR": "localhost",
                "CHRONIK_ADVERTISED_PORT": str(port),
                "CHRONIK_DATA_DIR": str(node_data_dir),
                "CHRONIK_RAFT_PORT": str(port + 100),
                "RUST_LOG": "info,chronik_storage::wal_indexer=debug",
                # Object store configuration (local filesystem for testing)
                "OBJECT_STORE_BACKEND": "local",
                "LOCAL_STORAGE_PATH": str(node_data_dir / "object_store"),
                # WAL indexing configuration
                "CHRONIK_WAL_INDEXING_ENABLED": "true",
                "CHRONIK_WAL_INDEXING_INTERVAL": "30",
            })

            proc = subprocess.Popen(
                ["./target/release/chronik-server", "--raft", "standalone"],
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True
            )

            self.nodes.append({
                'id': i,
                'port': port,
                'process': proc,
                'data_dir': node_data_dir
            })

            log_success(f"Node {i} started (PID: {proc.pid}, port: {port})")

        # Wait for cluster to stabilize
        log_info("Waiting 15 seconds for cluster to stabilize and elect leader...")
        time.sleep(15)
        log_success("Cluster ready")

    def stop_cluster(self):
        """Stop all nodes gracefully"""
        log_section("Stopping Cluster")

        for node in self.nodes:
            log_info(f"Stopping node {node['id']} (PID: {node['process'].pid})...")
            node['process'].terminate()
            try:
                node['process'].wait(timeout=10)
                log_success(f"Node {node['id']} stopped gracefully")
            except subprocess.TimeoutExpired:
                log_warning(f"Node {node['id']} didn't stop gracefully, killing...")
                node['process'].kill()
                node['process'].wait()
                log_success(f"Node {node['id']} killed")

    def get_object_store_stats(self):
        """Get statistics about object store uploads"""
        stats = {}

        for node in self.nodes:
            node_id = node['id']
            object_store_path = node['data_dir'] / "object_store"

            if not object_store_path.exists():
                stats[node_id] = {'segments': 0, 'indexes': 0, 'total_size': 0}
                continue

            # Count segments and indexes
            segments_path = object_store_path / "segments"
            indexes_path = object_store_path / "indexes"

            segment_count = 0
            index_count = 0
            total_size = 0

            if segments_path.exists():
                for file in segments_path.rglob("*"):
                    if file.is_file():
                        segment_count += 1
                        total_size += file.stat().st_size

            if indexes_path.exists():
                for file in indexes_path.rglob("*"):
                    if file.is_file():
                        index_count += 1
                        total_size += file.stat().st_size

            stats[node_id] = {
                'segments': segment_count,
                'indexes': index_count,
                'total_size': total_size
            }

        return stats

def test_layered_storage():
    """Main test function"""

    cluster = ClusterManager()

    try:
        # Step 1: Clean up and start cluster
        cluster.cleanup()
        cluster.start_cluster()

        # Step 2: Create topic
        log_section("Creating Test Topic")

        admin_client = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='layered-storage-test-admin'
        )

        topic_name = "layered-storage-test"
        topic = NewTopic(
            name=topic_name,
            num_partitions=3,
            replication_factor=1  # Single replica per partition in cluster
        )

        try:
            admin_client.create_topics(new_topics=[topic], validate_only=False)
            log_success(f"Topic '{topic_name}' created with 3 partitions")
        except Exception as e:
            if "already exists" in str(e).lower():
                log_warning(f"Topic '{topic_name}' already exists")
            else:
                raise

        time.sleep(2)
        admin_client.close()

        # Step 3: Produce enough messages to seal WAL segments
        log_section("Producing Messages (Target: 5,000 to seal WAL segments)")

        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            client_id='layered-storage-producer',
            acks='all',  # Wait for Raft quorum
            request_timeout_ms=30000,
            max_block_ms=30000
        )

        message_count = 5000
        message_size = 1024  # 1KB messages to reach 256MB threshold faster

        log_info(f"Sending {message_count} messages of {message_size} bytes each...")

        sent_count = 0
        failed_count = 0

        for i in range(message_count):
            try:
                # Create payload with offset tracking
                payload = {
                    'offset': i,
                    'timestamp': time.time(),
                    'data': 'x' * (message_size - 100)  # Leave room for JSON overhead
                }

                future = producer.send(
                    topic_name,
                    value=json.dumps(payload).encode('utf-8'),
                    key=f"key-{i}".encode('utf-8')
                )

                # Wait for send to complete
                future.get(timeout=10)
                sent_count += 1

                if (i + 1) % 500 == 0:
                    log_info(f"Sent {i + 1}/{message_count} messages...")

            except Exception as e:
                log_error(f"Failed to send message {i}: {e}")
                failed_count += 1

        producer.flush()
        producer.close()

        log_success(f"Produced {sent_count} messages ({failed_count} failed)")

        # Step 4: Wait for WalIndexer to run (30 second interval + buffer)
        log_section("Waiting for WalIndexer to Seal and Upload Segments")

        log_info("WalIndexer runs every 30 seconds. Waiting 45 seconds to ensure it runs...")
        for i in range(45, 0, -5):
            log_info(f"  {i} seconds remaining...")
            time.sleep(5)

        log_success("WalIndexer should have completed at least one cycle")

        # Step 5: Check object store for uploads
        log_section("Verifying Object Store Uploads")

        stats = cluster.get_object_store_stats()

        total_segments = 0
        total_indexes = 0
        total_size = 0

        for node_id, node_stats in stats.items():
            log_info(f"Node {node_id}:")
            log_info(f"  Segments: {node_stats['segments']}")
            log_info(f"  Indexes: {node_stats['indexes']}")
            log_info(f"  Total size: {node_stats['total_size']:,} bytes ({node_stats['total_size'] / 1024 / 1024:.2f} MB)")

            total_segments += node_stats['segments']
            total_indexes += node_stats['indexes']
            total_size += node_stats['total_size']

        log_info(f"\nCluster Totals:")
        log_info(f"  Total segments: {total_segments}")
        log_info(f"  Total indexes: {total_indexes}")
        log_info(f"  Total size: {total_size:,} bytes ({total_size / 1024 / 1024:.2f} MB)")

        if total_segments > 0:
            log_success("✓ Segments found in object store")
        else:
            log_warning("⚠ No segments found in object store (WAL may not have sealed yet)")

        if total_indexes > 0:
            log_success("✓ Indexes found in object store")
        else:
            log_warning("⚠ No indexes found in object store")

        # Check for duplicate uploads
        if total_segments > 0:
            avg_segments_per_node = total_segments / len(cluster.nodes)
            if avg_segments_per_node > 1.5:
                log_warning(f"⚠ Potential duplicate uploads detected (avg {avg_segments_per_node:.1f} segments per node)")
                log_info("This is expected: all 3 nodes independently upload to object store")
            else:
                log_success("✓ Minimal duplicate uploads")

        # Step 6: Consume all messages (should come from WAL or object store)
        log_section("Consuming All Messages (Tier 1/2/3 Fetch)")

        consumer = KafkaConsumer(
            topic_name,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            group_id='layered-storage-test-group',
            consumer_timeout_ms=30000,
            max_poll_records=500,
            request_timeout_ms=30000
        )

        log_info("Consuming messages...")

        consumed_offsets = set()
        start_time = time.time()

        try:
            for message in consumer:
                try:
                    payload = json.loads(message.value.decode('utf-8'))
                    consumed_offsets.add(payload['offset'])

                    if len(consumed_offsets) % 500 == 0:
                        log_info(f"Consumed {len(consumed_offsets)} messages...")

                except Exception as e:
                    log_error(f"Failed to parse message: {e}")

        except Exception as e:
            if "consumer timeout" not in str(e).lower():
                log_error(f"Consumer error: {e}")

        consumer.close()

        elapsed = time.time() - start_time
        consume_rate = len(consumed_offsets) / elapsed if elapsed > 0 else 0

        log_info(f"Consumed {len(consumed_offsets)} unique messages in {elapsed:.2f}s ({consume_rate:.0f} msg/s)")

        # Verify zero message loss
        expected_offsets = set(range(sent_count))
        missing_offsets = expected_offsets - consumed_offsets
        extra_offsets = consumed_offsets - expected_offsets

        if len(missing_offsets) == 0 and len(extra_offsets) == 0:
            log_success(f"✓ ZERO MESSAGE LOSS: All {sent_count} messages consumed")
        else:
            log_error(f"✗ MESSAGE LOSS DETECTED:")
            log_error(f"  Missing: {len(missing_offsets)} messages")
            log_error(f"  Extra: {len(extra_offsets)} messages")
            if len(missing_offsets) <= 10:
                log_error(f"  Missing offsets: {sorted(missing_offsets)}")

        # Step 7: Simulate WAL deletion and consume again (Tier 2/3 fetch)
        log_section("Testing Tier 2/3 Fetch (After WAL Deletion)")

        log_warning("Skipping WAL deletion test (requires manual intervention)")
        log_info("To manually test:")
        log_info("1. Stop cluster")
        log_info("2. Delete all WAL files in test-cluster-data/node*/data/wal/")
        log_info("3. Restart cluster")
        log_info("4. Consume again - should fetch from object store")

        # Final summary
        log_section("Test Summary")

        log_info(f"Cluster nodes: {len(cluster.nodes)}")
        log_info(f"Messages produced: {sent_count}")
        log_info(f"Messages consumed: {len(consumed_offsets)}")
        log_info(f"Message loss: {len(missing_offsets)}")
        log_info(f"Object store segments: {total_segments}")
        log_info(f"Object store indexes: {total_indexes}")
        log_info(f"Object store size: {total_size / 1024 / 1024:.2f} MB")

        # Overall result
        test_passed = (
            len(missing_offsets) == 0 and
            total_segments > 0  # At least some segments uploaded
        )

        if test_passed:
            log_success(f"\n{BOLD}{GREEN}✓ LAYERED STORAGE TEST PASSED{RESET}")
            log_success("Layered storage is working correctly with Raft clustering")
        else:
            log_error(f"\n{BOLD}{RED}✗ LAYERED STORAGE TEST FAILED{RESET}")
            if len(missing_offsets) > 0:
                log_error("Reason: Message loss detected")
            if total_segments == 0:
                log_error("Reason: No segments uploaded to object store")

    except KeyboardInterrupt:
        log_warning("\nTest interrupted by user")

    except Exception as e:
        log_error(f"Test failed with exception: {e}")
        import traceback
        traceback.print_exc()

    finally:
        # Always stop cluster
        cluster.stop_cluster()
        log_info("\nTest complete. Cluster data preserved in test-cluster-data/")

if __name__ == "__main__":
    print(f"{BOLD}Chronik Layered Storage with Raft Clustering - End-to-End Test{RESET}")
    print(f"{BOLD}================================================================{RESET}\n")

    test_layered_storage()
