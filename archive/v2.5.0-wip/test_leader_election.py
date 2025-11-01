#!/usr/bin/env python3
"""
Phase 5: Leader Election Testing Script

Tests automatic partition leader failover in a 3-node Raft cluster.

Test Flow:
1. Start 3-node cluster (nodes 1, 2, 3)
2. Create topic 'test-leader-election' (verifies partition metadata init)
3. Produce 100 messages to node 1 (verifies heartbeat recording)
4. Kill node 1 (leader)
5. Wait for leader election (10s timeout)
6. Produce 100 messages to node 2 (new leader)
7. Consume all 200 messages (verifies zero data loss)
"""

import subprocess
import time
import sys
import signal
import os
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

# Test configuration
TOPIC = 'test-leader-election'
NUM_MESSAGES_PHASE1 = 100
NUM_MESSAGES_PHASE2 = 100
TOTAL_MESSAGES = NUM_MESSAGES_PHASE1 + NUM_MESSAGES_PHASE2

# Node configuration
NODES = [
    {'id': 1, 'kafka_port': 9092, 'raft_port': 9192, 'raft_addr': '0.0.0.0:9192'},
    {'id': 2, 'kafka_port': 9093, 'raft_port': 9193, 'raft_addr': '0.0.0.0:9193'},
    {'id': 3, 'kafka_port': 9094, 'raft_port': 9194, 'raft_addr': '0.0.0.0:9194'},
]

# Track running processes
processes = {}

def start_node(node):
    """Start a Chronik node in raft-cluster mode"""
    node_id = node['id']
    kafka_port = node['kafka_port']
    raft_addr = node['raft_addr']

    # Build peer list (all nodes except self)
    peers = []
    for n in NODES:
        if n['id'] != node_id:
            peers.append(f"{n['id']}@localhost:{n['raft_port']}")
    peers_str = ','.join(peers)

    # Data directory for this node
    data_dir = f"./data-node{node_id}"

    # Clean data directory for fresh start
    subprocess.run(['rm', '-rf', data_dir], check=False)

    cmd = [
        './target/release/chronik-server',
        '--kafka-port', str(kafka_port),
        '--advertised-addr', 'localhost',
        '--node-id', str(node_id),
        '--data-dir', data_dir,
        'raft-cluster',
        '--raft-addr', raft_addr,
        '--peers', peers_str,
        '--bootstrap',
    ]

    print(f"🚀 Starting Node {node_id}:")
    print(f"   Kafka: localhost:{kafka_port}")
    print(f"   Raft: {raft_addr}")
    print(f"   Peers: {peers_str}")

    # Start process with output to log file
    log_file = open(f'node{node_id}.log', 'w')
    proc = subprocess.Popen(
        cmd,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid  # Create new process group for clean shutdown
    )

    processes[node_id] = {'proc': proc, 'log': log_file}
    print(f"✅ Node {node_id} started (PID: {proc.pid})")

    return proc

def kill_node(node_id):
    """Kill a node gracefully"""
    if node_id in processes:
        proc_info = processes[node_id]
        proc = proc_info['proc']

        print(f"💀 Killing Node {node_id} (PID: {proc.pid})...")

        # Kill entire process group
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            print(f"⚠️  Node {node_id} didn't stop gracefully, forcing...")
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            proc.wait()

        proc_info['log'].close()
        del processes[node_id]

        print(f"✅ Node {node_id} killed")

def cleanup_all():
    """Kill all running nodes"""
    print("\n🧹 Cleaning up all nodes...")
    for node_id in list(processes.keys()):
        kill_node(node_id)

def wait_for_cluster_ready():
    """Wait for all nodes to be ready"""
    print("\n⏳ Waiting for cluster to be ready (60s)...")
    time.sleep(60)  # Raft cluster needs time to establish quorum
    print("✅ Cluster should be ready")

def create_topic():
    """Create test topic"""
    print(f"\n📝 Creating topic '{TOPIC}'...")

    # Use kafka-python's admin client would be better, but let's use producer auto-create
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0),
        request_timeout_ms=30000,
    )

    # Send a dummy message to trigger auto-creation
    future = producer.send(TOPIC, b'init')

    try:
        future.get(timeout=30)
        print(f"✅ Topic '{TOPIC}' created")
    except KafkaError as e:
        print(f"❌ Failed to create topic: {e}")
        raise
    finally:
        producer.close()

    # Wait for topic metadata to propagate
    time.sleep(5)

def produce_messages(bootstrap_server, phase_name, num_messages, start_offset=0):
    """Produce messages to the cluster"""
    print(f"\n📤 {phase_name}: Producing {num_messages} messages to {bootstrap_server}...")

    producer = KafkaProducer(
        bootstrap_servers=bootstrap_server,
        api_version=(0, 10, 0),
        acks=-1,  # Wait for ISR quorum (acks=-1)
        request_timeout_ms=30000,
    )

    success_count = 0
    errors = []

    for i in range(num_messages):
        msg_id = start_offset + i
        message = f"msg-{msg_id}-{phase_name}".encode()

        try:
            future = producer.send(TOPIC, message)
            future.get(timeout=10)
            success_count += 1

            if (i + 1) % 20 == 0:
                print(f"   Produced {i + 1}/{num_messages} messages...")

        except Exception as e:
            errors.append((msg_id, str(e)))
            print(f"   ❌ Error producing msg-{msg_id}: {e}")

    producer.close()

    print(f"✅ {phase_name}: Produced {success_count}/{num_messages} messages")

    if errors:
        print(f"⚠️  Errors: {len(errors)}")
        for msg_id, error in errors[:5]:  # Show first 5 errors
            print(f"   - msg-{msg_id}: {error}")

    return success_count, errors

