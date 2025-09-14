# Chronik Stream Implementation Notes

## Licensing Constraints
**CRITICAL**: Cannot use code from AGPL-licensed projects:
- ❌ Chronik (GNU AGPL) - architectural concepts only
- ❌ Quickwit (AGPL 3.0) - architectural concepts only

**Safe licenses to use:**
- ✅ kafka-protocol (Apache 2.0) - for Kafka wire protocol
- ✅ Tantivy (MIT) - for search engine
- ✅ LaminarMQ (MIT) - for segmented log patterns
- ✅ Lance (Apache 2.0) - for composite file format ideas
- ✅ kube-rs (Apache 2.0) - for Kubernetes operator
- ✅ OpenDAL (Apache 2.0) - for object storage abstraction

## Key Architectural Decisions

### 1. Segment Format (inspired by Lance + LaminarMQ)
- Composite file with data + index + metadata
- Immutable, append-only segments
- Target size: 128MB-512MB per segment
- Include bloom filters for quick key existence checks

### 2. Object Storage Strategy (inspired by Quickwit concepts)
- Implement our own "hotcache" - small metadata header
- Use byte-range requests for efficient access
- Cache segment metadata in memory
- Use OpenDAL for multi-cloud support

### 3. Kafka Protocol Implementation
- Use kafka-protocol crate for wire protocol
- Sans-IO design for testability
- Support Kafka 3.x protocol versions
- Focus on core APIs first (Produce, Fetch, Metadata)

### 4. Search Architecture
- Tantivy for indexing and search
- Dynamic JSON schema support
- Fast fields for aggregations
- Memory-mapped indexes where possible

### 5. Distributed Architecture
- Stateless nodes (compute/storage separation)
- Consistent hashing for work distribution

## Implementation Order
1. Foundation: Project structure, core types
2. Storage: Segment format, object storage layer
3. Protocol: Kafka wire protocol implementation
4. Indexing: Tantivy integration
5. Services: Ingest, Search, Controller nodes
6. Operations: Janitor, compaction
7. Kubernetes: Operator and CRDs
8. Production: Metrics, tracing, security