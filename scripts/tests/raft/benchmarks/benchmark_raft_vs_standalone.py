#!/usr/bin/env python3
"""
Performance Benchmark: Raft Cluster vs Standalone

Measures:
1. Throughput (messages/second)
2. Latency (p50, p95, p99)
3. CPU usage
4. Memory footprint

Tests both:
- Standalone mode (single node, WAL-only)
- Raft cluster (3 nodes, quorum writes)
"""

import subprocess
import time
import sys
import os
import signal
from kafka import KafkaProducer, KafkaConsumer
import statistics
import psutil

# Test configuration
TOPIC_NAME = "perf-test"
NUM_MESSAGES = 10000
MESSAGE_SIZE = 1024  # 1KB
BATCH_SIZE = 100

# Node configurations
STANDALONE_PORT = 9095
RAFT_NODES = [
    {"id": 1, "kafka_port": 9092, "raft_port": 9192, "data_dir": "./data/node1"},
    {"id": 2, "kafka_port": 9093, "raft_port": 9193, "data_dir": "./data/node2"},
    {"id": 3, "kafka_port": 9094, "raft_port": 9194, "data_dir": "./data/node3"},
]

CLUSTER_PEERS = ",".join([f"localhost:{RAFT_NODES[i]['kafka_port']}:{RAFT_NODES[i]['raft_port']}" for i in range(3)])

class Colors:
    GREEN = '\033[92m'
    RED = '\033[91m'
    YELLOW = '\033[93m'
    BLUE = '\033[94m'
    CYAN = '\033[96m'
    ENDC = '\033[0m'
    BOLD = '\033[1m'

def log_info(msg):
    print(f"{Colors.BLUE}[INFO]{Colors.ENDC} {msg}")

def log_success(msg):
    print(f"{Colors.GREEN}[SUCCESS]{Colors.ENDC} {msg}")

def log_metric(metric, value):
    print(f"{Colors.CYAN}  {metric}: {Colors.BOLD}{value}{Colors.ENDC}")

def cleanup_all():
    """Clean all data directories"""
    log_info("Cleaning up all data directories...")
    for node in RAFT_NODES:
        subprocess.run(["rm", "-rf", node["data_dir"]], stderr=subprocess.DEVNULL)
    subprocess.run(["rm", "-rf", "./data/standalone"], stderr=subprocess.DEVNULL)
    subprocess.run(["rm", "-rf", "./data/wal/__meta"], stderr=subprocess.DEVNULL)

def start_standalone():
    """Start standalone Chronik server"""
    log_info(f"Starting standalone server on port {STANDALONE_PORT}...")

    env = os.environ.copy()
    env.update({
        "CHRONIK_KAFKA_PORT": str(STANDALONE_PORT),
        "CHRONIK_ADVERTISED_ADDR": "localhost",
        "CHRONIK_ADVERTISED_PORT": str(STANDALONE_PORT),
        "CHRONIK_DATA_DIR": "./data/standalone",
        "RUST_LOG": "info",
    })

    log_file = open("standalone_perf.log", "w")
    proc = subprocess.Popen(
        ["cargo", "run", "--release", "--bin", "chronik-server", "--", "standalone"],
        env=env,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid
    )

    return proc, log_file

def start_raft_node(node_id):
    """Start a Raft cluster node"""
    node = RAFT_NODES[node_id - 1]
    log_info(f"Starting Raft Node {node_id}...")

    env = os.environ.copy()
    env.update({
        "CHRONIK_CLUSTER_ENABLED": "true",
        "CHRONIK_NODE_ID": str(node_id),
        "CHRONIK_KAFKA_PORT": str(node["kafka_port"]),
        "CHRONIK_ADVERTISED_ADDR": "localhost",
        "CHRONIK_ADVERTISED_PORT": str(node["kafka_port"]),
        "CHRONIK_RAFT_PORT": str(node["raft_port"]),
        "CHRONIK_DATA_DIR": node["data_dir"],
        "CHRONIK_REPLICATION_FACTOR": "3",
        "CHRONIK_MIN_INSYNC_REPLICAS": "2",
        "CHRONIK_CLUSTER_PEERS": CLUSTER_PEERS,
        "RUST_LOG": "info",
    })

    log_file = open(f"node{node_id}_perf.log", "w")
    proc = subprocess.Popen(
        ["cargo", "run", "--release", "--bin", "chronik-server", "--features", "raft", "--", "standalone"],
        env=env,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid
    )

    return proc, log_file

