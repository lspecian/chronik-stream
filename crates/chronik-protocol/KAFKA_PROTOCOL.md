# Kafka Wire Protocol Implementation

This module provides a comprehensive implementation of the Kafka binary wire protocol for Chronik Stream.

## Architecture

The implementation is organized into several key modules:

### Core Protocol (`kafka_protocol.rs`)
- Complete Kafka protocol types and utilities
- API key enumeration (Produce, Fetch, Metadata, etc.)
- Error code definitions
- Request/Response header encoding/decoding
- Protocol utilities for compact types and varints
- API version support information

### Frame Handling (`frame.rs`)
- Length-prefixed message framing
- Kafka uses 4-byte length prefix for all messages
- Configurable maximum frame size to prevent OOM
- Integration with tokio-util codec traits

### Compression Support (`compression.rs`)
- Support for multiple compression codecs:
  - None (0)
  - Gzip (1) - fully implemented
  - Snappy (2) - placeholder
  - LZ4 (3) - placeholder
  - Zstd (4) - placeholder
- Message batching with size limits
- Transparent compression/decompression

### Record Batch Format (`records.rs`)
- Kafka record batch format v2 implementation
- Support for:
  - Record headers
  - Key/value pairs
  - Timestamps (CreateTime/LogAppendTime)
  - Transactional records
  - CRC validation
- Varint encoding for efficient storage

## Key Features

1. **API Version Negotiation**: Supports Kafka 2.x through 4.x protocols
2. **Flexible Versions**: Support for compact types and tagged fields
3. **Binary Serialization**: Efficient encoding/decoding with proper endianness
4. **Error Handling**: Comprehensive error codes matching Kafka specification
5. **Protocol Conformance**: Tests ensure compatibility with official Kafka clients

## Usage Example

```rust
use chronik_protocol::{
    kafka_protocol::{ApiKey, RequestHeader, ProtocolCodec},
    frame::KafkaFrameCodec,
    compression::CompressionType,
    records::RecordBatch,
};

// Create a request header
let header = RequestHeader {
    api_key: ApiKey::Metadata,
    api_version: 12,
    correlation_id: 123,
    client_id: Some("my-client".to_string()),
};

// Encode metadata response
let mut buf = BytesMut::new();
ProtocolCodec::encode_metadata_response(
    &mut buf,
    correlation_id,
    cluster_id,
    controller_id,
    &brokers,
    &topics,
    version,
).unwrap();
```

## Testing

The implementation includes comprehensive tests:
- Protocol conformance tests
- Frame codec tests
- Compression tests
- Record batch encoding/decoding tests
- API version negotiation tests

Run tests with: `cargo test -p chronik-protocol`