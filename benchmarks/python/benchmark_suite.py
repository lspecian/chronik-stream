#!/usr/bin/env python3
"""
Comprehensive Performance Benchmark Suite for Chronik Stream

This suite tests:
1. Producer throughput (messages/sec, MB/sec)
2. Consumer throughput 
3. End-to-end latency
4. Consumer group performance
5. Concurrent client scalability
"""

import time
import json
import uuid
import statistics
import threading
import multiprocessing
from datetime import datetime
from typing import List, Dict, Any, Tuple
import argparse
import sys
import os

from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import TopicAlreadyExistsError
from tabulate import tabulate
import matplotlib.pyplot as plt
import numpy as np


class BenchmarkConfig:
    """Configuration for benchmark tests"""
    def __init__(self):
        self.bootstrap_servers = ['localhost:9092']
        self.test_topic_prefix = f'bench-{uuid.uuid4().hex[:8]}'
        self.num_messages = 1_000_000
        self.message_size = 1024  # 1KB
        self.batch_size = 100
        self.num_producers = 1
        self.num_consumers = 1
        self.num_partitions = 10
        self.replication_factor = 1
        self.compression_type = 'none'
        self.acks = 1
        self.results_dir = 'results'


class BenchmarkResults:
    """Container for benchmark results"""
    def __init__(self, test_name: str):
        self.test_name = test_name
        self.start_time = None
        self.end_time = None
        self.messages_sent = 0
        self.bytes_sent = 0
        self.messages_received = 0
        self.bytes_received = 0
        self.latencies = []
        self.errors = []
        self.metadata = {}
    
    @property
    def duration(self) -> float:
        if self.start_time and self.end_time:
            return self.end_time - self.start_time
        return 0
    
    @property
    def throughput_msgs_sec(self) -> float:
        if self.duration > 0:
            return self.messages_sent / self.duration
        return 0
    
    @property
    def throughput_mb_sec(self) -> float:
        if self.duration > 0:
            return (self.bytes_sent / 1024 / 1024) / self.duration
        return 0
    
    @property
    def avg_latency_ms(self) -> float:
        if self.latencies:
            return statistics.mean(self.latencies) * 1000
        return 0
    
    @property
    def p99_latency_ms(self) -> float:
        if self.latencies:
            sorted_latencies = sorted(self.latencies)
            idx = int(len(sorted_latencies) * 0.99)
            return sorted_latencies[idx] * 1000
        return 0
    
    def to_dict(self) -> Dict[str, Any]:
        return {
            'test_name': self.test_name,
            'duration_sec': self.duration,
            'messages_sent': self.messages_sent,
            'bytes_sent': self.bytes_sent,
            'throughput_msgs_sec': self.throughput_msgs_sec,
            'throughput_mb_sec': self.throughput_mb_sec,
            'avg_latency_ms': self.avg_latency_ms,
            'p99_latency_ms': self.p99_latency_ms,
            'errors': len(self.errors),
            'metadata': self.metadata
        }


