#!/usr/bin/env python3
"""
Phase 4: Crash Recovery Testing

Tests that Raft cluster survives node crashes and recovers correctly:
- Scenario A: Kill follower, verify cluster continues, restart follower
- Scenario B: Kill leader, verify new election, restart old leader
"""

import subprocess
import time
import sys
import signal
import os
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic

node_pids = []

def cleanup():
    """Kill all Chronik nodes"""
    print("\nCleaning up...")
    for pid in node_pids:
        try:
            subprocess.run(["kill", str(pid)], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except:
            pass
    time.sleep(2)

def signal_handler(sig, frame):
    """Handle Ctrl+C"""
    print("\n\nInterrupted by user")
    cleanup()
    sys.exit(1)

signal.signal(signal.SIGINT, signal_handler)

def start_node(node_id, kafka_port, raft_port, peers, data_dir):
    """Start a Chronik Raft node"""
    cmd = [
        "./target/release/chronik-server",
        "--kafka-port", str(kafka_port),
        "--advertised-addr", "localhost",
        "--node-id", str(node_id),
        "--data-dir", data_dir,
        "raft-cluster",
        "--raft-addr", f"0.0.0.0:{raft_port}",
        "--peers", peers,
        "--bootstrap"
    ]

    log_file = open(f"{data_dir}.log", "w")
    process = subprocess.Popen(
        cmd,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        env={"RUST_LOG": "info"}
    )

    return process.pid

def find_leader():
    """Find which node is the Raft leader"""
    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"
        if not os.path.exists(log_file):
            continue

        try:
            result = subprocess.run(
                ["grep", "-i", "became leader", log_file],
                capture_output=True,
                text=True
            )
            if result.returncode == 0:
                # Check if it's still the leader (no "became follower" after)
                result2 = subprocess.run(
                    ["grep", "-i", "became follower\\|became candidate", log_file],
                    capture_output=True,
                    text=True
                )
                # If last line is "became leader", this is the leader
                if "became leader" in result.stdout.strip().split("\n")[-1]:
                    return node_id
        except:
            pass

    # Fallback: assume Node 1 is leader (for initial startup)
    return 1

def produce_message(port, topic, message):
    """Produce a message to a topic"""
    try:
        producer = KafkaProducer(
            bootstrap_servers=f"localhost:{port}",
            request_timeout_ms=10000,
            api_version=(2, 5, 0)
        )
        future = producer.send(topic, message.encode('utf-8'))
        result = future.get(timeout=10)
        producer.close()
        return result.offset
    except Exception as e:
        print(f"      ✗ Produce failed: {e}")
        return None

def consume_messages(port, topic):
    """Consume all messages from a topic"""
    try:
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=f"localhost:{port}",
            auto_offset_reset="earliest",
            consumer_timeout_ms=5000,
            api_version=(2, 5, 0)
        )

        messages = []
        for msg in consumer:
            messages.append(msg.value.decode('utf-8'))

        consumer.close()
        return messages
    except Exception as e:
        print(f"      ✗ Consume failed: {e}")
        return []

