# Known Test Issues

## chronik-common TiKV Connection Tests

The tests in `chronik-common` that require TiKV connection fail with hostname resolution errors. 

**Issue**: While tests specify `localhost:2379` for TiKV connection, the TiKV cluster discovery mechanism returns member information with hostname `pd0:2379`, which cannot be resolved outside the Docker network.

**Error Message**:
```
StorageError("Failed to connect to TiKV: ... failed to connect to [Member { ... client_urls: [\"http://pd0:2379\"] ... }]")
```

**Workaround Options**:
1. Use the test-specific docker-compose configuration:
   ```bash
   # Start TiKV with localhost advertising
   docker-compose -f docker-compose.test.yml up -d
   
   # Run tests
   cargo test -p chronik-common
   
   # Clean up
   docker-compose -f docker-compose.test.yml down -v
   ```

2. Or use the provided test script:
   ```bash
   ./scripts/test-with-tikv.sh
   ```

3. Run tests inside Docker network:
   ```bash
   docker-compose exec pd0 cargo test -p chronik-common
   ```

4. Add hostname mapping to `/etc/hosts` (requires sudo):
   ```
   127.0.0.1 pd0
   ```

5. Set environment variable to override TiKV endpoints in tests:
   ```bash
   TIKV_PD_ENDPOINTS=localhost:2379 cargo test -p chronik-common
   ```

**Affected Tests**:
- `metadata::tests::tests::test_init_system_state`
- `metadata::tests::tests::test_broker_operations`
- `metadata::tests::tests::test_consumer_group_operations`
- `metadata::tests::tests::test_consumer_offsets`
- `metadata::tests::tests::test_partition_assignments`
- `metadata::tests::tests::test_segment_operations`
- `metadata::tests::tests::test_topic_already_exists`
- `metadata::tests::tests::test_topic_crud_operations`
- `metadata::tests::tests::test_topic_deletion_cascades`

## chronik-ingest Test Compilation

The tests in `chronik-ingest` have compilation issues due to:
1. Complex struct initialization with many fields
2. Missing trait implementations in mock objects
3. API changes in dependencies

These are test infrastructure issues, not bugs in the production code.