# Kafka Compatibility Testing Framework

This directory contains comprehensive compatibility tests for Chronik Stream, validating its compatibility with various Kafka client libraries.

## Overview

The framework tests Chronik Stream against multiple Kafka clients to ensure protocol compatibility:

- **kafka-python** (Python, pure implementation)
- **confluent-kafka** (Python, librdkafka-based)
- **Sarama** (Go, IBM's pure Go implementation)
- **confluent-kafka-go** (Go, librdkafka-based)

## Quick Start

### Run a Single Test

```bash
# Quick validation with kafka-python
./validate-test.sh

# Test a specific client
./scripts/run-tests.sh --client kafka-python
```

### Run All Tests

```bash
# Run complete test suite
./scripts/run-tests.sh

# With cleanup and rebuild
./scripts/run-tests.sh --clean --build
```

## Directory Structure

```
compat-tests/
├── clients/                    # Test client implementations
│   ├── python/
│   │   ├── kafka-python/       # Pure Python Kafka client tests
│   │   │   ├── test.py
│   │   │   └── Dockerfile
│   │   └── confluent-kafka/    # librdkafka Python tests
│   │       ├── test.py
│   │       └── Dockerfile
│   └── go/
│       ├── sarama/              # IBM Sarama Go client tests
│       │   ├── test.go
│       │   └── Dockerfile
│       └── librdkafka/          # confluent-kafka-go tests
│           ├── test.go
│           └── Dockerfile
├── scripts/
│   ├── run-tests.sh             # Main test runner
│   └── gen-matrix.py            # Compatibility matrix generator
├── results/                     # Test results (auto-generated)
├── docs/                        # Documentation (auto-generated)
└── docker-compose.yaml          # Container orchestration
```

## Test Coverage

Each client tests the following Kafka operations:

### Core APIs
- **ApiVersions**: Version negotiation (including v3 and v0 fallback)
- **Metadata**: Topic and broker discovery
- **Produce**: Message production
- **Fetch**: Message consumption
- **ConsumerGroup**: Group coordination

### Regression Tests
- **ProduceResponse v2**: Validates throttle_time_ms field positioning
- **ApiVersions fallback**: Tests librdkafka v0 fallback behavior
- **Consumer group rebalancing**: Ensures proper group coordination

## Running Tests

### Prerequisites

- Docker and Docker Compose
- Python 3.x (for matrix generation)
- At least 2GB free memory

### Command Options

```bash
# Basic usage
./scripts/run-tests.sh

# Test specific client
./scripts/run-tests.sh --client kafka-python
./scripts/run-tests.sh --client confluent-kafka
./scripts/run-tests.sh --client sarama
./scripts/run-tests.sh --client confluent-kafka-go

# Clean environment before testing
./scripts/run-tests.sh --clean

# Force rebuild containers
./scripts/run-tests.sh --build

# Combined options
./scripts/run-tests.sh --clean --build --client kafka-python
```

### Test Output

Tests produce:
1. **JSON results** in `results/` directory
2. **Structured logs** to stdout
3. **Compatibility matrix** in `docs/compatibility-matrix.md`

Example result structure:
```json
{
  "client": "kafka-python",
  "version": "2.0.2",
  "timestamp": "2024-01-15T10:30:00Z",
  "tests": [
    {
      "test": "ApiVersions",
      "api": "ApiVersions",
      "passed": true
    }
  ],
  "summary": {
    "total": 5,
    "passed": 5,
    "failed": 0
  }
}
```

## Test Implementation Details

### Python Tests (kafka-python, confluent-kafka)

Each Python test client implements:
```python
def test_api_versions(bootstrap_servers)
def test_metadata(bootstrap_servers)
def test_produce(bootstrap_servers)
def test_fetch(bootstrap_servers)
def test_consumer_group(bootstrap_servers)
```

### Go Tests (Sarama, confluent-kafka-go)

Each Go test client implements:
```go
func testApiVersions(brokers string) (bool, string)
func testMetadata(brokers string) (bool, string)
func testProduce(brokers string) (bool, string)
func testFetch(brokers string) (bool, string)
func testConsumerGroup(brokers string) (bool, string)
```

## Compatibility Matrix

The generated matrix shows test results across all clients:

| Test | kafka-python | confluent-kafka | sarama | confluent-kafka-go |
|------|--------------|-----------------|---------|-------------------|
| ApiVersions | ✅ | ✅ | ✅ | ✅ |
| Metadata | ✅ | ✅ | ✅ | ✅ |
| Produce | ✅ | ✅ | ✅ | ✅ |
| Fetch | ✅ | ✅ | ✅ | ✅ |
| ConsumerGroup | ✅ | ✅ | ✅ | ✅ |
| ProduceV2Regression | - | ✅ | - | ✅ |

### Success Rates

- **100%**: Full compatibility ✅
- **80-99%**: Good compatibility with minor issues ⚠️
- **<80%**: Significant compatibility issues ❌

## Adding New Tests

### 1. Add a New Client

Create a new directory under `clients/` with:
- Test implementation (`test.py`, `test.go`, etc.)
- `Dockerfile` for containerization
- Add service to `docker-compose.yaml`

### 2. Add Test Cases

Update existing test files to include new test cases:

```python
def test_new_feature(bootstrap_servers):
    """Test new Kafka feature"""
    # Implementation
    return passed, "NewFeature"
```

### 3. Update Test Runner

Add the new client to `scripts/run-tests.sh`:

```bash
case $client in
    new-client)
        service_name="new-client-service"
        ;;
```

## Troubleshooting

### Common Issues

1. **Chronik fails to start**
   - Check port 9092 is available
   - Verify Docker has enough memory
   - Check Chronik logs: `docker-compose logs chronik`

2. **Tests timeout**
   - Increase timeout in test scripts
   - Check network connectivity
   - Verify Chronik is healthy

3. **Results not generated**
   - Check volume mounts in docker-compose.yaml
   - Verify results directory exists
   - Check container logs for errors

### Debug Commands

```bash
# View Chronik logs
docker-compose logs chronik

# Check container status
docker-compose ps

# Run test interactively
docker-compose run --rm kafka-python-client /bin/sh

# Clean everything
docker-compose down -v
docker system prune -f
```

## Known Limitations

1. Tests run sequentially (not in parallel)
2. Limited to Docker environments
3. Results are overwritten on each run (timestamped backups recommended)

## Regression Test Details

### ProduceResponse v2 Issue

The ProduceResponse v2 has a field ordering issue where `throttle_time_ms` appears at a different position than documented. This affects:
- kafka-python when using api_version=(0, 10, 1)
- Older librdkafka versions

### ApiVersions Compatibility

Some clients (notably librdkafka) require fallback to ApiVersions v0 when v3 fails. The test framework specifically tests this fallback behavior.

## Contributing

To add new compatibility tests:

1. Identify the protocol feature to test
2. Implement test in relevant client libraries
3. Ensure consistent test naming across clients
4. Update this README with new test descriptions
5. Submit PR with test results

## Related Documentation

- [Chronik Stream Documentation](../README.md)
- [Kafka Protocol Specification](https://kafka.apache.org/protocol)
- [Compatibility Matrix](docs/compatibility-matrix.md)