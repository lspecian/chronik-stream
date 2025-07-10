#!/usr/bin/env python3
"""
Generate a Kafka client compatibility report for Chronik Stream.
"""

import json
import datetime
import subprocess
import os
from typing import Dict, List, Tuple

class CompatibilityTester:
    def __init__(self, bootstrap_servers="localhost:9092"):
        self.bootstrap_servers = bootstrap_servers
        self.results = {
            "timestamp": datetime.datetime.now().isoformat(),
            "bootstrap_servers": bootstrap_servers,
            "clients": {},
            "operations": {}
        }
    
    def check_client_available(self, command: str) -> Tuple[bool, str]:
        """Check if a client command is available and get its version."""
        try:
            # Check if command exists
            result = subprocess.run(["which", command], capture_output=True)
            if result.returncode != 0:
                return False, "Not installed"
            
            # Try to get version
            version_cmds = [
                [command, "--version"],
                [command, "version"],
                [command, "-V"],
            ]
            
            for cmd in version_cmds:
                try:
                    result = subprocess.run(cmd, capture_output=True, text=True, timeout=5)
                    if result.returncode == 0:
                        version = result.stdout.strip() or result.stderr.strip()
                        return True, version.split('\n')[0]
                except:
                    continue
            
            return True, "Unknown version"
        except Exception as e:
            return False, str(e)
    
    def test_operation(self, client: str, operation: str, test_func) -> Dict:
        """Test a specific operation with a client."""
        try:
            success, details = test_func()
            return {
                "client": client,
                "operation": operation,
                "success": success,
                "details": details,
                "error": None
            }
        except Exception as e:
            return {
                "client": client,
                "operation": operation,
                "success": False,
                "details": None,
                "error": str(e)
            }
    
    def test_kafkactl(self) -> Dict:
        """Test kafkactl client."""
        available, version = self.check_client_available("kafkactl")
        
        results = {
            "name": "kafkactl",
            "available": available,
            "version": version,
            "operations": {}
        }
        
        if not available:
            return results
        
        # Test operations
        operations = {
            "list_brokers": lambda: self._run_kafkactl(["get", "brokers"]),
            "create_topic": lambda: self._run_kafkactl(["create", "topic", "test-kafkactl", "--partitions", "1"]),
            "list_topics": lambda: self._run_kafkactl(["get", "topics"]),
            "produce": lambda: self._run_kafkactl_produce(),
            "consume": lambda: self._run_kafkactl_consume(),
            "describe_topic": lambda: self._run_kafkactl(["describe", "topic", "test-kafkactl"]),
            "consumer_groups": lambda: self._run_kafkactl(["get", "consumer-groups"]),
        }
        
        for op_name, op_func in operations.items():
            result = self.test_operation("kafkactl", op_name, op_func)
            results["operations"][op_name] = result
        
        return results
    
    def _run_kafkactl(self, args: List[str]) -> Tuple[bool, str]:
        """Run a kafkactl command."""
        cmd = ["kafkactl"] + args + ["--brokers", self.bootstrap_servers]
        result = subprocess.run(cmd, capture_output=True, text=True)
        return result.returncode == 0, result.stdout or result.stderr
    
    def _run_kafkactl_produce(self) -> Tuple[bool, str]:
        """Test kafkactl produce."""
        cmd = ["kafkactl", "produce", "test-kafkactl", "--value", "test-message", "--brokers", self.bootstrap_servers]
        result = subprocess.run(cmd, capture_output=True, text=True)
        return result.returncode == 0, "Produced message"
    
    def _run_kafkactl_consume(self) -> Tuple[bool, str]:
        """Test kafkactl consume."""
        cmd = ["kafkactl", "consume", "test-kafkactl", "--from-beginning", "--max-messages", "1", "--brokers", self.bootstrap_servers]
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=5)
        success = result.returncode == 0 and "test-message" in result.stdout
        return success, result.stdout
    
    def test_python_client(self) -> Dict:
        """Test confluent-kafka-python."""
        try:
            import confluent_kafka
            available = True
            version = confluent_kafka.version()[0]
        except ImportError:
            available = False
            version = "Not installed"
        
        results = {
            "name": "confluent-kafka-python",
            "available": available,
            "version": version,
            "operations": {}
        }
        
        if not available:
            return results
        
        # Test operations
        operations = {
            "admin_metadata": lambda: self._test_python_metadata(),
            "create_topic": lambda: self._test_python_create_topic(),
            "produce": lambda: self._test_python_produce(),
            "consume": lambda: self._test_python_consume(),
            "consumer_group": lambda: self._test_python_consumer_group(),
        }
        
        for op_name, op_func in operations.items():
            result = self.test_operation("confluent-kafka-python", op_name, op_func)
            results["operations"][op_name] = result
        
        return results
    
    def _test_python_metadata(self) -> Tuple[bool, str]:
        """Test Python client metadata."""
        from confluent_kafka.admin import AdminClient
        admin = AdminClient({'bootstrap.servers': self.bootstrap_servers})
        metadata = admin.list_topics(timeout=5)
        return True, f"Found {len(metadata.brokers)} brokers"
    
    def _test_python_create_topic(self) -> Tuple[bool, str]:
        """Test Python client topic creation."""
        from confluent_kafka.admin import AdminClient, NewTopic
        admin = AdminClient({'bootstrap.servers': self.bootstrap_servers})
        topic = NewTopic('test-python', num_partitions=1, replication_factor=1)
        fs = admin.create_topics([topic])
        
        for topic, f in fs.items():
            try:
                f.result()
                return True, "Topic created"
            except Exception as e:
                if 'already exists' in str(e):
                    return True, "Topic already exists"
                raise
    
    def _test_python_produce(self) -> Tuple[bool, str]:
        """Test Python client produce."""
        from confluent_kafka import Producer
        
        producer = Producer({'bootstrap.servers': self.bootstrap_servers})
        delivered = []
        
        def delivery_report(err, msg):
            if err:
                raise Exception(f"Delivery failed: {err}")
            delivered.append(msg)
        
        producer.produce('test-python', value=b'test-message', callback=delivery_report)
        producer.flush(timeout=5)
        
        return len(delivered) > 0, f"Delivered {len(delivered)} messages"
    
    def _test_python_consume(self) -> Tuple[bool, str]:
        """Test Python client consume."""
        from confluent_kafka import Consumer, KafkaError
        
        consumer = Consumer({
            'bootstrap.servers': self.bootstrap_servers,
            'group.id': 'test-group',
            'auto.offset.reset': 'earliest'
        })
        
        consumer.subscribe(['test-python'])
        msg = consumer.poll(timeout=5.0)
        consumer.close()
        
        if msg and not msg.error():
            return True, f"Consumed: {msg.value()}"
        else:
            return False, "No message consumed"
    
    def _test_python_consumer_group(self) -> Tuple[bool, str]:
        """Test Python client consumer group."""
        from confluent_kafka.admin import AdminClient
        admin = AdminClient({'bootstrap.servers': self.bootstrap_servers})
        
        # List consumer groups (if supported)
        try:
            # This might not be available in all versions
            groups = admin.list_consumer_groups(timeout=5)
            return True, f"Consumer groups listed"
        except:
            return True, "Consumer group operations not fully supported"
    
    def generate_report(self) -> str:
        """Generate compatibility report."""
        # Test all clients
        self.results["clients"]["kafkactl"] = self.test_kafkactl()
        self.results["clients"]["confluent-kafka-python"] = self.test_python_client()
        
        # Add more clients here as needed
        
        # Generate operation matrix
        all_operations = set()
        for client_data in self.results["clients"].values():
            if "operations" in client_data:
                all_operations.update(client_data["operations"].keys())
        
        self.results["operations"] = {}
        for op in sorted(all_operations):
            self.results["operations"][op] = {}
            for client_name, client_data in self.results["clients"].items():
                if "operations" in client_data and op in client_data["operations"]:
                    op_result = client_data["operations"][op]
                    self.results["operations"][op][client_name] = op_result["success"]
                else:
                    self.results["operations"][op][client_name] = None
        
        return json.dumps(self.results, indent=2)
    
    def generate_markdown_report(self) -> str:
        """Generate a markdown compatibility report."""
        lines = [
            "# Chronik Stream - Kafka Client Compatibility Report",
            "",
            f"Generated: {self.results['timestamp']}",
            f"Bootstrap Servers: {self.results['bootstrap_servers']}",
            "",
            "## Client Availability",
            "",
            "| Client | Available | Version |",
            "|--------|-----------|---------|"
        ]
        
        for client_name, client_data in self.results["clients"].items():
            available = "✅" if client_data["available"] else "❌"
            version = client_data["version"]
            lines.append(f"| {client_name} | {available} | {version} |")
        
        lines.extend([
            "",
            "## Operation Compatibility Matrix",
            "",
            "| Operation | " + " | ".join(self.results["clients"].keys()) + " |",
            "|-----------|" + "|".join(["---"] * len(self.results["clients"])) + "|"
        ])
        
        for op_name, op_results in self.results["operations"].items():
            row = [op_name.replace("_", " ").title()]
            for client_name in self.results["clients"].keys():
                if client_name in op_results:
                    result = op_results[client_name]
                    if result is None:
                        row.append("N/A")
                    elif result:
                        row.append("✅")
                    else:
                        row.append("❌")
                else:
                    row.append("N/A")
            lines.append("| " + " | ".join(row) + " |")
        
        lines.extend([
            "",
            "## Detailed Results",
            ""
        ])
        
        for client_name, client_data in self.results["clients"].items():
            lines.append(f"### {client_name}")
            lines.append("")
            
            if not client_data["available"]:
                lines.append(f"Client not available: {client_data['version']}")
                lines.append("")
                continue
            
            if "operations" in client_data:
                for op_name, op_result in client_data["operations"].items():
                    status = "✅" if op_result["success"] else "❌"
                    lines.append(f"- **{op_name.replace('_', ' ').title()}**: {status}")
                    if op_result["error"]:
                        lines.append(f"  - Error: {op_result['error']}")
                    elif op_result["details"]:
                        lines.append(f"  - Details: {op_result['details'][:100]}...")
            
            lines.append("")
        
        return "\n".join(lines)

