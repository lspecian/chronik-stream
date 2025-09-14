# Consumer Group Tests

This directory contains test scripts specifically for validating Kafka consumer group coordination functionality.

## Files

- `produce_first.py` - Producer script to create test messages for consumer testing
- `test_consumer_*.py` - Various consumer test scripts for different validation scenarios
- `test_simple_consumer.py` - Basic consumer test script

## Usage

These tests were used to validate the consumer group coordination fix for v1.0.2, specifically the resolution of "Unknown Group" errors during consumer group coordination.

## Test Scenarios

- Consumer group creation (auto-creation)
- FindCoordinator API validation
- JoinGroup API validation
- SyncGroup API validation
- End-to-end message consumption after group coordination