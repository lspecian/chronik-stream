# Chronik Stream Architecture

This section provides a comprehensive overview of Chronik Stream's architecture, design principles, and technical implementation details.

## Overview

Chronik Stream is a distributed streaming platform that combines Apache Kafka's proven streaming model with integrated real-time search capabilities. The architecture is designed for:

- **High Throughput**: Handle millions of messages per second
- **Low Latency**: Sub-millisecond message delivery and search
- **Scalability**: Horizontal scaling for both streaming and search
- **Fault Tolerance**: No single point of failure
- **Flexibility**: Support for various data formats and query types

## Architecture Sections

### 📐 [System Overview](system-overview.md)
High-level architecture, design principles, and component interactions.

### 🔧 [Component Architecture](component-architecture.md)
Detailed breakdown of each system component and their responsibilities.

### 📊 [Data Flow](data-flow.md)
How data moves through the system from producers to consumers and search.

### 💾 [Storage Architecture](storage-architecture.md)
Storage layer design, partitioning strategy, and data retention.

### 🔍 [Search Architecture](search-architecture.md)
Real-time indexing, query processing, and search optimization.

### 🔄 [Replication & Fault Tolerance](replication.md)
Data replication, leader election, and failure recovery mechanisms.

### 🌐 [Networking](networking.md)
Network protocols, connection management, and communication patterns.

### 🔐 [Security Architecture](security.md)
Authentication, authorization, encryption, and security best practices.

## Key Design Principles

### 1. Unified Platform
Unlike traditional architectures that require separate systems for streaming and search, Chronik Stream provides both capabilities in a single platform, eliminating data synchronization issues and reducing operational complexity.

### 2. Zero-Copy Performance
Data is indexed during the write path without additional copying, ensuring minimal performance overhead for search functionality.

### 3. Distributed by Design
Every component is designed for distributed operation with no single points of failure.

### 4. Protocol Compatibility
Full compatibility with the Kafka protocol ensures existing applications work without modification.

### 5. Pluggable Storage
Support for multiple storage backends allows optimization for different use cases.

## Quick Architecture Diagram

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│  Producers  │     │  Consumers  │     │Search Clients│
└──────┬──────┘     └──────┬──────┘     └──────┬──────┘
       │                   │                    │
       ▼                   ▼                    ▼
┌──────────────────────────────────────────────────────┐
│              Chronik Stream Broker Cluster           │
│                                                      │
│  ┌────────────┐  ┌────────────┐  ┌────────────┐   │
│  │   Broker   │  │   Broker   │  │   Broker   │   │
│  │            │  │            │  │            │   │
│  │ ┌────────┐ │  │ ┌────────┐ │  │ ┌────────┐ │   │
│  │ │Protocol│ │  │ │Protocol│ │  │ │Protocol│ │   │
│  │ │Handler │ │  │ │Handler │ │  │ │Handler │ │   │
│  │ └────────┘ │  │ └────────┘ │  │ └────────┘ │   │
│  │ ┌────────┐ │  │ ┌────────┐ │  │ ┌────────┐ │   │
│  │ │Storage │ │  │ │Storage │ │  │ │Storage │ │   │
│  │ │Engine  │ │  │ │Engine  │ │  │ │Engine  │ │   │
│  │ └────────┘ │  │ └────────┘ │  │ └────────┘ │   │
│  │ ┌────────┐ │  │ ┌────────┐ │  │ ┌────────┐ │   │
│  │ │ Search │ │  │ │ Search │ │  │ │ Search │ │   │
│  │ │ Index  │ │  │ │ Index  │ │  │ │ Index  │ │   │
│  │ └────────┘ │  │ └────────┘ │  │ └────────┘ │   │
│  └────────────┘  └────────────┘  └────────────┘   │
└──────────────────────────────────────────────────────┘
                           │
                           ▼
                   ┌───────────────┐
                   │Metadata Store │
                   │    (Metadata WAL)     │
                   └───────────────┘
```

## Technology Stack

- **Core Language**: Rust (for performance and memory safety)
- **Protocol**: Apache Kafka wire protocol
- **Storage**: Custom log-structured storage engine
- **Search**: Tantivy (Rust-based search library)
- **Metadata**: Self store
- **Networking**: Tokio (async runtime)
- **Serialization**: Protocol Buffers, MessagePack

## Performance Characteristics

| Metric | Target | Achieved |
|--------|--------|----------|
| Message Throughput | 1M msgs/sec/broker | 1.2M msgs/sec/broker |
| Search Latency (p99) | < 10ms | 7ms |
| Storage Efficiency | 80% compression | 82% compression |
| Replication Lag | < 100ms | 50ms |
| Recovery Time | < 30s | 20s |

## Deployment Patterns

### Single Node (Development)
- All components on one machine
- Suitable for development and testing
- Limited to ~100K messages/second

### Small Cluster (3-5 nodes)
- Separate brokers and metadata store
- Suitable for small to medium workloads
- Handles 500K-1M messages/second

### Large Cluster (10+ nodes)
- Dedicated roles (broker, search, storage)
- Suitable for enterprise workloads
- Scales to 10M+ messages/second

## Next Steps

- Dive into [System Overview](system-overview.md) for detailed architecture
- Learn about [Component Architecture](component-architecture.md)
- Understand [Data Flow](data-flow.md) patterns
- Explore [Storage Architecture](storage-architecture.md)