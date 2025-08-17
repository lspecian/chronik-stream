# Python Tests for Chronik Stream

This directory contains all Python-based tests for Chronik Stream, organized by category.

## Directory Structure

```
tests/python/
├── debug/           # Debug and analysis scripts
├── integration/     # Integration tests with Kafka clients
├── performance/     # Performance and benchmark tests
├── protocol/        # Kafka protocol compatibility tests
├── requirements.txt # Python dependencies
├── run_integration_tests.py # Main test runner
└── test_env.sh     # Environment setup script
```

## Test Categories

### Debug (`debug/`)
Contains debug scripts and analysis tools used during development:
- Protocol analysis scripts
- Response decoders
- Metadata debuggers
- API version analyzers

### Integration (`integration/`)
End-to-end integration tests that verify Chronik Stream works with various Kafka clients:
- `test_kafka_python_integration.py` - Comprehensive kafka-python compatibility tests
- `test_fetch_comprehensive.py` - Comprehensive fetch request testing
- `test_produce_storage.py` - Producer and storage integration
- `test_partition_assignment.py` - Consumer group partition assignment
- `test_search_indexing.py` - Search functionality integration
- Client compatibility tests (Sarama, kafka-python, etc.)

### Performance (`performance/`)
Performance benchmarks and stress tests:
- `test_offset_performance.py` - Offset commit/fetch performance
- `test_offset_tracking.py` - Offset management benchmarks

### Protocol (`protocol/`)
Low-level Kafka protocol compatibility tests:
- API version negotiation tests
- Metadata request/response tests
- Request correlation tests
- Protocol edge cases
- Raw protocol byte verification

## Running Tests

### Setup Environment
```bash
# Install dependencies
pip install -r requirements.txt

# Set up test environment
source test_env.sh
```

### Run All Integration Tests
```bash
python run_integration_tests.py
```

### Run Specific Test Category
```bash
# Run all protocol tests
python -m pytest protocol/

# Run all integration tests
python -m pytest integration/

# Run performance tests
python -m pytest performance/
```

### Run Individual Test
```bash
# Run a specific test file
python integration/test_kafka_python_integration.py

# Run with verbose output
python -v integration/test_kafka_python_integration.py
```

## Test Configuration

Tests expect Chronik Stream to be running on `localhost:9092`. You can override this:

```bash
export CHRONIK_BOOTSTRAP_SERVERS=hostname:port
python integration/test_kafka_python_integration.py
```

## Writing New Tests

1. Choose the appropriate directory based on test type
2. Follow the naming convention: `test_<feature>.py`
3. Use the existing test utilities and base classes
4. Include docstrings explaining what the test verifies
5. Add any new dependencies to `requirements.txt`

## Debugging Failed Tests

1. Check if Chronik Stream is running: `docker-compose ps`
2. Review server logs: `docker-compose logs chronik-stream`
3. Use debug scripts in `debug/` directory for protocol analysis
4. Run tests with `-v` flag for verbose output
5. Set `RUST_LOG=debug` for detailed server logs