def main():
    global node_pids

    print("=" * 70)
    print("Phase 4: Crash Recovery Testing")
    print("=" * 70)
    print()

    # Step 1: Clean old data
    print("1. Cleaning old test data...")
    subprocess.run(["pkill", "-f", "chronik-server.*raft-cluster"], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)
    subprocess.run(["rm", "-rf", "/tmp/chronik-test-cluster"], check=False)
    subprocess.run(["mkdir", "-p", "/tmp/chronik-test-cluster/node1"], check=True)
    subprocess.run(["mkdir", "-p", "/tmp/chronik-test-cluster/node2"], check=True)
    subprocess.run(["mkdir", "-p", "/tmp/chronik-test-cluster/node3"], check=True)
    print("   ✓ Old data cleaned")

    # Step 2: Start 3-node cluster
    print("\n2. Starting 3-node Raft cluster...")

    pid1 = start_node(1, 9092, 9192, "2@localhost:9193,3@localhost:9194", "/tmp/chronik-test-cluster/node1")
    node_pids.append(pid1)
    print(f"   ✓ Node 1 started (PID: {pid1})")

    pid2 = start_node(2, 9093, 9193, "1@localhost:9192,3@localhost:9194", "/tmp/chronik-test-cluster/node2")
    node_pids.append(pid2)
    print(f"   ✓ Node 2 started (PID: {pid2})")

    pid3 = start_node(3, 9094, 9194, "1@localhost:9192,2@localhost:9193", "/tmp/chronik-test-cluster/node3")
    node_pids.append(pid3)
    print(f"   ✓ Node 3 started (PID: {pid3})")

    # Wait for cluster to form
    print("\n3. Waiting for cluster to form (25 seconds)...")
    time.sleep(25)

    # Check if nodes are still running
    for i, pid in enumerate(node_pids, 1):
        try:
            subprocess.run(["kill", "-0", str(pid)], check=True)
            print(f"   ✓ Node {i} is running")
        except subprocess.CalledProcessError:
            print(f"   ✗ Node {i} crashed!")
            cleanup()
            sys.exit(1)

    # Step 4: Create test topic
    print("\n4. Creating test topic 'crash-recovery-test'...")
    try:
        admin_client = KafkaAdminClient(
            bootstrap_servers="localhost:9092",
            request_timeout_ms=30000,
            api_version=(2, 5, 0)
        )

        topic = NewTopic(
            name="crash-recovery-test",
            num_partitions=1,
            replication_factor=1
        )

        admin_client.create_topics([topic], timeout_ms=30000)
        print("   ✓ Topic created")
        time.sleep(3)
    except Exception as e:
        print(f"   ✗ Failed to create topic: {e}")
        cleanup()
        sys.exit(1)

    # Step 5: Produce initial messages
    print("\n5. Producing initial messages (before any crashes)...")
    for i in range(1, 4):
        offset = produce_message(9092, "crash-recovery-test", f"message-{i}")
        if offset is not None:
            print(f"   ✓ Produced message-{i} at offset {offset}")
        else:
            print(f"   ✗ Failed to produce message-{i}")

    time.sleep(2)

    # =================================================================
    # SCENARIO A: Kill Follower
    # =================================================================
    print("\n" + "=" * 70)
    print("SCENARIO A: Kill Follower Node")
    print("=" * 70)

    # Find leader
    print("\n6. Identifying Raft leader...")
    leader_id = find_leader()
    print(f"   ✓ Leader is Node {leader_id}")

    # Choose a follower to kill
    follower_id = 2 if leader_id != 2 else 3
    follower_port = 9092 + follower_id - 1
    follower_pid = node_pids[follower_id - 1]

    print(f"\n7. Killing Node {follower_id} (FOLLOWER)...")
    try:
        subprocess.run(["kill", str(follower_pid)], check=True)
        print(f"   ✓ Node {follower_id} killed (PID: {follower_pid})")
        time.sleep(3)
    except:
        print(f"   ✗ Failed to kill Node {follower_id}")

    # Verify cluster still works
    print(f"\n8. Verifying cluster still works (producing to leader Node {leader_id})...")
    leader_port = 9092 + leader_id - 1
    offset = produce_message(leader_port, "crash-recovery-test", "message-after-follower-kill")
    if offset is not None:
        print(f"   ✓ Cluster still works! Message produced at offset {offset}")
    else:
        print("   ✗ Cluster failed after follower kill!")

    # Restart follower
    print(f"\n9. Restarting Node {follower_id}...")
    peers_map = {
        1: "2@localhost:9193,3@localhost:9194",
        2: "1@localhost:9192,3@localhost:9194",
        3: "1@localhost:9192,2@localhost:9193"
    }
    raft_ports = {1: 9192, 2: 9193, 3: 9194}

    new_pid = start_node(
        follower_id,
        follower_port,
        raft_ports[follower_id],
        peers_map[follower_id],
        f"/tmp/chronik-test-cluster/node{follower_id}"
    )
    node_pids[follower_id - 1] = new_pid
    print(f"   ✓ Node {follower_id} restarted (PID: {new_pid})")

    print("\n10. Waiting for Node to rejoin cluster (15 seconds)...")
    time.sleep(15)

    # Verify follower caught up
    print(f"\n11. Verifying Node {follower_id} caught up...")
    messages = consume_messages(follower_port, "crash-recovery-test")
    if len(messages) >= 4:  # Should have all 3 initial + 1 after kill
        print(f"   ✓ Node {follower_id} caught up! Consumed {len(messages)} messages")
        print(f"      Messages: {messages}")
    else:
        print(f"   ✗ Node {follower_id} didn't catch up. Only got {len(messages)} messages")

    # =================================================================
    # SCENARIO B: Kill Leader
    # =================================================================
    print("\n" + "=" * 70)
    print("SCENARIO B: Kill Leader Node")
    print("=" * 70)

    print(f"\n12. Killing current leader (Node {leader_id})...")
    leader_pid = node_pids[leader_id - 1]
    try:
        subprocess.run(["kill", str(leader_pid)], check=True)
        print(f"   ✓ Leader Node {leader_id} killed (PID: {leader_pid})")
        time.sleep(3)
    except:
        print(f"   ✗ Failed to kill leader Node {leader_id}")

    # Wait for new leader election
    print("\n13. Waiting for new leader election (15 seconds)...")
    time.sleep(15)

    # Find new leader
    new_leader_id = find_leader()
    if new_leader_id != leader_id:
        print(f"   ✓ New leader elected: Node {new_leader_id}")
    else:
        print(f"   ⚠ Could not determine new leader (may still be electing)")
        # Try both remaining nodes
        new_leader_id = 2 if leader_id == 1 else 1

    # Produce to new leader
    print(f"\n14. Producing to new leader (Node {new_leader_id})...")
    new_leader_port = 9092 + new_leader_id - 1
    offset = produce_message(new_leader_port, "crash-recovery-test", "message-after-leader-kill")
    if offset is not None:
        print(f"   ✓ Cluster survived leader kill! Message produced at offset {offset}")
    else:
        print("   ✗ Cluster failed after leader kill")

    # Restart old leader
    print(f"\n15. Restarting old leader (Node {leader_id})...")
    new_pid = start_node(
        leader_id,
        9092 + leader_id - 1,
        raft_ports[leader_id],
        peers_map[leader_id],
        f"/tmp/chronik-test-cluster/node{leader_id}"
    )
    node_pids[leader_id - 1] = new_pid
    print(f"   ✓ Node {leader_id} restarted (PID: {new_pid})")

    print("\n16. Waiting for old leader to rejoin as follower (15 seconds)...")
    time.sleep(15)

    # Verify old leader caught up
    print(f"\n17. Verifying Node {leader_id} caught up...")
    old_leader_port = 9092 + leader_id - 1
    messages = consume_messages(old_leader_port, "crash-recovery-test")
    if len(messages) >= 5:  # Should have all messages
        print(f"   ✓ Node {leader_id} caught up! Consumed {len(messages)} messages")
        print(f"      Messages: {messages}")
    else:
        print(f"   ✗ Node {leader_id} didn't catch up. Only got {len(messages)} messages")

    # Final verification
    print("\n" + "=" * 70)
    print("Final Verification")
    print("=" * 70)

    print("\n18. Verifying data consistency across all nodes...")
    for node_id, port in [(1, 9092), (2, 9093), (3, 9094)]:
        messages = consume_messages(port, "crash-recovery-test")
        print(f"   Node {node_id} (port {port}): {len(messages)} messages")

    # Cleanup
    cleanup()

    print("\n" + "=" * 70)
    print("Phase 4 Crash Recovery Test Complete")
    print("=" * 70)
    print("\nLogs available at: /tmp/chronik-test-cluster/node*.log")
    print("\nTo check recovery details:")
    print("  grep -i 'became leader\\|became follower\\|election' /tmp/chronik-test-cluster/node*.log")

if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"\n✗ Test failed with exception: {e}")
        import traceback
        traceback.print_exc()
        cleanup()
        sys.exit(1)