def stop_process(proc, log_file):
    """Stop a process"""
    try:
        if proc.poll() is None:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                    proc.wait()
                except OSError:
                    pass
            except OSError:
                pass
    finally:
        try:
            log_file.close()
        except:
            pass

def wait_for_server(bootstrap_server, timeout=60):
    """Wait for server to be ready"""
    start = time.time()
    while time.time() - start < timeout:
        try:
            producer = KafkaProducer(
                bootstrap_servers=[bootstrap_server],
                request_timeout_ms=5000,
                api_version=(2, 5, 0)
            )
            producer.close()
            return True
        except Exception:
            time.sleep(2)
    return False

def get_process_stats(proc):
    """Get CPU and memory stats for a process"""
    try:
        process = psutil.Process(proc.pid)
        return {
            "cpu_percent": process.cpu_percent(interval=1),
            "memory_mb": process.memory_info().rss / 1024 / 1024
        }
    except Exception:
        return {"cpu_percent": 0, "memory_mb": 0}

def benchmark_produce(bootstrap_server, num_messages, message_size):
    """Benchmark produce performance"""
    log_info(f"Producing {num_messages} messages ({message_size} bytes each)...")

    producer = KafkaProducer(
        bootstrap_servers=[bootstrap_server],
        acks='all',
        compression_type='none',
        batch_size=16384,
        linger_ms=10,
        request_timeout_ms=30000,
        api_version=(2, 5, 0)
    )

    message = b'X' * message_size
    latencies = []

    start_time = time.time()

    for i in range(num_messages):
        msg_start = time.time()
        try:
            future = producer.send(TOPIC_NAME, value=message)
            future.get(timeout=30)
            latency = (time.time() - msg_start) * 1000  # Convert to ms
            latencies.append(latency)
        except Exception as e:
            log_info(f"Send failed at message {i}: {e}")

        if (i + 1) % 1000 == 0:
            log_info(f"  Produced {i + 1}/{num_messages}...")

    producer.flush()
    producer.close()

    elapsed = time.time() - start_time
    throughput = num_messages / elapsed

    return {
        "throughput": throughput,
        "elapsed": elapsed,
        "latencies": latencies
    }

def benchmark_consume(bootstrap_server, expected_messages):
    """Benchmark consume performance"""
    log_info(f"Consuming {expected_messages} messages...")

    consumer = KafkaConsumer(
        TOPIC_NAME,
        bootstrap_servers=[bootstrap_server],
        auto_offset_reset='earliest',
        consumer_timeout_ms=30000,
        api_version=(2, 5, 0)
    )

    count = 0
    start_time = time.time()

    try:
        for msg in consumer:
            count += 1
            if count >= expected_messages:
                break
            if count % 1000 == 0:
                log_info(f"  Consumed {count}/{expected_messages}...")
    finally:
        consumer.close()

    elapsed = time.time() - start_time
    throughput = count / elapsed

    return {
        "throughput": throughput,
        "elapsed": elapsed,
        "count": count
    }

def calculate_percentiles(latencies):
    """Calculate latency percentiles"""
    if not latencies:
        return {"p50": 0, "p95": 0, "p99": 0, "mean": 0}

    sorted_latencies = sorted(latencies)
    return {
        "p50": sorted_latencies[int(len(sorted_latencies) * 0.50)],
        "p95": sorted_latencies[int(len(sorted_latencies) * 0.95)],
        "p99": sorted_latencies[int(len(sorted_latencies) * 0.99)],
        "mean": statistics.mean(latencies)
    }

