#!/usr/bin/env python3
"""
Quick profile benchmark - tests key scenarios only.
"""

import os
import sys
import time
import subprocess
from kafka import KafkaProducer, KafkaConsumer


PROFILES = ['low-latency', 'balanced', 'high-throughput']
WORKLOAD = {'messages': 10_000, 'timeout_ms': 60_000}


def kill_servers():
    subprocess.run(['pkill', '-9', 'chronik-server'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)


def start_server(profile):
    kill_servers()
    subprocess.run(['rm', '-rf', './data'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    env = os.environ.copy()
    if profile:
        env['CHRONIK_PRODUCE_PROFILE'] = profile

    process = subprocess.Popen(
        ['./target/release/chronik-server', '--advertised-addr', 'localhost', 'standalone'],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(3)
    return process


def test_profile(profile_name):
    topic = f'test-{profile_name}-10k-snappy'

    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0),
        request_timeout_ms=120000,
        compression_type='snappy',
    )

    # Produce
    start = time.time()
    for i in range(WORKLOAD['messages']):
        producer.send(topic, value=f'msg-{i}-{"x"*100}'.encode())
    producer.flush(timeout=30)
    producer.close()
    produce_time = time.time() - start
    produce_rate = int(WORKLOAD['messages'] / produce_time)

    time.sleep(1)

    # Consume
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers='localhost:9092',
        group_id=f'group-{topic}',
        auto_offset_reset='earliest',
        consumer_timeout_ms=WORKLOAD['timeout_ms'],
        api_version=(0, 10, 0),
    )

    start = time.time()
    consumed = sum(1 for _ in consumer)
    consume_time = time.time() - start
    consumer.close()

    consume_rate = int(consumed / consume_time) if consume_time > 0 else 0
    success_rate = (consumed / WORKLOAD['messages'] * 100)

    return {
        'produce_rate': produce_rate,
        'consume_rate': consume_rate,
        'success_rate': success_rate,
        'consumed': consumed,
    }


def main():
    print("\n" + "="*80)
    print("ProduceHandler Profile Quick Benchmark (10K Snappy)")
    print("="*80 + "\n")

    if not os.path.exists('./target/release/chronik-server'):
        print("Error: Build chronik-server first!")
        return 1

    results = {}

    for profile in PROFILES:
        display_name = profile.title() if profile else 'Balanced'
        print(f"Testing {display_name}...", end=' ', flush=True)

        server = start_server(profile)

        try:
            result = test_profile(profile or 'balanced')
            results[profile or 'balanced'] = result

            color = '\033[92m' if result['success_rate'] >= 95 else '\033[93m'
            print(f"{color}{result['success_rate']:.1f}% success\033[0m "
                  f"(produce: {result['produce_rate']:,}/s, consume: {result['consume_rate']:,}/s, "
                  f"consumed: {result['consumed']:,})")

        except Exception as e:
            print(f"\033[91mFAILED: {e}\033[0m")
            results[profile or 'balanced'] = {'error': str(e)}

        finally:
            server.terminate()
            try:
                server.wait(timeout=3)
            except:
                server.kill()
            time.sleep(1)

    # Summary
    print("\n" + "="*80)
    print("SUMMARY")
    print("="*80 + "\n")

    print(f"{'Profile':<20} {'Success':>10} {'Produce':>15} {'Consume':>15} {'Consumed':>12}")
    print("-"*80)

    for profile in PROFILES:
        key = profile or 'balanced'
        if 'error' not in results[key]:
            r = results[key]
            print(f"{profile.title():<20} {r['success_rate']:>9.1f}% {r['produce_rate']:>14,}/s "
                  f"{r['consume_rate']:>14,}/s {r['consumed']:>12,}")

    print("\nProfile Settings:")
    print("  Low-Latency:     1 batch / 10ms  / 16MB buffer")
    print("  Balanced:       10 batches / 100ms / 32MB buffer")
    print("  High-Throughput: 100 batches / 500ms / 128MB buffer")
    print()

    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\nInterrupted")
        kill_servers()
        sys.exit(1)
    finally:
        kill_servers()
