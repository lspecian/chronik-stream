# Chronik Stream Tests

**Canonical Tests**: 28 Rust integration tests in `integration/*.rs`

---

## Quick Start

### Run All Tests
```bash
./tests/run_all_tests.sh
```

### Run Specific Tests
```bash
# Unit tests
cargo test --workspace --lib --bins

# Integration tests
cargo test --test integration
cargo test --test kafka_compatibility_test
cargo test --test wal_recovery_test
```

---

## Test Organization

```
tests/
├── integration/          # Rust integration tests (CANONICAL)
│   ├── kafka_compatibility_test.rs
│   ├── wal_recovery_test.rs
│   ├── consumer_groups.rs
│   ├── admin_api_test.rs
│   └── *.rs
│
├── raft/                 # Raft-specific tests
│   └── *.rs
│
├── scripts/              # Helper scripts
│   ├── audit_tests.sh
│   ├── cleanup_tests.sh
│   └── start_cluster.sh
│
├── run_all_tests.sh      # Master test runner
├── TEST_INVENTORY.md     # Test catalog
└── README.md             # This file
```

---

## Cleanup Required

**CRITICAL**: 292 Python test files need to be deleted (experimental debris).

```bash
# Review what will be deleted
./tests/scripts/audit_tests.sh

# Execute cleanup
./tests/scripts/cleanup_tests.sh --execute
```

See [docs/TEST_CLEANUP_SUMMARY.md](../docs/TEST_CLEANUP_SUMMARY.md) for details.

---

## Testing Standards

### DO ✅
- Write Rust integration tests in `tests/integration/`
- Test with real Kafka clients directly (one-liners, interactive)
- Document tested clients in PRs
- Delete experimental files immediately

### DON'T ❌
- Create Python test files
- Commit debug scripts
- Keep temporary test output

**See**: [docs/TESTING_STANDARDS.md](../docs/TESTING_STANDARDS.md)

---

## Client Compatibility Testing

### kafka-python
```bash
cargo run --bin chronik-server start

python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers='localhost:9092')
producer.send('test', b'hello')
producer.flush()
print('✓ Works')
"
```

### confluent-kafka (librdkafka)
```bash
cargo run --bin chronik-server start

python3 -c "
from confluent_kafka import Producer
p = Producer({'bootstrap.servers': 'localhost:9092'})
p.produce('test', b'hello')
p.flush()
print('✓ Works')
"
```

### Java/KSQLDB
```bash
cargo run --bin chronik-server start
cd ksql/confluent-7.5.0/
bin/ksql http://localhost:8088
```

---

## Before Every Release

- [ ] Run `./tests/run_all_tests.sh`
- [ ] Test with kafka-python (manual)
- [ ] Test with confluent-kafka (manual)
- [ ] Test with Java clients if changed (KSQLDB)
- [ ] Verify no Python files: `find tests -name '*.py'` (should be empty)

---

## Documentation

- **[docs/TESTING_STANDARDS.md](../docs/TESTING_STANDARDS.md)** - Complete testing standards
- **[docs/TEST_CLEANUP_SUMMARY.md](../docs/TEST_CLEANUP_SUMMARY.md)** - Cleanup instructions
- **[TEST_INVENTORY.md](./TEST_INVENTORY.md)** - Test catalog and status
- **[CLAUDE.md](../CLAUDE.md)** - Project overview and work ethic
