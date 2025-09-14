# Chronik Stream Roadmap

## Current State (as of 2025-07-11)

The project has a solid foundation with ~60% completion, but critical gaps prevent it from being a functional Kafka replacement.

### What's Working ✅
- Docker-compose infrastructure with all services
- Basic Kafka wire protocol parsing/encoding
- File-based metadata store
- Search service (Tantivy-based)
- Kubernetes operator framework
- Observability instrumentation

### What's Broken ❌
- Correlation ID handling causes kafkactl failures
- No actual message persistence in Produce handler
- Fetch returns empty responses
- Consumer groups non-functional
- 21 Kafka APIs unimplemented

## Phase 1: Core Functionality (Weeks 1-2)

### 1.1 Fix Critical Bugs
- [ ] Fix correlation ID handling for unknown APIs (DescribeConfigs)
- [ ] Ensure all error responses preserve correlation IDs
- [ ] Make kafkactl work for basic operations

### 1.2 Implement Message Persistence
- [ ] Wire Produce handler to actually store messages
- [ ] Implement proper offset tracking
- [ ] Make Fetch handler read from storage
- [ ] Add topic auto-creation support

### 1.3 Clean Up Testing
- [ ] Move all test files from root to `tests/` directory
- [ ] Create `tests/python/` for Python tests
- [ ] Create `tests/integration/` for end-to-end tests
- [ ] Remove debugging scripts or move to `scripts/debug/`

## Phase 2: Kafka Compatibility (Weeks 3-4)

### 2.1 Essential Consumer APIs
- [ ] Implement OffsetCommit
- [ ] Implement OffsetFetch
- [ ] Complete FindCoordinator
- [ ] Fix JoinGroup/SyncGroup/Heartbeat

### 2.2 Topic Management
- [ ] Implement CreateTopics properly
- [ ] Add DeleteTopics support
- [ ] Make Metadata return actual topics
- [ ] Support topic configuration

### 2.3 Integration Tests
- [ ] Create test suite using kafka-python
- [ ] Test produce/consume workflows
- [ ] Validate consumer group functionality
- [ ] Benchmark performance claims

## Phase 3: Production Features (Weeks 5-6)

### 3.1 Performance Validation
- [ ] Create benchmark suite
- [ ] Test 1M+ messages/second claim
- [ ] Measure actual latencies
- [ ] Optimize hot paths

### 3.2 Search Integration
- [ ] Wire search to actually index messages
- [ ] Implement search query API
- [ ] Add search benchmarks
- [ ] Document search capabilities

### 3.3 Multi-tenancy
- [ ] Complete authentication integration
- [ ] Add namespace/tenant isolation
- [ ] Implement quota management
- [ ] Add access control

## Phase 4: Documentation & Polish (Week 7)

### 4.1 Honest Documentation
- [ ] Update README with actual capabilities
- [ ] Create KNOWN_ISSUES.md
- [ ] Add architecture diagrams
- [ ] Write operation guide

### 4.2 Client Examples
- [ ] Java client example
- [ ] Python client example
- [ ] Go client example
- [ ] Performance tuning guide

### 4.3 Deployment
- [ ] Production Docker images
- [ ] Kubernetes manifests
- [ ] Monitoring dashboards
- [ ] Runbooks

## Success Metrics

### Minimum Viable Product
- [ ] kafkactl can list/create topics
- [ ] kafka-console-producer works
- [ ] kafka-console-consumer works
- [ ] Basic consumer groups functional

### Production Ready
- [ ] 100k+ messages/second sustained
- [ ] < 50ms p99 latency
- [ ] 99.9% uptime over 7 days
- [ ] Full Kafka 2.8 API compatibility

## Quick Wins for This Week

1. **Fix the correlation ID bug** blocking kafkactl
2. **Move test files** to proper directory structure
3. **Implement basic produce/fetch** persistence
4. **Update README** to reflect reality
5. **Create working demo** with producer/consumer

## Known Technical Debt

1. **TODO Comments**: 68 TODOs need addressing
2. **Error Handling**: Many unwrap() calls need proper error handling
3. **Performance**: No optimization work done yet
4. **Testing**: Integration test coverage < 20%
5. **Security**: Auth not integrated into request flow

## Next Steps

1. Start with Phase 1.1 - Fix critical bugs
2. Set up proper CI/CD with integration tests
3. Create performance benchmark harness
4. Regular progress tracking against this roadmap

---

*Note: This roadmap is based on honest assessment of current state. The project has good bones but needs significant work to deliver on its promises.*