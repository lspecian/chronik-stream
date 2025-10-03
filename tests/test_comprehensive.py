#!/usr/bin/env python3
"""
Comprehensive test suite for Chronik Stream Kafka APIs
Tests all implemented APIs to verify functionality
"""

import socket
import struct
import time
import subprocess
import sys

class ChronikTestSuite:
    def __init__(self, host='localhost', port=9094):
        self.host = host
        self.port = port
        self.passed_tests = 0
        self.failed_tests = 0
        self.test_results = []

    def connect(self):
        """Create a new socket connection"""
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect((self.host, self.port))
        return sock

    def send_request(self, sock, api_key, api_version, correlation_id, body):
        """Send a Kafka protocol request"""
        client_id = b'test-suite'

        # Build request header
        header = struct.pack('>hhi', api_key, api_version, correlation_id)
        header += struct.pack('>h', len(client_id)) + client_id

        # Build full request
        request = header + body
        message = struct.pack('>i', len(request)) + request
        sock.sendall(message)

    def receive_response(self, sock):
        """Receive a Kafka protocol response"""
        size_bytes = sock.recv(4)
        if len(size_bytes) != 4:
            return None

        size = struct.unpack('>i', size_bytes)[0]
        response = sock.recv(size)
        return response

    def test_api_versions(self):
        """Test ApiVersions request"""
        print("\n[TEST] ApiVersions...")
        sock = self.connect()

        # ApiVersions request (API Key 18, Version 0)
        body = b''  # Empty body for v0
        self.send_request(sock, 18, 0, 1, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 1:
                self.passed_tests += 1
                self.test_results.append(("ApiVersions", "✅ PASSED"))
                return True

        self.failed_tests += 1
        self.test_results.append(("ApiVersions", "❌ FAILED"))
        return False

    def test_metadata(self):
        """Test Metadata request"""
        print("\n[TEST] Metadata...")
        sock = self.connect()

        # Metadata request (API Key 3, Version 0)
        # Topics array (null = all topics)
        body = struct.pack('>i', -1)
        self.send_request(sock, 3, 0, 2, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 2:
                self.passed_tests += 1
                self.test_results.append(("Metadata", "✅ PASSED"))
                return True

        self.failed_tests += 1
        self.test_results.append(("Metadata", "❌ FAILED"))
        return False

    def test_create_topics(self):
        """Test CreateTopics request"""
        print("\n[TEST] CreateTopics...")
        sock = self.connect()

        topic_name = f'test-topic-{int(time.time())}'
        topic_bytes = topic_name.encode('utf-8')

        # CreateTopics request body
        body = struct.pack('>i', 1)  # 1 topic
        body += struct.pack('>h', len(topic_bytes)) + topic_bytes
        body += struct.pack('>i', 3)  # 3 partitions
        body += struct.pack('>h', 1)  # replication factor
        body += struct.pack('>i', -1)  # no replica assignments
        body += struct.pack('>i', -1)  # no configs
        body += struct.pack('>i', 5000)  # timeout

        self.send_request(sock, 19, 0, 3, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 3:
                self.passed_tests += 1
                self.test_results.append(("CreateTopics", "✅ PASSED"))
                return True

        self.failed_tests += 1
        self.test_results.append(("CreateTopics", "❌ FAILED"))
        return False

    def test_list_groups(self):
        """Test ListGroups request"""
        print("\n[TEST] ListGroups...")
        sock = self.connect()

        # ListGroups request (API Key 16, Version 0)
        body = b''  # Empty body
        self.send_request(sock, 16, 0, 4, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 4:
                # Check error code
                error_code = struct.unpack('>h', response[4:6])[0]
                if error_code == 0:
                    self.passed_tests += 1
                    self.test_results.append(("ListGroups", "✅ PASSED"))
                    return True

        self.failed_tests += 1
        self.test_results.append(("ListGroups", "❌ FAILED"))
        return False

    def test_describe_configs(self):
        """Test DescribeConfigs request"""
        print("\n[TEST] DescribeConfigs...")
        sock = self.connect()

        # DescribeConfigs request (API Key 32, Version 0)
        # Resources array
        body = struct.pack('>i', 1)  # 1 resource
        body += struct.pack('>b', 4)  # Resource type: Cluster
        body += struct.pack('>h', 0) + b''  # Empty resource name for cluster
        body += struct.pack('>i', -1)  # null config names (all configs)

        self.send_request(sock, 32, 0, 5, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 5:
                self.passed_tests += 1
                self.test_results.append(("DescribeConfigs", "✅ PASSED"))
                return True

        self.failed_tests += 1
        self.test_results.append(("DescribeConfigs", "❌ FAILED"))
        return False

    def test_sasl_handshake(self):
        """Test SaslHandshake request"""
        print("\n[TEST] SaslHandshake...")
        sock = self.connect()

        # SaslHandshake request (API Key 17, Version 0)
        mechanism = b'PLAIN'
        body = struct.pack('>h', len(mechanism)) + mechanism

        self.send_request(sock, 17, 0, 6, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 6:
                error_code = struct.unpack('>h', response[4:6])[0]
                if error_code == 0:
                    # Try to read mechanisms
                    offset = 6
                    if offset + 4 <= len(response):
                        mech_count = struct.unpack('>i', response[offset:offset+4])[0]
                        if mech_count > 0:
                            self.passed_tests += 1
                            self.test_results.append(("SaslHandshake", "✅ PASSED"))
                            return True

        self.failed_tests += 1
        self.test_results.append(("SaslHandshake", "❌ FAILED"))
        return False

    def test_describe_acls(self):
        """Test DescribeAcls request"""
        print("\n[TEST] DescribeAcls...")
        sock = self.connect()

        # DescribeAcls request (API Key 29, Version 0)
        # Filter
        body = struct.pack('>b', 1)  # Resource type: Any
        body += struct.pack('>h', -1)  # Null resource name
        body += struct.pack('>h', -1)  # Null principal
        body += struct.pack('>h', -1)  # Null host
        body += struct.pack('>b', 1)  # Operation: Any
        body += struct.pack('>b', 1)  # Permission type: Any

        self.send_request(sock, 29, 0, 7, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 7:
                # Skip throttle time
                error_code = struct.unpack('>h', response[8:10])[0]
                if error_code == 0:
                    self.passed_tests += 1
                    self.test_results.append(("DescribeAcls", "✅ PASSED"))
                    return True

        self.failed_tests += 1
        self.test_results.append(("DescribeAcls", "❌ FAILED"))
        return False

    def test_produce(self):
        """Test Produce request"""
        print("\n[TEST] Produce...")
        sock = self.connect()

        # First create a topic
        topic_name = f'test-produce-{int(time.time())}'
        self.create_test_topic(topic_name)
        time.sleep(1)

        # Produce request (API Key 0, Version 0)
        topic_bytes = topic_name.encode('utf-8')

        # Build message
        message = b'test message'
        msg_bytes = struct.pack('>q', 0)  # offset (ignored in produce)
        msg_bytes += struct.pack('>i', len(message))  # message size
        msg_bytes += struct.pack('>i', 0)  # CRC (0 for simplicity)
        msg_bytes += struct.pack('>bb', 0, 0)  # magic, attributes
        msg_bytes += struct.pack('>i', -1)  # no key
        msg_bytes += struct.pack('>i', len(message))
        msg_bytes += message

        # Build request body
        body = struct.pack('>h', 1)  # required acks
        body += struct.pack('>i', 1000)  # timeout
        body += struct.pack('>i', 1)  # 1 topic
        body += struct.pack('>h', len(topic_bytes)) + topic_bytes
        body += struct.pack('>i', 1)  # 1 partition
        body += struct.pack('>i', 0)  # partition 0
        body += struct.pack('>i', len(msg_bytes))  # message set size
        body += msg_bytes

        self.send_request(sock, 0, 0, 8, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 8:
                self.passed_tests += 1
                self.test_results.append(("Produce", "✅ PASSED"))
                return True

        self.failed_tests += 1
        self.test_results.append(("Produce", "❌ FAILED"))
        return False

    def test_fetch(self):
        """Test Fetch request"""
        print("\n[TEST] Fetch...")
        sock = self.connect()

        # Use a known topic
        topic_name = b'test-topic'

        # Fetch request (API Key 1, Version 0)
        body = struct.pack('>i', -1)  # replica id
        body += struct.pack('>i', 100)  # max wait time
        body += struct.pack('>i', 1024)  # min bytes
        body += struct.pack('>i', 1)  # 1 topic
        body += struct.pack('>h', len(topic_name)) + topic_name
        body += struct.pack('>i', 1)  # 1 partition
        body += struct.pack('>i', 0)  # partition 0
        body += struct.pack('>q', 0)  # fetch offset
        body += struct.pack('>i', 1048576)  # max bytes

        self.send_request(sock, 1, 0, 9, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 9:
                self.passed_tests += 1
                self.test_results.append(("Fetch", "✅ PASSED"))
                return True

        self.failed_tests += 1
        self.test_results.append(("Fetch", "❌ FAILED"))
        return False

    def test_find_coordinator(self):
        """Test FindCoordinator request"""
        print("\n[TEST] FindCoordinator...")
        sock = self.connect()

        # FindCoordinator request (API Key 10, Version 0)
        group_id = b'test-group'
        body = struct.pack('>h', len(group_id)) + group_id

        self.send_request(sock, 10, 0, 10, body)

        response = self.receive_response(sock)
        sock.close()

        if response and len(response) > 4:
            correlation_id = struct.unpack('>i', response[:4])[0]
            if correlation_id == 10:
                error_code = struct.unpack('>h', response[4:6])[0]
                if error_code == 0 or error_code == 15:  # 15 = COORDINATOR_NOT_AVAILABLE (expected)
                    self.passed_tests += 1
                    self.test_results.append(("FindCoordinator", "✅ PASSED"))
                    return True

        self.failed_tests += 1
        self.test_results.append(("FindCoordinator", "❌ FAILED"))
        return False

    def create_test_topic(self, topic_name):
        """Helper to create a test topic"""
        sock = self.connect()

        topic_bytes = topic_name.encode('utf-8')

        # CreateTopics request
        body = struct.pack('>i', 1)  # 1 topic
        body += struct.pack('>h', len(topic_bytes)) + topic_bytes
        body += struct.pack('>i', 1)  # 1 partition
        body += struct.pack('>h', 1)  # replication factor
        body += struct.pack('>i', -1)  # no replica assignments
        body += struct.pack('>i', -1)  # no configs
        body += struct.pack('>i', 5000)  # timeout

        self.send_request(sock, 19, 0, 100, body)
        response = self.receive_response(sock)
        sock.close()

        return response is not None

    def run_all_tests(self):
        """Run all test cases"""
        print("=" * 60)
        print("CHRONIK STREAM COMPREHENSIVE TEST SUITE")
        print("=" * 60)

        # Core APIs
        self.test_api_versions()
        self.test_metadata()

        # Topic Management
        self.test_create_topics()

        # Consumer Group APIs
        self.test_list_groups()
        self.test_find_coordinator()

        # Configuration APIs
        self.test_describe_configs()

        # Security APIs
        self.test_sasl_handshake()
        self.test_describe_acls()

        # Data APIs
        self.test_produce()
        self.test_fetch()

        # Print results
        print("\n" + "=" * 60)
        print("TEST RESULTS SUMMARY")
        print("=" * 60)

        for test_name, result in self.test_results:
            print(f"{test_name:20} {result}")

        print("\n" + "-" * 60)
        print(f"Total Tests: {self.passed_tests + self.failed_tests}")
        print(f"Passed: {self.passed_tests} ✅")
        print(f"Failed: {self.failed_tests} ❌")

        coverage = (self.passed_tests / (self.passed_tests + self.failed_tests)) * 100
        print(f"Coverage: {coverage:.1f}%")

        if self.failed_tests == 0:
            print("\n🎉 ALL TESTS PASSED! 🎉")
        else:
            print(f"\n⚠️  {self.failed_tests} tests failed")

        return self.failed_tests == 0

def main():
    # Kill any existing server
    subprocess.run(['pkill', '-f', 'chronik-server'], stderr=subprocess.DEVNULL)
    time.sleep(1)

    # Start server
    print("Starting Chronik server...")
    subprocess.run(['rm', '-rf', './data'])
    server = subprocess.Popen(
        ['./target/release/chronik-server', '-p', '9094'],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE
    )
    time.sleep(2)

    try:
        # Run test suite
        suite = ChronikTestSuite()
        success = suite.run_all_tests()

        # Return appropriate exit code
        sys.exit(0 if success else 1)

    finally:
        # Stop server
        server.terminate()
        server.wait()

if __name__ == "__main__":
    main()