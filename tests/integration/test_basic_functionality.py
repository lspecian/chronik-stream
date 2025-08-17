#!/usr/bin/env python3
"""
Basic functionality tests for Chronik Stream.
These tests validate the current working state of the system.
"""

import socket
import struct
import time


class TestBasicFunctionality:
    """Test basic Kafka protocol operations"""
    
    def create_connection(self, host="localhost", port=9092):
        """Create a socket connection to Chronik Stream"""
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect((host, port))
        return sock
    
    def send_request(self, sock, request_bytes):
        """Send a request with length prefix"""
        length_prefix = struct.pack(">i", len(request_bytes))
        sock.send(length_prefix + request_bytes)
    
    def receive_response(self, sock):
        """Receive a response with length prefix"""
        length_bytes = sock.recv(4)
        if len(length_bytes) < 4:
            return None
        
        length = struct.unpack(">i", length_bytes)[0]
        response = sock.recv(length)
        return response
    
    def test_connection(self):
        """Test that we can connect to the server"""
        sock = self.create_connection()
        assert sock is not None
        sock.close()
    
    def test_api_versions(self):
        """Test API versions request"""
        sock = self.create_connection()
        
        # Build API versions request
        request = bytearray()
        request.extend(struct.pack(">h", 18))  # ApiVersions
        request.extend(struct.pack(">h", 0))   # Version 0
        request.extend(struct.pack(">i", 1))   # Correlation ID
        request.extend(struct.pack(">h", -1))  # No client ID
        
        self.send_request(sock, request)
        response = self.receive_response(sock)
        
        assert response is not None
        # Check correlation ID
        correlation_id = struct.unpack(">i", response[0:4])[0]
        assert correlation_id == 1
        
        sock.close()
    
    def test_metadata_v0(self):
        """Test metadata request version 0"""
        sock = self.create_connection()
        
        # Build metadata request v0
        request = bytearray()
        request.extend(struct.pack(">h", 3))   # Metadata
        request.extend(struct.pack(">h", 0))   # Version 0
        request.extend(struct.pack(">i", 2))   # Correlation ID
        request.extend(struct.pack(">h", 8))   # Client ID length
        request.extend(b"test-client")         # Client ID
        # v0 has no topics array
        
        self.send_request(sock, request)
        response = self.receive_response(sock)
        
        assert response is not None
        # Check correlation ID
        correlation_id = struct.unpack(">i", response[0:4])[0]
        assert correlation_id == 2
        
        # Parse broker count
        broker_count = struct.unpack(">i", response[4:8])[0]
        assert broker_count >= 1  # Should have at least one broker
        
        sock.close()
    
    def test_metadata_v5(self):
        """Test metadata request version 5"""
        sock = self.create_connection()
        
        # Build metadata request v5
        request = bytearray()
        request.extend(struct.pack(">h", 3))   # Metadata
        request.extend(struct.pack(">h", 5))   # Version 5
        request.extend(struct.pack(">i", 3))   # Correlation ID
        request.extend(struct.pack(">h", 8))   # Client ID length
        request.extend(b"test-client")         # Client ID
        request.extend(struct.pack(">i", -1))  # All topics
        request.extend(struct.pack(">b", 1))   # allow_auto_topic_creation
        
        self.send_request(sock, request)
        response = self.receive_response(sock)
        
        assert response is not None
        # Check correlation ID
        correlation_id = struct.unpack(">i", response[0:4])[0]
        assert correlation_id == 3
        
        sock.close()
    
    def test_produce_basic(self):
        """Test basic produce request (currently doesn't persist)"""
        sock = self.create_connection()
        
        # Build produce request v0
        request = bytearray()
        request.extend(struct.pack(">h", 0))   # Produce
        request.extend(struct.pack(">h", 0))   # Version 0
        request.extend(struct.pack(">i", 4))   # Correlation ID
        request.extend(struct.pack(">h", 8))   # Client ID length
        request.extend(b"producer")            # Client ID
        request.extend(struct.pack(">h", 1))   # Required acks
        request.extend(struct.pack(">i", 1000)) # Timeout
        request.extend(struct.pack(">i", 0))   # 0 topics (empty produce)
        
        self.send_request(sock, request)
        response = self.receive_response(sock)
        
        assert response is not None
        # Check correlation ID
        correlation_id = struct.unpack(">i", response[0:4])[0]
        assert correlation_id == 4
        
        sock.close()
    
    def test_fetch_basic(self):
        """Test basic fetch request"""
        sock = self.create_connection()
        
        # Build fetch request v0
        request = bytearray()
        request.extend(struct.pack(">h", 1))   # Fetch
        request.extend(struct.pack(">h", 0))   # Version 0
        request.extend(struct.pack(">i", 5))   # Correlation ID
        request.extend(struct.pack(">h", 8))   # Client ID length
        request.extend(b"consumer")            # Client ID
        request.extend(struct.pack(">i", -1))  # Replica ID
        request.extend(struct.pack(">i", 100)) # Max wait ms
        request.extend(struct.pack(">i", 1))   # Min bytes
        request.extend(struct.pack(">i", 0))   # 0 topics
        
        self.send_request(sock, request)
        response = self.receive_response(sock)
        
        assert response is not None
        # Check correlation ID
        correlation_id = struct.unpack(">i", response[0:4])[0]
        assert correlation_id == 5
        
        # This would fail because fetch returns empty
        # Check that we got some data back
        assert len(response) > 4
        
        sock.close()
    
    def test_describe_configs_error(self):
        """Test DescribeConfigs error response"""
        sock = self.create_connection()
        
        # Build DescribeConfigs request v0
        request = bytearray()
        request.extend(struct.pack(">h", 32))  # DescribeConfigs
        request.extend(struct.pack(">h", 0))   # Version 0
        request.extend(struct.pack(">i", 6))   # Correlation ID
        request.extend(struct.pack(">h", 8))   # Client ID length
        request.extend(b"kafkactl")            # Client ID
        request.extend(struct.pack(">i", 0))   # 0 resources
        
        self.send_request(sock, request)
        response = self.receive_response(sock)
        
        assert response is not None
        # This fails - returns correlation ID 0 instead of 6
        correlation_id = struct.unpack(">i", response[0:4])[0]
        assert correlation_id == 6
        
        sock.close()


if __name__ == "__main__":
    # Run tests
    test = TestBasicFunctionality()
    
    print("Testing connection...")
    test.test_connection()
    print("✓ Connection successful")
    
    print("\nTesting API versions...")
    test.test_api_versions()
    print("✓ API versions working")
    
    print("\nTesting metadata v0...")
    test.test_metadata_v0()
    print("✓ Metadata v0 working")
    
    print("\nTesting metadata v5...")
    test.test_metadata_v5()
    print("✓ Metadata v5 working")
    
    print("\nTesting produce...")
    test.test_produce_basic()
    print("✓ Produce request accepted (but doesn't persist)")
    
    print("\nTesting fetch...")
    try:
        test.test_fetch_basic()
        print("✓ Fetch working")
    except:
        print("✗ Fetch returns empty (expected)")
    
    print("\nTesting DescribeConfigs...")
    try:
        test.test_describe_configs_error()
        print("✓ DescribeConfigs working")
    except AssertionError:
        print("✗ DescribeConfigs has correlation ID bug (known issue)")
    
    print("\nSummary: Basic protocol working, but no persistence yet")