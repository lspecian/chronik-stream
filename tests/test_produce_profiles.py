#!/usr/bin/env python3
"""
Comprehensive benchmark for ProduceHandler flush profiles.

Tests all 3 profiles (LowLatency, Balanced, HighThroughput) with various workloads:
- Small (1K messages)
- Medium (10K messages)
- Large (50K messages)
- XLarge (100K messages)

Measures:
- Produce rate (msg/s)
- Consume rate (msg/s)
- Success rate (%)
- End-to-end latency (p50, p95, p99)
"""

import os
import sys
import time
import signal
import subprocess
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaTimeoutError, KafkaError


# Test configurations
PROFILES = {
    'low-latency': {
        'env': 'CHRONIK_PRODUCE_PROFILE=low-latency',
        'name': 'LowLatency',
        'expected_latency': '< 20ms p99',
    },
    'balanced': {
        'env': 'CHRONIK_PRODUCE_PROFILE=balanced',
        'name': 'Balanced',
        'expected_latency': '100-150ms p99',
    },
    'high-throughput': {
        'env': 'CHRONIK_PRODUCE_PROFILE=high-throughput',
        'name': 'HighThroughput',
        'expected_latency': '< 500ms p99',
    },
}

WORKLOADS = [
    {'name': '1k', 'messages': 1_000, 'timeout_ms': 30_000},
    {'name': '10k', 'messages': 10_000, 'timeout_ms': 60_000},
    {'name': '50k', 'messages': 50_000, 'timeout_ms': 120_000},
    {'name': '100k', 'messages': 100_000, 'timeout_ms': 180_000},
]

COMPRESSION_TYPES = ['snappy', 'none']


class Colors:
    HEADER = '\033[95m'
    OKBLUE = '\033[94m'
    OKCYAN = '\033[96m'
    OKGREEN = '\033[92m'
    WARNING = '\033[93m'
    FAIL = '\033[91m'
    ENDC = '\033[0m'
    BOLD = '\033[1m'


def kill_existing_servers():
    """Kill any existing chronik-server processes"""
    try:
        subprocess.run(['pkill', '-9', 'chronik-server'],
                      stdout=subprocess.DEVNULL,
                      stderr=subprocess.DEVNULL)
        time.sleep(2)
    except Exception:
        pass