def run_standalone_benchmark():
    """Run benchmark on standalone server"""
    log_info("\n" + "="*60)
    log_info("BENCHMARKING STANDALONE MODE")
    log_info("="*60 + "\n")

    cleanup_all()

    proc, log_file = start_standalone()
    time.sleep(10)

    bootstrap = f"localhost:{STANDALONE_PORT}"

    if not wait_for_server(bootstrap, timeout=60):
        log_info("Standalone server failed to start")
        stop_process(proc, log_file)
        return None

    try:
        # Get baseline stats
        baseline_stats = get_process_stats(proc)

        # Produce benchmark
        produce_result = benchmark_produce(bootstrap, NUM_MESSAGES, MESSAGE_SIZE)

        # Get stats after produce
        produce_stats = get_process_stats(proc)

        # Consume benchmark
        consume_result = benchmark_consume(bootstrap, NUM_MESSAGES)

        # Calculate latency percentiles
        latency_stats = calculate_percentiles(produce_result["latencies"])

        results = {
            "produce_throughput": produce_result["throughput"],
            "produce_elapsed": produce_result["elapsed"],
            "consume_throughput": consume_result["throughput"],
            "consume_elapsed": consume_result["elapsed"],
            "latency": latency_stats,
            "cpu_percent": produce_stats["cpu_percent"],
            "memory_mb": produce_stats["memory_mb"]
        }

        log_success("\nStandalone Results:")
        log_metric("Produce Throughput", f"{results['produce_throughput']:.0f} msg/s")
        log_metric("Consume Throughput", f"{results['consume_throughput']:.0f} msg/s")
        log_metric("Latency (p50)", f"{latency_stats['p50']:.2f} ms")
        log_metric("Latency (p95)", f"{latency_stats['p95']:.2f} ms")
        log_metric("Latency (p99)", f"{latency_stats['p99']:.2f} ms")
        log_metric("CPU Usage", f"{results['cpu_percent']:.1f}%")
        log_metric("Memory", f"{results['memory_mb']:.0f} MB")

        return results

    finally:
        stop_process(proc, log_file)
        time.sleep(2)

def run_raft_benchmark():
    """Run benchmark on Raft cluster"""
    log_info("\n" + "="*60)
    log_info("BENCHMARKING RAFT CLUSTER MODE (3 nodes)")
    log_info("="*60 + "\n")

    cleanup_all()

    processes = []
    log_files = []

    # Start all nodes
    for node_id in [1, 2, 3]:
        proc, log_file = start_raft_node(node_id)
        processes.append(proc)
        log_files.append(log_file)
        time.sleep(2)

    time.sleep(15)  # Wait for cluster formation

    bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in RAFT_NODES]
    bootstrap = ",".join(bootstrap_servers)

    if not wait_for_server(bootstrap_servers[0], timeout=60):
        log_info("Raft cluster failed to start")
        for i, proc in enumerate(processes):
            stop_process(proc, log_files[i])
        return None

    try:
        # Get baseline stats for all nodes
        baseline_stats = [get_process_stats(proc) for proc in processes]

        # Produce benchmark
        produce_result = benchmark_produce(bootstrap, NUM_MESSAGES, MESSAGE_SIZE)

        # Get stats after produce
        produce_stats = [get_process_stats(proc) for proc in processes]

        # Consume benchmark
        consume_result = benchmark_consume(bootstrap, NUM_MESSAGES)

        # Calculate latency percentiles
        latency_stats = calculate_percentiles(produce_result["latencies"])

        # Average stats across nodes
        avg_cpu = sum(s["cpu_percent"] for s in produce_stats) / len(produce_stats)
        avg_memory = sum(s["memory_mb"] for s in produce_stats) / len(produce_stats)

        results = {
            "produce_throughput": produce_result["throughput"],
            "produce_elapsed": produce_result["elapsed"],
            "consume_throughput": consume_result["throughput"],
            "consume_elapsed": consume_result["elapsed"],
            "latency": latency_stats,
            "cpu_percent": avg_cpu,
            "memory_mb": avg_memory,
            "total_memory_mb": sum(s["memory_mb"] for s in produce_stats)
        }

        log_success("\nRaft Cluster Results:")
        log_metric("Produce Throughput", f"{results['produce_throughput']:.0f} msg/s")
        log_metric("Consume Throughput", f"{results['consume_throughput']:.0f} msg/s")
        log_metric("Latency (p50)", f"{latency_stats['p50']:.2f} ms")
        log_metric("Latency (p95)", f"{latency_stats['p95']:.2f} ms")
        log_metric("Latency (p99)", f"{latency_stats['p99']:.2f} ms")
        log_metric("CPU Usage (avg per node)", f"{avg_cpu:.1f}%")
        log_metric("Memory (avg per node)", f"{avg_memory:.0f} MB")
        log_metric("Total Memory (all nodes)", f"{results['total_memory_mb']:.0f} MB")

        return results

    finally:
        for i, proc in enumerate(processes):
            stop_process(proc, log_files[i])
        time.sleep(2)