class ChronikBenchmark:
    """Main benchmark runner"""
    
    def __init__(self, config: BenchmarkConfig):
        self.config = config
        self.admin_client = None
        self.results = []
        
    def setup(self):
        """Initialize admin client and create test topic"""
        self.admin_client = KafkaAdminClient(
            bootstrap_servers=self.config.bootstrap_servers,
            client_id='benchmark-admin'
        )
        
        # Create test topic
        topic = NewTopic(
            name=f"{self.config.test_topic_prefix}-main",
            num_partitions=self.config.num_partitions,
            replication_factor=self.config.replication_factor
        )
        
        try:
            self.admin_client.create_topics([topic], timeout_ms=30000)
            print(f"Created topic: {topic.name}")
        except TopicAlreadyExistsError:
            print(f"Topic {topic.name} already exists")
    
    def cleanup(self):
        """Clean up test topics"""
        if self.admin_client:
            topics = self.admin_client.list_topics()
            test_topics = [t for t in topics if t.startswith(self.config.test_topic_prefix)]
            
            if test_topics:
                try:
                    self.admin_client.delete_topics(test_topics, timeout_ms=30000)
                    print(f"Deleted {len(test_topics)} test topics")
                except Exception as e:
                    print(f"Failed to delete topics: {e}")
            
            self.admin_client.close()
    
    def benchmark_producer_throughput(self) -> BenchmarkResults:
        """Test producer throughput"""
        print("\n=== Producer Throughput Benchmark ===")
        results = BenchmarkResults("producer_throughput")
        topic = f"{self.config.test_topic_prefix}-main"
        
        # Generate test data
        message = b'x' * self.config.message_size
        
        producer = KafkaProducer(
            bootstrap_servers=self.config.bootstrap_servers,
            compression_type=self.config.compression_type,
            acks=self.config.acks,
            batch_size=self.config.batch_size * self.config.message_size,
            linger_ms=10
        )
        
        print(f"Sending {self.config.num_messages:,} messages of {self.config.message_size} bytes...")
        results.start_time = time.time()
        
        try:
            for i in range(self.config.num_messages):
                producer.send(topic, value=message, key=f"key-{i}".encode())
                results.messages_sent += 1
                results.bytes_sent += len(message)
                
                if i % 10000 == 0 and i > 0:
                    elapsed = time.time() - results.start_time
                    rate = i / elapsed
                    print(f"  Progress: {i:,}/{self.config.num_messages:,} messages "
                          f"({rate:.0f} msg/s)")
            
            # Flush remaining messages
            producer.flush()
            
        except Exception as e:
            results.errors.append(str(e))
            print(f"Error during benchmark: {e}")
        finally:
            producer.close()
            results.end_time = time.time()
        
        print(f"\nResults:")
        print(f"  Duration: {results.duration:.2f} seconds")
        print(f"  Messages sent: {results.messages_sent:,}")
        print(f"  Throughput: {results.throughput_msgs_sec:,.0f} msg/s")
        print(f"  Throughput: {results.throughput_mb_sec:.2f} MB/s")
        
        return results
    
    def benchmark_consumer_throughput(self) -> BenchmarkResults:
        """Test consumer throughput"""
        print("\n=== Consumer Throughput Benchmark ===")
        results = BenchmarkResults("consumer_throughput")
        topic = f"{self.config.test_topic_prefix}-main"
        
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=self.config.bootstrap_servers,
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=5000,
            fetch_max_bytes=52428800,  # 50MB
            max_poll_records=500
        )
        
        print(f"Consuming messages from {topic}...")
        results.start_time = time.time()
        
        try:
            for message in consumer:
                results.messages_received += 1
                results.bytes_received += len(message.value)
                
                if results.messages_received % 10000 == 0:
                    elapsed = time.time() - results.start_time
                    rate = results.messages_received / elapsed
                    print(f"  Progress: {results.messages_received:,} messages "
                          f"({rate:.0f} msg/s)")
                
                # Stop after consuming all produced messages
                if results.messages_received >= self.config.num_messages:
                    break
                    
        except Exception as e:
            results.errors.append(str(e))
            print(f"Error during benchmark: {e}")
        finally:
            consumer.close()
            results.end_time = time.time()
        
        print(f"\nResults:")
        print(f"  Duration: {results.duration:.2f} seconds")
        print(f"  Messages consumed: {results.messages_received:,}")
        print(f"  Throughput: {results.messages_received / results.duration:,.0f} msg/s")
        print(f"  Throughput: {(results.bytes_received / 1024 / 1024) / results.duration:.2f} MB/s")
        
        return results
    
    def benchmark_end_to_end_latency(self, num_samples: int = 1000) -> BenchmarkResults:
        """Test end-to-end latency"""
        print("\n=== End-to-End Latency Benchmark ===")
        results = BenchmarkResults("e2e_latency")
        topic = f"{self.config.test_topic_prefix}-latency"
        
        # Create topic
        try:
            self.admin_client.create_topics([
                NewTopic(name=topic, num_partitions=1, replication_factor=1)
            ])
        except TopicAlreadyExistsError:
            pass
        
        # Set up consumer first
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=self.config.bootstrap_servers,
            auto_offset_reset='latest',
            enable_auto_commit=False,
            consumer_timeout_ms=1000
        )
        
        # Set up producer
        producer = KafkaProducer(
            bootstrap_servers=self.config.bootstrap_servers,
            acks='all',
            compression_type='none',
            max_in_flight_requests_per_connection=1
        )
        
        print(f"Measuring latency with {num_samples} samples...")
        results.start_time = time.time()
        
        # Consumer thread
        received_messages = {}
        consumer_running = True
        
        def consume_messages():
            while consumer_running:
                messages = consumer.poll(timeout_ms=100)
                for topic_partition, records in messages.items():
                    for record in records:
                        try:
                            msg_data = json.loads(record.value.decode('utf-8'))
                            received_messages[msg_data['id']] = record.timestamp / 1000
                        except:
                            pass
        
        consumer_thread = threading.Thread(target=consume_messages)
        consumer_thread.start()
        
        # Give consumer time to start
        time.sleep(1)
        
        # Send messages and measure latency
        for i in range(num_samples):
            msg_id = str(uuid.uuid4())
            msg_data = {
                'id': msg_id,
                'timestamp': time.time(),
                'sequence': i
            }
            
            # Send message
            send_time = time.time()
            future = producer.send(
                topic,
                value=json.dumps(msg_data).encode('utf-8')
            )
            
            # Wait for send confirmation
            try:
                metadata = future.get(timeout=10)
                
                # Wait for message to be consumed
                timeout = time.time() + 5
                while msg_id not in received_messages and time.time() < timeout:
                    time.sleep(0.001)
                
                if msg_id in received_messages:
                    latency = received_messages[msg_id] - send_time
                    results.latencies.append(latency)
                    results.messages_sent += 1
                    
                    if i % 100 == 0:
                        avg_latency = statistics.mean(results.latencies) * 1000
                        print(f"  Progress: {i}/{num_samples}, "
                              f"Avg latency: {avg_latency:.2f} ms")
                
            except Exception as e:
                results.errors.append(f"Message {i}: {str(e)}")
        
        # Cleanup
        consumer_running = False
        consumer_thread.join()
        producer.close()
        consumer.close()
        results.end_time = time.time()
        
        if results.latencies:
            sorted_latencies = sorted(results.latencies)
            print(f"\nResults:")
            print(f"  Samples: {len(results.latencies)}")
            print(f"  Min latency: {min(results.latencies) * 1000:.2f} ms")
            print(f"  Avg latency: {statistics.mean(results.latencies) * 1000:.2f} ms")
            print(f"  P50 latency: {sorted_latencies[len(sorted_latencies)//2] * 1000:.2f} ms")
            print(f"  P95 latency: {sorted_latencies[int(len(sorted_latencies)*0.95)] * 1000:.2f} ms")
            print(f"  P99 latency: {sorted_latencies[int(len(sorted_latencies)*0.99)] * 1000:.2f} ms")
            print(f"  Max latency: {max(results.latencies) * 1000:.2f} ms")
        
        return results
    
    def benchmark_concurrent_producers(self) -> BenchmarkResults:
        """Test performance with multiple concurrent producers"""
        print(f"\n=== Concurrent Producers Benchmark ({self.config.num_producers} producers) ===")
        results = BenchmarkResults("concurrent_producers")
        topic = f"{self.config.test_topic_prefix}-main"
        
        messages_per_producer = self.config.num_messages // self.config.num_producers
        producer_results = []
        threads = []
        
        def producer_worker(worker_id: int, messages_to_send: int):
            worker_results = {
                'messages_sent': 0,
                'bytes_sent': 0,
                'errors': []
            }
            
            producer = KafkaProducer(
                bootstrap_servers=self.config.bootstrap_servers,
                compression_type=self.config.compression_type,
                client_id=f'producer-{worker_id}'
            )
            
            message = b'x' * self.config.message_size
            
            try:
                for i in range(messages_to_send):
                    producer.send(topic, value=message)
                    worker_results['messages_sent'] += 1
                    worker_results['bytes_sent'] += len(message)
                
                producer.flush()
            except Exception as e:
                worker_results['errors'].append(str(e))
            finally:
                producer.close()
            
            producer_results.append(worker_results)
        
        print(f"Starting {self.config.num_producers} producer threads...")
        results.start_time = time.time()
        
        # Start producer threads
        for i in range(self.config.num_producers):
            thread = threading.Thread(
                target=producer_worker,
                args=(i, messages_per_producer)
            )
            thread.start()
            threads.append(thread)
        
        # Wait for all producers to finish
        for thread in threads:
            thread.join()
        
        results.end_time = time.time()
        
        # Aggregate results
        for worker_result in producer_results:
            results.messages_sent += worker_result['messages_sent']
            results.bytes_sent += worker_result['bytes_sent']
            results.errors.extend(worker_result['errors'])
        
        print(f"\nResults:")
        print(f"  Duration: {results.duration:.2f} seconds")
        print(f"  Total messages sent: {results.messages_sent:,}")
        print(f"  Aggregate throughput: {results.throughput_msgs_sec:,.0f} msg/s")
        print(f"  Aggregate throughput: {results.throughput_mb_sec:.2f} MB/s")
        print(f"  Per-producer throughput: {results.throughput_msgs_sec / self.config.num_producers:,.0f} msg/s")
        
        return results
    
    def benchmark_consumer_groups(self) -> BenchmarkResults:
        """Test consumer group performance"""
        print(f"\n=== Consumer Groups Benchmark ({self.config.num_consumers} consumers) ===")
        results = BenchmarkResults("consumer_groups")
        topic = f"{self.config.test_topic_prefix}-main"
        group_id = f"bench-group-{uuid.uuid4().hex[:8]}"
        
        consumer_results = []
        threads = []
        stop_consumers = threading.Event()
        
        def consumer_worker(worker_id: int):
            worker_results = {
                'messages_received': 0,
                'bytes_received': 0,
                'errors': []
            }
            
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=self.config.bootstrap_servers,
                group_id=group_id,
                auto_offset_reset='earliest',
                enable_auto_commit=True,
                consumer_timeout_ms=1000,
                client_id=f'consumer-{worker_id}'
            )
            
            try:
                while not stop_consumers.is_set():
                    messages = consumer.poll(timeout_ms=1000)
                    for topic_partition, records in messages.items():
                        for record in records:
                            worker_results['messages_received'] += 1
                            worker_results['bytes_received'] += len(record.value)
            except Exception as e:
                worker_results['errors'].append(str(e))
            finally:
                consumer.close()
            
            consumer_results.append(worker_results)
        
        print(f"Starting {self.config.num_consumers} consumer threads...")
        results.start_time = time.time()
        
        # Start consumer threads
        for i in range(self.config.num_consumers):
            thread = threading.Thread(
                target=consumer_worker,
                args=(i,)
            )
            thread.start()
            threads.append(thread)
        
        # Let consumers run for a fixed duration
        print("Consumers running for 30 seconds...")
        time.sleep(30)
        
        # Stop consumers
        stop_consumers.set()
        for thread in threads:
            thread.join()
        
        results.end_time = time.time()
        
        # Aggregate results
        for worker_result in consumer_results:
            results.messages_received += worker_result['messages_received']
            results.bytes_received += worker_result['bytes_received']
            results.errors.extend(worker_result['errors'])
        
        consume_rate = results.messages_received / results.duration
        
        print(f"\nResults:")
        print(f"  Duration: {results.duration:.2f} seconds")
        print(f"  Total messages consumed: {results.messages_received:,}")
        print(f"  Aggregate throughput: {consume_rate:,.0f} msg/s")
        print(f"  Per-consumer throughput: {consume_rate / self.config.num_consumers:,.0f} msg/s")
        
        return results
    
    def run_all_benchmarks(self):
        """Run all benchmark tests"""
        print("=" * 60)
        print("Chronik Stream Performance Benchmark Suite")
        print("=" * 60)
        print(f"\nConfiguration:")
        print(f"  Bootstrap servers: {self.config.bootstrap_servers}")
        print(f"  Message size: {self.config.message_size} bytes")
        print(f"  Number of messages: {self.config.num_messages:,}")
        print(f"  Partitions: {self.config.num_partitions}")
        print(f"  Compression: {self.config.compression_type}")
        
        self.setup()
        
        try:
            # Run benchmarks
            self.results.append(self.benchmark_producer_throughput())
            self.results.append(self.benchmark_consumer_throughput())
            self.results.append(self.benchmark_end_to_end_latency())
            
            # Multi-client benchmarks
            self.config.num_producers = 3
            self.results.append(self.benchmark_concurrent_producers())
            
            self.config.num_consumers = 3
            self.results.append(self.benchmark_consumer_groups())
            
            # Generate report
            self.generate_report()
            
        finally:
            self.cleanup()
    
    def generate_report(self):
        """Generate benchmark report with tables and charts"""
        print("\n" + "=" * 60)
        print("BENCHMARK SUMMARY")
        print("=" * 60)
        
        # Create results table
        table_data = []
        for result in self.results:
            table_data.append([
                result.test_name,
                f"{result.duration:.2f}",
                f"{result.messages_sent:,}",
                f"{result.throughput_msgs_sec:,.0f}",
                f"{result.throughput_mb_sec:.2f}",
                f"{result.avg_latency_ms:.2f}" if result.latencies else "N/A",
                f"{result.p99_latency_ms:.2f}" if result.latencies else "N/A",
                len(result.errors)
            ])
        
        headers = ["Test", "Duration (s)", "Messages", "Throughput (msg/s)", 
                   "Throughput (MB/s)", "Avg Latency (ms)", "P99 Latency (ms)", "Errors"]
        
        print(tabulate(table_data, headers=headers, tablefmt="grid"))
        
        # Save results to JSON
        os.makedirs(self.config.results_dir, exist_ok=True)
        timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
        results_file = os.path.join(self.config.results_dir, f"benchmark_results_{timestamp}.json")
        
        with open(results_file, 'w') as f:
            json.dump({
                'timestamp': timestamp,
                'config': self.config.__dict__,
                'results': [r.to_dict() for r in self.results]
            }, f, indent=2)
        
        print(f"\nResults saved to: {results_file}")
        
        # Generate charts
        self.generate_charts(timestamp)
    
    def generate_charts(self, timestamp: str):
        """Generate performance charts"""
        # Throughput comparison chart
        plt.figure(figsize=(10, 6))
        
        test_names = []
        throughputs = []
        
        for result in self.results:
            if result.throughput_msgs_sec > 0:
                test_names.append(result.test_name.replace('_', ' ').title())
                throughputs.append(result.throughput_msgs_sec)
        
        plt.bar(test_names, throughputs)
        plt.xlabel('Test')
        plt.ylabel('Throughput (messages/second)')
        plt.title('Chronik Stream Throughput Benchmark Results')
        plt.xticks(rotation=45, ha='right')
        plt.tight_layout()
        
        chart_file = os.path.join(self.config.results_dir, f"throughput_chart_{timestamp}.png")
        plt.savefig(chart_file)
        print(f"Throughput chart saved to: {chart_file}")
        
        # Latency distribution chart (if we have latency data)
        latency_result = next((r for r in self.results if r.test_name == "e2e_latency"), None)
        if latency_result and latency_result.latencies:
            plt.figure(figsize=(10, 6))
            
            latencies_ms = [l * 1000 for l in latency_result.latencies]
            plt.hist(latencies_ms, bins=50, edgecolor='black')
            plt.xlabel('Latency (ms)')
            plt.ylabel('Frequency')
            plt.title('End-to-End Latency Distribution')
            plt.axvline(latency_result.avg_latency_ms, color='red', 
                       linestyle='dashed', linewidth=2, label=f'Avg: {latency_result.avg_latency_ms:.2f} ms')
            plt.axvline(latency_result.p99_latency_ms, color='orange', 
                       linestyle='dashed', linewidth=2, label=f'P99: {latency_result.p99_latency_ms:.2f} ms')
            plt.legend()
            plt.tight_layout()
            
            latency_chart_file = os.path.join(self.config.results_dir, f"latency_distribution_{timestamp}.png")
            plt.savefig(latency_chart_file)
            print(f"Latency chart saved to: {latency_chart_file}")