def start_server(profile_name):
    """Start chronik-server with specified profile"""
    kill_existing_servers()

    # Clean data directory
    subprocess.run(['rm', '-rf', './data'],
                  stdout=subprocess.DEVNULL,
                  stderr=subprocess.DEVNULL)

    env = os.environ.copy()
    env_var = PROFILES[profile_name]['env']
    key, value = env_var.split('=')
    env[key] = value

    print(f"Starting server with profile: {PROFILES[profile_name]['name']} ({env_var})")

    process = subprocess.Popen(
        ['./target/release/chronik-server', '--advertised-addr', 'localhost', 'standalone'],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    time.sleep(3)  # Wait for server to start
    return process


def test_workload(profile_name, workload, compression):
    """Test a specific workload configuration"""
    topic = f'test-{profile_name}-{workload["name"]}-{compression}'
    total_messages = workload['messages']
    timeout_ms = workload['timeout_ms']

    # Create producer
    producer_config = {
        'bootstrap_servers': 'localhost:9092',
        'api_version': (0, 10, 0),
        'request_timeout_ms': 120000,
        'compression_type': compression if compression != 'none' else None,
    }

    try:
        producer = KafkaProducer(**producer_config)
    except Exception as e:
        return {
            'error': f'Producer failed: {e}',
            'produced': 0,
            'consumed': 0,
            'success_rate': 0.0,
            'produce_rate': 0,
            'consume_rate': 0,
        }

    # Produce messages
    produce_start = time.time()
    produced = 0

    try:
        for i in range(total_messages):
            message = f'message-{i}-{"x" * 100}'.encode('utf-8')  # ~120 bytes each
            future = producer.send(topic, value=message)
            # Don't wait for ack to test server buffering (realistic scenario)
            produced += 1

        producer.flush(timeout=30)  # Wait for all sends to complete
    except Exception as e:
        print(f"  {Colors.FAIL}Produce error: {e}{Colors.ENDC}")
    finally:
        producer.close()

    produce_elapsed = time.time() - produce_start
    produce_rate = int(produced / produce_elapsed) if produce_elapsed > 0 else 0

    # Wait a bit for messages to be available
    time.sleep(1)

    # Consume messages
    consumer_config = {
        'bootstrap_servers': 'localhost:9092',
        'group_id': f'test-group-{topic}',
        'auto_offset_reset': 'earliest',
        'consumer_timeout_ms': timeout_ms,
        'enable_auto_commit': True,
        'api_version': (0, 10, 0),
    }

    try:
        consumer = KafkaConsumer(topic, **consumer_config)
    except Exception as e:
        return {
            'error': f'Consumer failed: {e}',
            'produced': produced,
            'consumed': 0,
            'success_rate': 0.0,
            'produce_rate': produce_rate,
            'consume_rate': 0,
        }

    consume_start = time.time()
    consumed = 0
    latencies = []

    try:
        for message in consumer:
            consumed += 1
            # Calculate end-to-end latency
            if message.timestamp:
                latency_ms = (time.time() * 1000) - message.timestamp
                latencies.append(latency_ms)
    except Exception as e:
        # Consumer timeout is expected when all messages are consumed
        pass
    finally:
        consumer.close()

    consume_elapsed = time.time() - consume_start
    consume_rate = int(consumed / consume_elapsed) if consume_elapsed > 0 else 0

    success_rate = (consumed / produced * 100) if produced > 0 else 0.0

    # Calculate latency percentiles
    p50 = p95 = p99 = 0
    if latencies:
        latencies.sort()
        p50 = latencies[int(len(latencies) * 0.50)]
        p95 = latencies[int(len(latencies) * 0.95)]
        p99 = latencies[int(len(latencies) * 0.99)]

    return {
        'produced': produced,
        'consumed': consumed,
        'success_rate': success_rate,
        'produce_rate': produce_rate,
        'consume_rate': consume_rate,
        'latency_p50': p50,
        'latency_p95': p95,
        'latency_p99': p99,
    }


def main():
    print(f"\n{Colors.BOLD}{'='*80}")
    print("ProduceHandler Flush Profile Benchmark")
    print(f"{'='*80}{Colors.ENDC}\n")

    if not os.path.exists('./target/release/chronik-server'):
        print(f"{Colors.FAIL}Error: chronik-server binary not found!{Colors.ENDC}")
        print("Run: cargo build --release --bin chronik-server")
        return 1

    results = {}

    for profile_name in PROFILES.keys():
        print(f"\n{Colors.HEADER}{Colors.BOLD}Testing Profile: {PROFILES[profile_name]['name']}{Colors.ENDC}")
        print(f"Expected latency: {PROFILES[profile_name]['expected_latency']}\n")

        # Start server with this profile
        server_process = start_server(profile_name)

        try:
            profile_results = []

            for workload in WORKLOADS:
                for compression in COMPRESSION_TYPES:
                    test_name = f"{workload['name']}-{compression}"
                    print(f"  Testing {test_name}...", end=' ', flush=True)

                    result = test_workload(profile_name, workload, compression)

                    if 'error' in result:
                        print(f"{Colors.FAIL}FAILED: {result['error']}{Colors.ENDC}")
                    else:
                        success = result['success_rate']
                        color = Colors.OKGREEN if success >= 95 else Colors.WARNING if success >= 80 else Colors.FAIL
                        print(f"{color}{success:.1f}% success{Colors.ENDC} "
                              f"(produce: {result['produce_rate']:,} msg/s, "
                              f"consume: {result['consume_rate']:,} msg/s, "
                              f"p99: {result['latency_p99']:.1f}ms)")

                    profile_results.append({
                        'test': test_name,
                        **result
                    })

            results[profile_name] = profile_results

        finally:
            # Stop server
            server_process.terminate()
            try:
                server_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server_process.kill()
            time.sleep(1)

    # Print comparison table
    print(f"\n{Colors.BOLD}{'='*120}")
    print("PROFILE COMPARISON")
    print(f"{'='*120}{Colors.ENDC}\n")

    print(f"{'Test':<20} {'Profile':<15} {'Produced':>10} {'Consumed':>10} {'Success':>10} "
          f"{'Produce':>12} {'Consume':>12} {'p50':>8} {'p95':>8} {'p99':>8}")
    print(f"{'-'*20} {'-'*15} {'-'*10} {'-'*10} {'-'*10} {'-'*12} {'-'*12} {'-'*8} {'-'*8} {'-'*8}")

    for profile_name in PROFILES.keys():
        for result in results[profile_name]:
            if 'error' not in result:
                print(f"{result['test']:<20} {PROFILES[profile_name]['name']:<15} "
                      f"{result['produced']:>10,} {result['consumed']:>10,} "
                      f"{result['success_rate']:>9.1f}% "
                      f"{result['produce_rate']:>11,}/s "
                      f"{result['consume_rate']:>11,}/s "
                      f"{result['latency_p50']:>7.0f}ms "
                      f"{result['latency_p95']:>7.0f}ms "
                      f"{result['latency_p99']:>7.0f}ms")

    print(f"\n{Colors.BOLD}Profile Characteristics:{Colors.ENDC}")
    for profile_name, config in PROFILES.items():
        print(f"  {config['name']}: {config['expected_latency']}")

    print(f"\n{Colors.OKGREEN}Benchmark complete!{Colors.ENDC}\n")
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print(f"\n{Colors.WARNING}Interrupted by user{Colors.ENDC}")
        kill_existing_servers()
        sys.exit(1)
    finally:
        kill_existing_servers()