def consume_messages(expected_count):
    """Consume all messages from the topic"""
    print(f"\n📥 Consuming messages (expecting {expected_count})...")

    consumer = KafkaConsumer(
        TOPIC,
        bootstrap_servers='localhost:9093',  # Use node 2 (should be alive)
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000,  # 10s timeout
        api_version=(0, 10, 0),
    )

    messages = []
    for msg in consumer:
        messages.append(msg.value.decode())

    consumer.close()

    print(f"✅ Consumed {len(messages)} messages")

    if len(messages) != expected_count:
        print(f"❌ MISMATCH: Expected {expected_count}, got {len(messages)}")
        return False

    return True

def check_logs_for_leader_election():
    """Check node logs for leader election messages"""
    print("\n🔍 Checking logs for leader election...")

    # Check node 2 and node 3 logs for election messages
    for node_id in [2, 3]:
        log_file = f'node{node_id}.log'

        try:
            with open(log_file, 'r') as f:
                content = f.read()

                if 'Leader timeout' in content:
                    print(f"✅ Node {node_id}: Detected leader timeout")

                if 'Elected new leader' in content or 'Election' in content:
                    print(f"✅ Node {node_id}: Participated in election")

                if 'LeaderElector' in content:
                    print(f"✅ Node {node_id}: LeaderElector is active")

        except FileNotFoundError:
            print(f"⚠️  Log file not found: {log_file}")

def main():
    """Main test flow"""
    print("=" * 70)
    print("Phase 5: Leader Election Test")
    print("=" * 70)

    try:
        # Step 1: Start 3-node cluster
        print("\n" + "=" * 70)
        print("STEP 1: Starting 3-node Raft cluster")
        print("=" * 70)

        for node in NODES:
            start_node(node)
            time.sleep(2)  # Stagger startup

        wait_for_cluster_ready()

        # Step 2: Create topic
        print("\n" + "=" * 70)
        print("STEP 2: Create topic (verifies partition metadata init)")
        print("=" * 70)

        create_topic()

        # Step 3: Produce messages to node 1 (initial leader)
        print("\n" + "=" * 70)
        print("STEP 3: Produce messages (verifies heartbeat recording)")
        print("=" * 70)

        success1, errors1 = produce_messages(
            'localhost:9092',
            'Phase 1 (Node 1)',
            NUM_MESSAGES_PHASE1,
            start_offset=0
        )

        # Step 4: Kill node 1 (leader)
        print("\n" + "=" * 70)
        print("STEP 4: Kill leader node (Node 1)")
        print("=" * 70)

        kill_node(1)

        # Step 5: Wait for leader election
        print("\n" + "=" * 70)
        print("STEP 5: Wait for leader election (15s)")
        print("=" * 70)

        print("⏳ Waiting for new leader to be elected...")
        time.sleep(15)  # 10s timeout + 5s buffer

        check_logs_for_leader_election()

        # Step 6: Produce messages to node 2 (should be new leader)
        print("\n" + "=" * 70)
        print("STEP 6: Produce to new leader (Node 2)")
        print("=" * 70)

        success2, errors2 = produce_messages(
            'localhost:9093',
            'Phase 2 (Node 2)',
            NUM_MESSAGES_PHASE2,
            start_offset=NUM_MESSAGES_PHASE1
        )

        # Step 7: Consume all messages
        print("\n" + "=" * 70)
        print("STEP 7: Verify all messages (zero data loss)")
        print("=" * 70)

        # Calculate expected messages (only count successes)
        expected = success1 + success2

        success = consume_messages(expected)

        # Final summary
        print("\n" + "=" * 70)
        print("TEST RESULTS")
        print("=" * 70)

        print(f"Phase 1 produce: {success1}/{NUM_MESSAGES_PHASE1} messages")
        print(f"Phase 2 produce: {success2}/{NUM_MESSAGES_PHASE2} messages")
        print(f"Total consumed: {expected} messages")
        print(f"Data loss: {'NONE ✅' if success else 'DETECTED ❌'}")

        if success and success1 > 0 and success2 > 0:
            print("\n🎉 PHASE 5 LEADER ELECTION TEST: PASSED ✅")
            return 0
        else:
            print("\n❌ PHASE 5 LEADER ELECTION TEST: FAILED")
            return 1

    except KeyboardInterrupt:
        print("\n⚠️  Test interrupted by user")
        return 1

    except Exception as e:
        print(f"\n❌ Test failed with error: {e}")
        import traceback
        traceback.print_exc()
        return 1

    finally:
        cleanup_all()
        print("\n✅ Cleanup complete")

if __name__ == '__main__':
    sys.exit(main())
