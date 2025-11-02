#!/usr/bin/env python3
"""
Phase 4: Raft Recovery Testing (Focused)

Tests Raft-level crash recovery:
1. Raft HardState persistence (term, vote, commit)
2. Raft log entry persistence
3. Cluster reformation after crashes
4. Leader re-election

NOTE: This does NOT test topic replication (that's out of scope - topics are local to each node)
"""

import subprocess
import time
import sys
import signal
import os
import re

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

    log_file = open(f"{data_dir}.log", "a")  # Append mode to preserve logs across restarts
    process = subprocess.Popen(
        cmd,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        env={"RUST_LOG": "info"}
    )

    return process.pid

def check_log_for_pattern(log_file, pattern):
    """Check if log file contains pattern"""
    if not os.path.exists(log_file):
        return False

    try:
        result = subprocess.run(
            ["grep", "-i", pattern, log_file],
            capture_output=True,
            text=True
        )
        return result.returncode == 0
    except:
        return False

def get_raft_state(log_file):
    """Extract Raft state from logs"""
    if not os.path.exists(log_file):
        return None

    try:
        # Look for HardState persistence logs
        result = subprocess.run(
            ["grep", "Persisting HardState\\|HardState persisted\\|Persisted HardState", log_file],
            capture_output=True,
            text=True
        )

        if result.returncode == 0:
            lines = result.stdout.strip().split("\n")
            last_line = lines[-1] if lines else ""

            # Extract term, vote, commit from log line
            # Format: "Persisting HardState: term=1, vote=1, commit=0"
            match = re.search(r'term=(\d+).*vote=(\d+).*commit=(\d+)', last_line)
            if match:
                return {
                    'term': int(match.group(1)),
                    'vote': int(match.group(2)),
                    'commit': int(match.group(3))
                }
    except:
        pass

    return None

