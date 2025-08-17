# Integration Tests for Chronik Stream

## Overview

This directory contains integration tests that validate Chronik Stream's Kafka compatibility and core functionality.

## Test Categories

### 1. Protocol Compatibility Tests (`test_protocol_*.py`)
- Verify wire protocol compatibility with Kafka
- Test all supported API versions
- Validate error handling and correlation IDs

### 2. Core Functionality Tests (`test_core_*.py`)
- Message produce/consume flows
- Topic management operations
- Partition assignment and rebalancing

### 3. Consumer Group Tests (`test_consumer_*.py`)
- Group coordination
- Offset management
- Rebalancing scenarios

### 4. Performance Tests (`test_perf_*.py`)
- Throughput benchmarks
- Latency measurements
- Concurrent client stress tests

### 5. Search Integration Tests (`test_search_*.py`)
- Message indexing
- Query functionality
- Search performance

## Running Tests

### Prerequisites
```bash
# Start Chronik Stream
docker-compose up -d

# Install test dependencies
pip install kafka-python pytest pytest-asyncio
```

### Run All Tests
```bash
pytest tests/integration/
```

### Run Specific Category
```bash
pytest tests/integration/test_protocol_*.py -v
```

### Run with Coverage
```bash
pytest tests/integration/ --cov=chronik --cov-report=html
```

## Test Structure

Each test file follows this pattern:

```python
import pytest
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic

class TestFeatureName:
    @pytest.fixture
    def kafka_client(self):
        # Setup client
        yield client
        # Cleanup
    
    def test_specific_scenario(self, kafka_client):
        # Test implementation
        pass
```

## Current Test Status

### ✅ Implemented
- Basic protocol parsing tests
- Metadata API tests
- API version negotiation

### 🚧 In Progress
- Message persistence tests
- Consumer group coordination

### ❌ Not Started
- Performance benchmarks
- Search integration
- Multi-client scenarios
- Failure recovery tests

## Adding New Tests

1. Create test file following naming convention
2. Use appropriate fixtures for setup/teardown
3. Test both success and error cases
4. Add performance assertions where relevant
5. Document any special requirements

## Known Limitations

1. Consumer group tests currently fail (not implemented)
2. Performance tests need baseline metrics
3. Search tests require search service integration
4. Some Kafka client features not compatible

## CI Integration

Tests run automatically on:
- Every PR via GitHub Actions
- Nightly for extended test suite
- Release branches for full validation

See `.github/workflows/integration-tests.yml` for configuration.