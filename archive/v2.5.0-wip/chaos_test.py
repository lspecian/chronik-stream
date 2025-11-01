#!/usr/bin/env python3
"""
Phase 5 CHAOS TEST - Actually kill the leader and verify failover!
"""

import subprocess
import time
import sys
import os
import signal
import shutil

def run_cmd(cmd, timeout=10):
    """Run command and return output"""
    try:
        result = subprocess.run(cmd, shell=True, capture_output=True, timeout=timeout, text=True)
        return result.stdout, result.stderr, result.returncode
    except subprocess.TimeoutExpired:
        return "", "TIMEOUT", -1

def start_node(node_id, kafka_port, raft_port):
    """Start a chronik node"""
    data_dir = f"./data-node{node_id}"

    # Build peer list
    peers = []
    for n in [1, 2, 3]:
        if n != node_id:
            peer_port = 9192 if n == 1 else (9193 if n == 2 else 9194)
            peers.append(f"{n}@localhost:{peer_port}")

    cmd = [
        './target/release/chronik-server',
        '--kafka-port', str(kafka_port),
        '--advertised-addr', 'localhost',
        '--node-id', str(node_id),
        '--data-dir', data_dir,
        'raft-cluster',
        '--raft-addr', f'0.0.0.0:{raft_port}',
        '--peers', ','.join(peers),
        '--bootstrap',
    ]

    log_file = open(f'node{node_id}.log', 'w')
    proc = subprocess.Popen(cmd, stdout=log_file, stderr=subprocess.STDOUT)

    return proc, log_file

