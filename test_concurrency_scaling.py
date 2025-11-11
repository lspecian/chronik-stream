#!/usr/bin/env python3
"""
Test how throughput scales with concurrency to identify bottleneck.
If Raft is the bottleneck, throughput will plateau at low concurrency.
"""

import time
import threading
from kafka import KafkaProducer
import statistics

BOOTSTRAP_SERVERS = 'localhost:9092,localhost:9093,localhost:9094'
MESSAGE_SIZE = 256
MESSAGES_PER_THREAD = 100  # Each thread sends 100 messages
TOPIC = 'concurrency-scaling-test'

def benchmark_concurrency(num_threads):
    """Run benchmark with specified number of threads"""
    results = {
        'success': 0,
        'failed': 0,
        'latencies': [],
    }
    lock = threading.Lock()
    
    def producer_thread(thread_id):
        producer = KafkaProducer(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            acks=1,
            api_version=(0, 10, 0),
            request_timeout_ms=10000
        )
        
        local_success = 0
        local_latencies = []
        
        try:
            for i in range(MESSAGES_PER_THREAD):
                start = time.time()
                try:
                    future = producer.send(TOPIC, b'X' * MESSAGE_SIZE)
                    future.get(timeout=10)
                    latency_ms = (time.time() - start) * 1000
                    local_latencies.append(latency_ms)
                    local_success += 1
                except Exception:
                    pass
        finally:
            producer.close()
        
        with lock:
            results['success'] += local_success
            results['latencies'].extend(local_latencies)
    
    # Run benchmark
    start_time = time.time()
    
    threads = []
    for i in range(num_threads):
        t = threading.Thread(target=producer_thread, args=(i,))
        t.start()
        threads.append(t)
    
    for t in threads:
        t.join()
    
    elapsed = time.time() - start_time
    
    throughput = results['success'] / elapsed if elapsed > 0 else 0
    
    latencies = sorted(results['latencies']) if results['latencies'] else []
    p50 = latencies[len(latencies)//2] if latencies else 0
    p95 = latencies[int(len(latencies)*0.95)] if latencies else 0
    p99 = latencies[int(len(latencies)*0.99)] if latencies else 0
    
    return {
        'threads': num_threads,
        'throughput': throughput,
        'latency_p50': p50,
        'latency_p95': p95,
        'latency_p99': p99,
        'success': results['success'],
        'total': num_threads * MESSAGES_PER_THREAD,
    }

print("=" * 80)
print("Chronik Concurrency Scaling Test")
print("=" * 80)
print(f"Configuration: {MESSAGES_PER_THREAD} messages/thread, {MESSAGE_SIZE} byte messages")
print("Testing concurrency levels: 1, 2, 4, 8, 16, 32, 64, 128")
print("=" * 80)
print()

concurrency_levels = [1, 2, 4, 8, 16, 32, 64, 128]
results = []

for concurrency in concurrency_levels:
    print(f"Testing {concurrency} threads...", end='', flush=True)
    result = benchmark_concurrency(concurrency)
    results.append(result)
    print(f" {result['throughput']:.0f} msg/s (p50: {result['latency_p50']:.1f}ms, p99: {result['latency_p99']:.1f}ms)")

print()
print("=" * 80)
print("Results Summary:")
print("=" * 80)
print(f"{'Threads':<10} {'Throughput':<15} {'p50 Latency':<15} {'p95 Latency':<15} {'p99 Latency':<15}")
print("-" * 80)

for r in results:
    print(f"{r['threads']:<10} {r['throughput']:>10.0f} msg/s  {r['latency_p50']:>10.1f}ms   {r['latency_p95']:>10.1f}ms   {r['latency_p99']:>10.1f}ms")

print("=" * 80)
print()

# Analyze scaling
print("Scaling Analysis:")
print("-" * 80)

for i in range(1, len(results)):
    prev = results[i-1]
    curr = results[i]
    
    thread_ratio = curr['threads'] / prev['threads']
    throughput_ratio = curr['throughput'] / prev['throughput'] if prev['throughput'] > 0 else 0
    efficiency = (throughput_ratio / thread_ratio) * 100
    
    print(f"{prev['threads']:>3} → {curr['threads']:>3} threads: "
          f"Throughput +{(throughput_ratio - 1) * 100:>6.1f}%  "
          f"(Efficiency: {efficiency:>5.1f}%)")

print()
print("Interpretation:")
print("  100% efficiency = Perfect linear scaling (doubling threads doubles throughput)")
print("   50% efficiency = Bottleneck limiting scaling")
print("  <25% efficiency = Severe bottleneck (likely Raft consensus overhead)")
print("=" * 80)

# Find plateau point
plateau_found = False
for i in range(2, len(results)):
    prev = results[i-2]
    curr = results[i]
    
    throughput_gain = (curr['throughput'] - prev['throughput']) / prev['throughput']
    
    if throughput_gain < 0.1:  # Less than 10% gain
        print(f"\n⚠️  PLATEAU DETECTED at {prev['threads']} threads:")
        print(f"    Throughput barely increases beyond this point.")
        print(f"    This suggests the bottleneck is NOT in client-side concurrency,")
        print(f"    but in server-side processing (likely Raft consensus).")
        plateau_found = True
        break

if not plateau_found:
    print(f"\n✓ No clear plateau detected. Throughput continues scaling with concurrency.")
    print(f"  Peak throughput: {results[-1]['throughput']:.0f} msg/s at {results[-1]['threads']} threads")