def print_comparison(standalone, raft):
    """Print comparison between standalone and Raft"""
    log_info("\n" + "="*60)
    log_info("PERFORMANCE COMPARISON")
    log_info("="*60 + "\n")

    def compare_metric(name, standalone_val, raft_val, lower_is_better=False):
        diff_pct = ((raft_val - standalone_val) / standalone_val * 100) if standalone_val > 0 else 0

        if lower_is_better:
            diff_pct = -diff_pct

        color = Colors.GREEN if diff_pct > -10 else Colors.YELLOW if diff_pct > -30 else Colors.RED
        sign = "+" if diff_pct > 0 else ""

        print(f"{Colors.BOLD}{name}:{Colors.ENDC}")
        print(f"  Standalone: {standalone_val:.2f}")
        print(f"  Raft:       {raft_val:.2f}")
        print(f"  Difference: {color}{sign}{diff_pct:.1f}%{Colors.ENDC}\n")

    compare_metric("Produce Throughput (msg/s)",
                   standalone["produce_throughput"],
                   raft["produce_throughput"])

    compare_metric("Latency p99 (ms)",
                   standalone["latency"]["p99"],
                   raft["latency"]["p99"],
                   lower_is_better=True)

    compare_metric("Memory per node (MB)",
                   standalone["memory_mb"],
                   raft["memory_mb"],
                   lower_is_better=True)

    # Summary
    throughput_overhead = ((standalone["produce_throughput"] - raft["produce_throughput"]) /
                           standalone["produce_throughput"] * 100)
    latency_overhead = ((raft["latency"]["p99"] - standalone["latency"]["p99"]) /
                        standalone["latency"]["p99"] * 100)

    log_info("\nSUMMARY:")
    log_metric("Raft Throughput Overhead", f"{throughput_overhead:.1f}%")
    log_metric("Raft Latency Overhead", f"{latency_overhead:.1f}%")

    if throughput_overhead < 30 and latency_overhead < 100:
        log_success("\n✅ Raft performance is acceptable (< 30% throughput overhead, < 2x latency)")
    else:
        log_info("\n⚠️  Raft has significant performance overhead (expected for quorum writes)")

def main():
    """Main benchmark flow"""
    try:
        log_info("Starting Performance Benchmark: Raft vs Standalone\n")

        # Run standalone benchmark
        standalone_results = run_standalone_benchmark()
        if not standalone_results:
            log_info("Standalone benchmark failed")
            return False

        time.sleep(5)

        # Run Raft benchmark
        raft_results = run_raft_benchmark()
        if not raft_results:
            log_info("Raft benchmark failed")
            return False

        # Print comparison
        print_comparison(standalone_results, raft_results)

        return True

    except Exception as e:
        log_info(f"Benchmark failed: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    success = main()
    sys.exit(0 if success else 1)