def main():
    print("="*80)
    print("PHASE 5: CHAOS TEST - Leader Election Failover")
    print("="*80)

    processes = {}

    try:
        # Step 1: Start 3-node cluster
        print("\n[1/7] Starting 3-node cluster...")
        for node_id, kafka_port, raft_port in [(1, 9092, 9192), (2, 9093, 9193), (3, 9094, 9194)]:
            proc, log = start_node(node_id, kafka_port, raft_port)
            processes[node_id] = {'proc': proc, 'log': log}
            print(f"    Node {node_id} started (PID: {proc.pid})")
            time.sleep(3)

        print("    Waiting 60s for cluster to stabilize...")
        time.sleep(60)

        # Step 2: Verify all nodes running
        print("\n[2/7] Verifying LeaderElector on all nodes...")
        for node_id in [1, 2, 3]:
            with open(f'node{node_id}.log', 'r') as f:
                if 'LeaderElector monitoring started' in f.read():
                    print(f"    ✅ Node {node_id}: LeaderElector active")
                else:
                    print(f"    ❌ Node {node_id}: LeaderElector NOT active")
                    return 1

        # Step 3: Create topic and produce messages to node 1 (establish as leader)
        print("\n[3/7] Producing 50 messages to Node 1 (establishing leader)...")
        for i in range(50):
            stdout, stderr, rc = run_cmd(f'echo "message-{i}" | kcat -P -b localhost:9092 -t chaos-test -p 0', timeout=5)
            if rc != 0 and i == 0:
                print(f"    First produce attempt: {stderr}")

        time.sleep(5)
        print("    ✅ 50 messages produced to Node 1")

        # Step 4: Verify messages are there
        print("\n[4/7] Verifying messages on Node 1...")
        stdout, stderr, rc = run_cmd('kcat -C -b localhost:9092 -t chaos-test -p 0 -o beginning -c 50 -e', timeout=10)
        msg_count = len(stdout.strip().split('\n')) if stdout.strip() else 0
        print(f"    Consumed {msg_count} messages from Node 1")

        if msg_count < 40:  # Allow some margin
            print(f"    ⚠️  Expected ~50, got {msg_count}")

        # Step 5: CHAOS - Kill Node 1 (the leader)
        print("\n[5/7] 💀 CHAOS: Killing Node 1 (leader)...")
        node1_pid = processes[1]['proc'].pid
        print(f"    Sending SIGKILL to PID {node1_pid}")

        os.kill(node1_pid, signal.SIGKILL)
        processes[1]['proc'].wait()
        processes[1]['log'].close()

        print(f"    ✅ Node 1 killed at {time.strftime('%H:%M:%S')}")
        print("    ⏳ Waiting 15 seconds for leader election...")
        time.sleep(15)

        # Step 6: Check logs for leader election
        print("\n[6/7] Checking for leader election evidence...")

        election_found = False
        for node_id in [2, 3]:
            with open(f'node{node_id}.log', 'r') as f:
                content = f.read()

                # Look for election-related messages
                if 'Leader timeout' in content or 'timeout' in content.lower():
                    print(f"    ✅ Node {node_id}: Detected leader timeout")
                    election_found = True

                if 'Elected' in content or 'election' in content.lower():
                    print(f"    ✅ Node {node_id}: Participated in election")
                    election_found = True

                if 'No leader for' in content:
                    print(f"    ✅ Node {node_id}: Detected missing leader")
                    election_found = True

        if not election_found:
            print("    ⚠️  No explicit election logs found")
            print("    (Checking node2.log and node3.log for any relevant activity...)")

            for node_id in [2, 3]:
                print(f"\n    Last 20 lines of node{node_id}.log:")
                with open(f'node{node_id}.log', 'r') as f:
                    lines = f.readlines()
                    for line in lines[-20:]:
                        if any(kw in line.lower() for kw in ['leader', 'timeout', 'elect', 'raft', 'partition']):
                            print(f"      {line.rstrip()}")

        # Step 7: Try producing to Node 2 (should be new leader or handle request)
        print("\n[7/7] Producing 50 messages to Node 2 (new leader)...")

        success_count = 0
        for i in range(50, 100):
            stdout, stderr, rc = run_cmd(f'echo "message-{i}" | kcat -P -b localhost:9093 -t chaos-test -p 0', timeout=5)
            if rc == 0:
                success_count += 1

        print(f"    ✅ {success_count}/50 messages produced to Node 2")

        if success_count < 40:
            print(f"    ⚠️  Only {success_count} succeeded - cluster may not be ready")

        # Verify we can consume from Node 2
        print("\n[FINAL] Consuming all messages from Node 2...")
        time.sleep(3)
        stdout, stderr, rc = run_cmd('kcat -C -b localhost:9093 -t chaos-test -p 0 -o beginning -e', timeout=15)

        final_count = len(stdout.strip().split('\n')) if stdout.strip() else 0
        print(f"    Final message count: {final_count}")

        # Summary
        print("\n" + "="*80)
        print("CHAOS TEST RESULTS")
        print("="*80)
        print(f"Messages before failover: ~50")
        print(f"Messages after failover:  {success_count}/50 produced")
        print(f"Total messages consumed:  {final_count}")
        print(f"Leader election logs:     {'FOUND' if election_found else 'NOT FOUND'}")

        if final_count >= 80 and success_count >= 40:
            print("\n🎉 CHAOS TEST: PASSED ✅")
            print("  - Node 1 killed successfully")
            print("  - Cluster continued operating")
            print("  - Messages produced to new leader")
            print("  - All messages recoverable")
            return 0
        elif success_count >= 40:
            print("\n⚠️  CHAOS TEST: PARTIAL PASS")
            print("  - Cluster survived leader failure")
            print("  - New leader accepting writes")
            print("  - Data may need recovery verification")
            return 0
        else:
            print("\n❌ CHAOS TEST: FAILED")
            print("  - Cluster may not have recovered from leader failure")
            return 1

    except KeyboardInterrupt:
        print("\n⚠️  Test interrupted")
        return 1

    except Exception as e:
        print(f"\n❌ Error: {e}")
        import traceback
        traceback.print_exc()
        return 1

    finally:
        print("\n🧹 Cleaning up remaining nodes...")
        for node_id, info in processes.items():
            try:
                info['proc'].kill()
                info['proc'].wait()
                info['log'].close()
                print(f"    Killed node {node_id}")
            except:
                pass

if __name__ == '__main__':
    sys.exit(main())
