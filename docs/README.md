# Chronik Documentation

Documentation for Chronik Stream, a high-performance Kafka-compatible streaming platform.

## Quick Start

| Document | Description |
|----------|-------------|
| [BUILD_INSTRUCTIONS.md](BUILD_INSTRUCTIONS.md) | Building from source |
| [DEPLOYMENT.md](DEPLOYMENT.md) | Deployment guide |
| [RUNNING_A_CLUSTER.md](RUNNING_A_CLUSTER.md) | Running a 3-node cluster |
| [DOCKER.md](DOCKER.md) | Docker deployment |

## Features

| Document | Description |
|----------|-------------|
| [SEARCHABLE_TOPICS.md](SEARCHABLE_TOPICS.md) | Real-time full-text search with Tantivy |
| [COMPRESSION_SUPPORT.md](COMPRESSION_SUPPORT.md) | Gzip/Snappy/LZ4/Zstd compression |
| [DISASTER_RECOVERY.md](DISASTER_RECOVERY.md) | S3/GCS/Azure backup and recovery |

## Operations

| Document | Description |
|----------|-------------|
| [ADMIN_API_SECURITY.md](ADMIN_API_SECURITY.md) | Admin API authentication and TLS |
| [TESTING.md](TESTING.md) | Testing guide |
| [TESTING_NODE_REMOVAL.md](TESTING_NODE_REMOVAL.md) | Testing node removal scenarios |

## Compatibility

| Document | Description |
|----------|-------------|
| [kafka-client-compatibility.md](kafka-client-compatibility.md) | Kafka client compatibility matrix |

## Internals

| Document | Description |
|----------|-------------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | System architecture overview |
| [wal/](wal/) | WAL implementation details |
| [grafana/](grafana/) | Grafana dashboard JSON |

## Directory Structure

```
docs/
├── ADMIN_API_SECURITY.md    # Admin API security
├── ARCHITECTURE.md          # Architecture overview
├── BUILD_INSTRUCTIONS.md    # Build guide
├── COMPRESSION_SUPPORT.md   # Compression support
├── DEPLOYMENT.md            # Deployment guide
├── DISASTER_RECOVERY.md     # DR with S3/GCS/Azure
├── DOCKER.md                # Docker deployment
├── DOCKER_PUBLISH.md        # Publishing Docker images
├── kafka-client-compatibility.md  # Client compatibility
├── README.md                # This file
├── RUNNING_A_CLUSTER.md     # Cluster deployment
├── SEARCHABLE_TOPICS.md     # Full-text search feature
├── TESTING.md               # Testing guide
├── TESTING_NODE_REMOVAL.md  # Node removal testing
├── grafana/                 # Grafana dashboards
│   └── chronik_raft_dashboard.json
└── wal/                     # WAL internals
    ├── architecture.md
    ├── concurrency.md
    ├── performance.md
    └── thread_safety_audit.md
```

## Related Documentation

- [Main README](../README.md) - Project overview and quick start
- [CLAUDE.md](../CLAUDE.md) - Development guidelines and architecture
- [tests/cluster/](../tests/cluster/) - Test cluster configuration