def main():
    """Generate compatibility report."""
    import argparse
    
    parser = argparse.ArgumentParser(description="Generate Kafka client compatibility report")
    parser.add_argument("--bootstrap-servers", default="localhost:9092", help="Kafka bootstrap servers")
    parser.add_argument("--format", choices=["json", "markdown", "both"], default="both", help="Output format")
    parser.add_argument("--output", help="Output file (default: stdout)")
    
    args = parser.parse_args()
    
    print("🔍 Testing Kafka client compatibility with Chronik Stream...")
    
    tester = CompatibilityTester(args.bootstrap_servers)
    
    # Generate reports
    json_report = tester.generate_report()
    md_report = tester.generate_markdown_report()
    
    # Output results
    if args.output:
        if args.format in ["json", "both"]:
            with open(f"{args.output}.json", "w") as f:
                f.write(json_report)
            print(f"✅ JSON report saved to {args.output}.json")
        
        if args.format in ["markdown", "both"]:
            with open(f"{args.output}.md", "w") as f:
                f.write(md_report)
            print(f"✅ Markdown report saved to {args.output}.md")
    else:
        if args.format == "json":
            print(json_report)
        elif args.format == "markdown":
            print(md_report)
        else:
            print(md_report)
            print("\n" + "="*80 + "\n")
            print("JSON Report:")
            print(json_report)

if __name__ == "__main__":
    main()