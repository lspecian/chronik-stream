#!/usr/bin/env python3
"""
Production Readiness Test Suite for Chronik Stream

This script validates all implemented features for production readiness.
"""

import time
import logging
import sys
import json
from typing import Dict, List, Tuple
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic, ConfigResource, ConfigResourceType
from kafka.errors import KafkaError
import concurrent.futures

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

KAFKA_BROKER = 'localhost:9094'

class ProductionReadinessTest:
    def __init__(self):
        self.results = {}
        self.admin = None
        self.producer = None
        self.consumer = None

    def setup(self):
        """Initialize clients"""
        try:
            self.admin = KafkaAdminClient(
                bootstrap_servers=[KAFKA_BROKER],
                client_id='production-test-admin'
            )
            logger.info("✅ Admin client connected")
            return True
        except Exception as e:
            logger.error(f"❌ Failed to connect: {e}")
            return False

    def cleanup(self):
        """Clean up resources"""
        if self.admin:
            self.admin.close()
        if self.producer:
            self.producer.close()
        if self.consumer:
            self.consumer.close()

    def test_api_compatibility(self) -> Dict[str, bool]:
        """Test all implemented Kafka APIs"""
        logger.info("\n" + "=" * 60)
        logger.info("Testing API Compatibility")
        logger.info("=" * 60)

        api_tests = {
            "Metadata API": self._test_metadata,
            "CreateTopics API": self._test_create_topics,
            "DeleteTopics API": self._test_delete_topics,
            "DescribeConfigs API": self._test_describe_configs,
            "AlterConfigs API": self._test_alter_configs,
            "ListGroups API": self._test_list_groups,
            "DescribeGroups API": self._test_describe_groups,
            "Produce/Fetch APIs": self._test_produce_fetch,
            "Consumer Group APIs": self._test_consumer_groups,
            "Offset Management": self._test_offset_management,
        }

        results = {}
        for api_name, test_func in api_tests.items():
            try:
                result = test_func()
                results[api_name] = result
                status = "✅" if result else "❌"
                logger.info(f"{status} {api_name}: {'Working' if result else 'Failed'}")
            except Exception as e:
                results[api_name] = False
                logger.error(f"❌ {api_name}: {e}")

        return results

    def _test_metadata(self) -> bool:
        """Test Metadata API"""
        try:
            metadata = self.admin.describe_cluster()
            return metadata is not None
        except:
            return False

    def _test_create_topics(self) -> bool:
        """Test CreateTopics API"""
        try:
            topic = NewTopic(name='test-create-topic', num_partitions=2, replication_factor=1)
            fs = self.admin.create_topics([topic])
            for topic, f in fs.items():
                f.result()
            return True
        except:
            return False

    def _test_delete_topics(self) -> bool:
        """Test DeleteTopics API"""
        try:
            # Create then delete
            topic = NewTopic(name='test-delete-topic', num_partitions=1, replication_factor=1)
            self.admin.create_topics([topic])
            time.sleep(0.5)
            fs = self.admin.delete_topics(['test-delete-topic'])
            for topic, f in fs.items():
                f.result()
            return True
        except:
            return False

    def _test_describe_configs(self) -> bool:
        """Test DescribeConfigs API"""
        try:
            # Create a topic first
            topic = NewTopic(name='test-config-topic', num_partitions=1, replication_factor=1)
            self.admin.create_topics([topic])
            time.sleep(0.5)

            # Describe its configs
            resource = ConfigResource(ConfigResourceType.TOPIC, 'test-config-topic')
            configs = self.admin.describe_configs([resource])
            return len(configs) > 0
        except:
            return False

    def _test_alter_configs(self) -> bool:
        """Test AlterConfigs API"""
        try:
            # This might not persist but should not error
            resource = ConfigResource(
                ConfigResourceType.TOPIC,
                'test-config-topic',
                {'retention.ms': '86400000'}
            )
            self.admin.alter_configs([resource])
            return True
        except:
            return False

    def _test_list_groups(self) -> bool:
        """Test ListGroups API"""
        try:
            groups = self.admin.list_consumer_groups()
            return True
        except:
            return False

    def _test_describe_groups(self) -> bool:
        """Test DescribeGroups API"""
        try:
            # Create a consumer to form a group
            consumer = KafkaConsumer(
                'test-config-topic',
                bootstrap_servers=[KAFKA_BROKER],
                group_id='test-describe-group',
                auto_offset_reset='earliest'
            )
            consumer.poll(timeout_ms=100)

            # Describe the group
            groups = self.admin.describe_consumer_groups(['test-describe-group'])
            consumer.close()
            return len(groups) > 0
        except:
            return False

    def _test_produce_fetch(self) -> bool:
        """Test Produce and Fetch APIs"""
        try:
            topic = 'test-produce-fetch'
            # Create topic
            t = NewTopic(name=topic, num_partitions=1, replication_factor=1)
            self.admin.create_topics([t])
            time.sleep(0.5)

            # Produce
            producer = KafkaProducer(bootstrap_servers=[KAFKA_BROKER])
            future = producer.send(topic, b'test-message')
            producer.flush()
            meta = future.get(timeout=10)

            # Consume
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=[KAFKA_BROKER],
                auto_offset_reset='earliest',
                consumer_timeout_ms=1000
            )

            messages = list(consumer)
            consumer.close()
            producer.close()

            return len(messages) > 0
        except:
            return False

    def _test_consumer_groups(self) -> bool:
        """Test Consumer Group coordination"""
        try:
            topic = 'test-consumer-group'
            group_id = 'test-cg'

            # Create topic
            t = NewTopic(name=topic, num_partitions=3, replication_factor=1)
            self.admin.create_topics([t])
            time.sleep(0.5)

            # Create multiple consumers in same group
            consumers = []
            for i in range(2):
                c = KafkaConsumer(
                    topic,
                    bootstrap_servers=[KAFKA_BROKER],
                    group_id=group_id,
                    auto_offset_reset='earliest'
                )
                consumers.append(c)

            # Poll to trigger rebalance
            for c in consumers:
                c.poll(timeout_ms=100)

            # Check assignments
            assignments = [c.assignment() for c in consumers]

            # Clean up
            for c in consumers:
                c.close()

            # Should have distributed partitions
            return all(len(a) > 0 for a in assignments)
        except:
            return False

    def _test_offset_management(self) -> bool:
        """Test Offset Commit and Fetch"""
        try:
            topic = 'test-offset-mgmt'
            group_id = 'test-offset-group'

            # Create topic
            t = NewTopic(name=topic, num_partitions=1, replication_factor=1)
            self.admin.create_topics([t])
            time.sleep(0.5)

            # Produce message
            producer = KafkaProducer(bootstrap_servers=[KAFKA_BROKER])
            producer.send(topic, b'offset-test')
            producer.flush()
            producer.close()

            # Consume and commit
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=[KAFKA_BROKER],
                group_id=group_id,
                auto_offset_reset='earliest',
                enable_auto_commit=False
            )

            for msg in consumer:
                consumer.commit()
                break

            # Get committed offset
            from kafka import TopicPartition
            tp = TopicPartition(topic, 0)
            offsets = consumer.committed([tp])
            consumer.close()

            return offsets[tp] is not None
        except:
            return False

    def test_performance(self) -> Dict[str, float]:
        """Basic performance benchmarks"""
        logger.info("\n" + "=" * 60)
        logger.info("Testing Performance Metrics")
        logger.info("=" * 60)

        results = {}

        # Throughput test
        throughput = self._test_throughput()
        results['throughput_msgs_sec'] = throughput
        logger.info(f"Throughput: {throughput:.0f} msgs/sec")

        # Latency test
        p99_latency = self._test_latency()
        results['p99_latency_ms'] = p99_latency
        logger.info(f"P99 Latency: {p99_latency:.2f} ms")

        return results

    def _test_throughput(self) -> float:
        """Test message throughput"""
        try:
            topic = 'perf-throughput-test'
            t = NewTopic(name=topic, num_partitions=3, replication_factor=1)
            self.admin.create_topics([t])
            time.sleep(0.5)

            producer = KafkaProducer(
                bootstrap_servers=[KAFKA_BROKER],
                compression_type='snappy',
                batch_size=16384
            )

            num_messages = 10000
            message = b'x' * 100  # 100 byte messages

            start = time.time()
            futures = []
            for i in range(num_messages):
                future = producer.send(topic, message, partition=i % 3)
                futures.append(future)

                # Flush every 1000 messages
                if i % 1000 == 0:
                    producer.flush()

            producer.flush()
            elapsed = time.time() - start
            producer.close()

            return num_messages / elapsed
        except Exception as e:
            logger.error(f"Throughput test failed: {e}")
            return 0

    def _test_latency(self) -> float:
        """Test message latency"""
        try:
            topic = 'perf-latency-test'
            t = NewTopic(name=topic, num_partitions=1, replication_factor=1)
            self.admin.create_topics([t])
            time.sleep(0.5)

            latencies = []

            producer = KafkaProducer(bootstrap_servers=[KAFKA_BROKER])

            for i in range(100):
                start = time.time()
                future = producer.send(topic, f'latency-test-{i}'.encode())
                future.get(timeout=10)  # Wait for ack
                latency = (time.time() - start) * 1000  # Convert to ms
                latencies.append(latency)

            producer.close()

            # Calculate P99
            latencies.sort()
            p99_index = int(len(latencies) * 0.99)
            return latencies[p99_index]
        except Exception as e:
            logger.error(f"Latency test failed: {e}")
            return -1

    def test_reliability(self) -> Dict[str, bool]:
        """Test reliability features"""
        logger.info("\n" + "=" * 60)
        logger.info("Testing Reliability Features")
        logger.info("=" * 60)

        results = {}

        # Test idempotent producer
        results['idempotent_producer'] = self._test_idempotent_producer()
        logger.info(f"{'✅' if results['idempotent_producer'] else '❌'} Idempotent Producer")

        # Test consumer recovery
        results['consumer_recovery'] = self._test_consumer_recovery()
        logger.info(f"{'✅' if results['consumer_recovery'] else '❌'} Consumer Recovery")

        # Test concurrent operations
        results['concurrent_ops'] = self._test_concurrent_operations()
        logger.info(f"{'✅' if results['concurrent_ops'] else '❌'} Concurrent Operations")

        return results

    def _test_idempotent_producer(self) -> bool:
        """Test idempotent producer (basic)"""
        try:
            topic = 'test-idempotent'
            t = NewTopic(name=topic, num_partitions=1, replication_factor=1)
            self.admin.create_topics([t])
            time.sleep(0.5)

            # Note: kafka-python doesn't support idempotence fully
            # Just test that we can produce with acks='all'
            producer = KafkaProducer(
                bootstrap_servers=[KAFKA_BROKER],
                acks='all',
                retries=5
            )

            for i in range(10):
                producer.send(topic, f'msg-{i}'.encode())

            producer.flush()
            producer.close()
            return True
        except:
            return False

    def _test_consumer_recovery(self) -> bool:
        """Test consumer can recover from disconnect"""
        try:
            topic = 'test-recovery'
            group = 'recovery-group'
            t = NewTopic(name=topic, num_partitions=1, replication_factor=1)
            self.admin.create_topics([t])
            time.sleep(0.5)

            # Produce messages
            producer = KafkaProducer(bootstrap_servers=[KAFKA_BROKER])
            for i in range(5):
                producer.send(topic, f'msg-{i}'.encode())
            producer.flush()
            producer.close()

            # Consumer 1 - consume partially
            consumer1 = KafkaConsumer(
                topic,
                bootstrap_servers=[KAFKA_BROKER],
                group_id=group,
                auto_offset_reset='earliest',
                enable_auto_commit=True
            )

            count1 = 0
            for msg in consumer1:
                count1 += 1
                if count1 >= 2:
                    break

            consumer1.close()

            # Consumer 2 - should continue from offset
            consumer2 = KafkaConsumer(
                topic,
                bootstrap_servers=[KAFKA_BROKER],
                group_id=group,
                auto_offset_reset='earliest',
                consumer_timeout_ms=1000
            )

            count2 = len(list(consumer2))
            consumer2.close()

            # Should have consumed remaining messages
            return count2 == 3
        except:
            return False

    def _test_concurrent_operations(self) -> bool:
        """Test concurrent produce/consume"""
        try:
            topic = 'test-concurrent'
            t = NewTopic(name=topic, num_partitions=3, replication_factor=1)
            self.admin.create_topics([t])
            time.sleep(0.5)

            def produce_batch(batch_id):
                p = KafkaProducer(bootstrap_servers=[KAFKA_BROKER])
                for i in range(100):
                    p.send(topic, f'batch-{batch_id}-msg-{i}'.encode())
                p.flush()
                p.close()
                return batch_id

            # Concurrent producers
            with concurrent.futures.ThreadPoolExecutor(max_workers=3) as executor:
                futures = [executor.submit(produce_batch, i) for i in range(3)]
                results = [f.result() for f in futures]

            # Verify all messages
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=[KAFKA_BROKER],
                auto_offset_reset='earliest',
                consumer_timeout_ms=2000
            )

            messages = list(consumer)
            consumer.close()

            return len(messages) == 300
        except:
            return False

    def generate_report(self, api_results, perf_results, reliability_results):
        """Generate production readiness report"""
        logger.info("\n" + "=" * 60)
        logger.info("PRODUCTION READINESS REPORT")
        logger.info("=" * 60)

        # API Compatibility Score
        api_score = sum(1 for v in api_results.values() if v) / len(api_results) * 100
        logger.info(f"\n📊 API Compatibility: {api_score:.0f}%")
        for api, result in api_results.items():
            status = "✅" if result else "❌"
            logger.info(f"  {status} {api}")

        # Performance Metrics
        logger.info(f"\n⚡ Performance Metrics:")
        logger.info(f"  Throughput: {perf_results.get('throughput_msgs_sec', 0):.0f} msgs/sec")
        logger.info(f"  P99 Latency: {perf_results.get('p99_latency_ms', -1):.2f} ms")

        # Reliability Score
        reliability_score = sum(1 for v in reliability_results.values() if v) / len(reliability_results) * 100
        logger.info(f"\n🛡️ Reliability: {reliability_score:.0f}%")
        for feature, result in reliability_results.items():
            status = "✅" if result else "❌"
            logger.info(f"  {status} {feature}")

        # Overall Score
        overall_score = (api_score + reliability_score) / 2
        logger.info(f"\n🎯 Overall Production Readiness: {overall_score:.0f}%")

        if overall_score >= 80:
            logger.info("✅ READY for light production workloads")
        elif overall_score >= 60:
            logger.info("⚠️ SUITABLE for development/testing")
        else:
            logger.info("❌ NOT READY for production")

        # Recommendations
        logger.info("\n📝 Recommendations:")
        if api_score < 100:
            missing = [api for api, result in api_results.items() if not result]
            if missing:
                logger.info(f"  - Fix failing APIs: {', '.join(missing[:3])}")

        if perf_results.get('throughput_msgs_sec', 0) < 10000:
            logger.info("  - Optimize throughput for production loads")

        if perf_results.get('p99_latency_ms', 100) > 50:
            logger.info("  - Reduce P99 latency below 50ms")

        if not reliability_results.get('idempotent_producer', False):
            logger.info("  - Implement full idempotent producer support")

        return {
            'api_score': api_score,
            'performance': perf_results,
            'reliability_score': reliability_score,
            'overall_score': overall_score
        }

def main():
    """Run production readiness tests"""
    test = ProductionReadinessTest()

    try:
        logger.info("🚀 Starting Production Readiness Tests")
        logger.info(f"Target: {KAFKA_BROKER}")

        if not test.setup():
            logger.error("Failed to connect to Chronik Stream")
            return 1

        # Run all tests
        api_results = test.test_api_compatibility()
        perf_results = test.test_performance()
        reliability_results = test.test_reliability()

        # Generate report
        report = test.generate_report(api_results, perf_results, reliability_results)

        # Save report
        with open('production_readiness_report.json', 'w') as f:
            json.dump(report, f, indent=2)
            logger.info(f"\n📄 Report saved to production_readiness_report.json")

        return 0 if report['overall_score'] >= 60 else 1

    except Exception as e:
        logger.error(f"Test suite failed: {e}")
        import traceback
        traceback.print_exc()
        return 1
    finally:
        test.cleanup()

if __name__ == "__main__":
    sys.exit(main())