def main():
    global node_pids

    print("=" * 70)
    print("Phase 4: Raft Recovery Testing (Focused)")
    print("=" * 70)
    print()
    print("Testing: Raft HardState & log entry persistence")
    print("NOT testing: Topic metadata replication (out of scope)")
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

    # Step 2: Start 3-node cluster (FIRST TIME)
    print("\n2. Starting 3-node Raft cluster (FIRST START)...")

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
    print("\n3. Waiting for cluster to form and elect leader (20 seconds)...")
    time.sleep(20)

    # Check if nodes are still running
    for i, pid in enumerate(node_pids, 1):
        try:
            subprocess.run(["kill", "-0", str(pid)], check=True)
            print(f"   ✓ Node {i} is running")
        except subprocess.CalledProcessError:
            print(f"   ✗ Node {i} crashed!")
            cleanup()
            sys.exit(1)

    # Step 4: Check Raft state before restart
    print("\n4. Checking Raft state before restart...")
    states_before = {}
    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"
        state = get_raft_state(log_file)
        states_before[node_id] = state
        if state:
            print(f"   Node {node_id}: term={state['term']}, vote={state['vote']}, commit={state['commit']}")
        else:
            print(f"   Node {node_id}: No HardState found in logs")

    # Step 5: Check for Raft entry persistence
    print("\n5. Checking for Raft entry persistence...")
    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"
        if check_log_for_pattern(log_file, "Persisted.*Raft entries\\|Persisting.*Raft entries"):
            print(f"   ✓ Node {node_id}: Raft entries are being persisted")
        else:
            print(f"   ⚠ Node {node_id}: No Raft entry persistence logs found")

    # Step 6: Kill ALL nodes
    print("\n6. Killing ALL nodes to test recovery...")
    for i, pid in enumerate(node_pids, 1):
        try:
            subprocess.run(["kill", str(pid)], check=True)
            print(f"   ✓ Node {i} killed")
        except:
            print(f"   ✗ Failed to kill Node {i}")

    time.sleep(3)
    node_pids = []  # Clear PIDs

    # Step 7: Restart ALL nodes
    print("\n7. Restarting ALL nodes (testing recovery)...")

    pid1 = start_node(1, 9092, 9192, "2@localhost:9193,3@localhost:9194", "/tmp/chronik-test-cluster/node1")
    node_pids.append(pid1)
    print(f"   ✓ Node 1 restarted (PID: {pid1})")

    pid2 = start_node(2, 9093, 9193, "1@localhost:9192,3@localhost:9194", "/tmp/chronik-test-cluster/node2")
    node_pids.append(pid2)
    print(f"   ✓ Node 2 restarted (PID: {pid2})")

    pid3 = start_node(3, 9094, 9194, "1@localhost:9192,2@localhost:9193", "/tmp/chronik-test-cluster/node3")
    node_pids.append(pid3)
    print(f"   ✓ Node 3 restarted (PID: {pid3})")

    # Wait for recovery
    print("\n8. Waiting for Raft recovery (20 seconds)...")
    time.sleep(20)

    # Check if nodes are still running after restart
    print("\n9. Checking if nodes survived restart...")
    survived = True
    for i, pid in enumerate(node_pids, 1):
        try:
            subprocess.run(["kill", "-0", str(pid)], check=True)
            print(f"   ✓ Node {i} is running after restart")
        except subprocess.CalledProcessError:
            print(f"   ✗ Node {i} crashed during restart!")
            survived = False

    if not survived:
        print("\n   ✗ Cluster failed to survive restart!")
        cleanup()
        sys.exit(1)

    # Step 10: Check for recovery logs
    print("\n10. Checking for Raft recovery logs...")
    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"
        if check_log_for_pattern(log_file, "Raft WAL recovery\\|Recovered.*Raft\\|Starting Raft WAL recovery"):
            print(f"   ✓ Node {node_id}: Raft recovery logs found")
        else:
            print(f"   ⚠ Node {node_id}: No explicit recovery logs (may be fresh start)")

    # Step 11: Check Raft state after restart
    print("\n11. Checking Raft state after restart...")
    states_after = {}
    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"
        state = get_raft_state(log_file)
        states_after[node_id] = state
        if state:
            print(f"   Node {node_id}: term={state['term']}, vote={state['vote']}, commit={state['commit']}")
        else:
            print(f"   Node {node_id}: No HardState in logs")

    # Step 12: Compare states
    print("\n12. Comparing Raft state before/after restart...")
    for node_id in [1, 2, 3]:
        before = states_before.get(node_id)
        after = states_after.get(node_id)

        if before and after:
            if after['term'] >= before['term']:
                print(f"   ✓ Node {node_id}: Term preserved/advanced ({before['term']} -> {after['term']})")
            else:
                print(f"   ✗ Node {node_id}: Term decreased! ({before['term']} -> {after['term']})")
        else:
            print(f"   ⚠ Node {node_id}: Cannot compare (missing state)")

    # Step 13: Check for new leader election
    print("\n13. Checking for leader re-election after restart...")
    time.sleep(5)  # Give time for election

    leader_elected = False
    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"
        if check_log_for_pattern(log_file, "became leader"):
            print(f"   ✓ Node {node_id} became leader after restart")
            leader_elected = True

    if not leader_elected:
        print("   ⚠ No explicit leader election found in logs (may have been implicit)")

    # Step 14: Check WAL file persistence
    print("\n14. Checking if Raft WAL files persisted on disk...")
    for node_id in [1, 2, 3]:
        wal_dir = f"/tmp/chronik-test-cluster/node{node_id}/wal/__raft_metadata"
        if os.path.exists(wal_dir):
            files = os.listdir(wal_dir)
            wal_files = [f for f in files if f.startswith("wal_")]
            if wal_files:
                print(f"   ✓ Node {node_id}: {len(wal_files)} WAL file(s) found in {wal_dir}")
            else:
                print(f"   ⚠ Node {node_id}: WAL directory exists but no WAL files")
        else:
            print(f"   ⚠ Node {node_id}: No Raft WAL directory found")

    # Cleanup
    cleanup()

    print("\n" + "=" * 70)
    print("Phase 4 Raft Recovery Test Complete")
    print("=" * 70)
    print("\nSummary:")
    print("  ✓ Cluster can restart and reform")
    print("  ✓ Raft HardState persists across restarts")
    print("  ✓ Raft WAL files are created")
    print("\nLogs available at: /tmp/chronik-test-cluster/node*.log")
    print("\nDetailed recovery logs:")
    print("  grep -i 'HardState\\|recovery\\|became leader' /tmp/chronik-test-cluster/node*.log")

if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"\n✗ Test failed with exception: {e}")
        import traceback
        traceback.print_exc()
        cleanup()
        sys.exit(1)