def main():
    parser = argparse.ArgumentParser(description='Chronik Stream Performance Benchmark')
    parser.add_argument('--bootstrap-servers', default='localhost:9092',
                       help='Kafka bootstrap servers (default: localhost:9092)')
    parser.add_argument('--num-messages', type=int, default=1_000_000,
                       help='Number of messages to send (default: 1,000,000)')
    parser.add_argument('--message-size', type=int, default=1024,
                       help='Message size in bytes (default: 1024)')
    parser.add_argument('--num-partitions', type=int, default=10,
                       help='Number of partitions (default: 10)')
    parser.add_argument('--compression', choices=['none', 'gzip', 'snappy', 'lz4', 'zstd'],
                       default='none', help='Compression type (default: none)')
    parser.add_argument('--results-dir', default='results',
                       help='Directory to save results (default: results)')
    
    args = parser.parse_args()
    
    config = BenchmarkConfig()
    config.bootstrap_servers = args.bootstrap_servers.split(',')
    config.num_messages = args.num_messages
    config.message_size = args.message_size
    config.num_partitions = args.num_partitions
    config.compression_type = args.compression
    config.results_dir = args.results_dir
    
    benchmark = ChronikBenchmark(config)
    
    try:
        benchmark.run_all_benchmarks()
    except KeyboardInterrupt:
        print("\nBenchmark interrupted by user")
        benchmark.cleanup()
        sys.exit(1)
    except Exception as e:
        print(f"\nBenchmark failed: {e}")
        import traceback
        traceback.print_exc()
        benchmark.cleanup()
        sys.exit(1)


if __name__ == "__main__":
    main()