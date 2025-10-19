# Chronik Raft - Usage Examples

This document provides practical examples of using the Raft RPC protocol defined in Phase 1.

## Prerequisites

Install Protocol Buffers compiler:
```bash
# macOS
brew install protobuf

# Ubuntu/Debian
sudo apt install protobuf-compiler

# Verify installation
protoc --version  # Should be >= 3.1.0
```

Build the crate:
```bash
cargo build --package chronik-raft
```

## Example 1: Starting a Raft RPC Server

```rust
use chronik_raft::rpc::{RaftServiceImpl, start_raft_server};
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Create Raft service
    let service = RaftServiceImpl::new();

    // Start gRPC server on port 5001
    println!("Starting Raft RPC server on 0.0.0.0:5001");
    start_raft_server("0.0.0.0:5001".to_string(), service).await?;

    Ok(())
}
```

## Example 2: Sending AppendEntries RPC

```rust
use chronik_raft::rpc::proto::*;
use tonic::Request;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to Raft node
    let mut client = raft_service_client::RaftServiceClient::connect(
        "http://localhost:5001"
    ).await?;

    // Create AppendEntries request (heartbeat)
    let request = Request::new(AppendEntriesRequest {
        term: 1,
        leader_id: 1,
        prev_log_index: 0,
        prev_log_term: 0,
        entries: vec![],  // Empty for heartbeat
        leader_commit: 0,
    });

    // Send RPC
    let response = client.append_entries(request).await?;
    let resp = response.into_inner();

    println!("AppendEntries response:");
    println!("  Term: {}", resp.term);
    println!("  Success: {}", resp.success);

    Ok(())
}
```

## Example 3: Sending AppendEntries with Log Entries

```rust
use chronik_raft::rpc::proto::*;
use tonic::Request;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = raft_service_client::RaftServiceClient::connect(
        "http://localhost:5001"
    ).await?;

    // Create log entries
    let entries = vec![
        LogEntry {
            term: 1,
            index: 1,
            data: bincode::serialize("command1")?,
        },
        LogEntry {
            term: 1,
            index: 2,
            data: bincode::serialize("command2")?,
        },
    ];

    // Create AppendEntries request with log entries
    let request = Request::new(AppendEntriesRequest {
        term: 1,
        leader_id: 1,
        prev_log_index: 0,
        prev_log_term: 0,
        entries,
        leader_commit: 0,
    });

    // Send RPC
    let response = client.append_entries(request).await?;
    let resp = response.into_inner();

    if resp.success {
        println!("Entries replicated successfully");
    } else {
        println!("Replication failed:");
        println!("  Conflict index: {}", resp.conflict_index);
        println!("  Conflict term: {}", resp.conflict_term);
    }

    Ok(())
}
```

## Example 4: Requesting Votes During Election

```rust
use chronik_raft::rpc::proto::*;
use tonic::Request;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = raft_service_client::RaftServiceClient::connect(
        "http://localhost:5001"
    ).await?;

    // Request vote from peer
    let request = Request::new(RequestVoteRequest {
        term: 2,
        candidate_id: 3,
        last_log_index: 10,
        last_log_term: 1,
    });

    let response = client.request_vote(request).await?;
    let resp = response.into_inner();

    if resp.vote_granted {
        println!("Vote granted by node");
    } else {
        println!("Vote denied (term: {})", resp.term);
    }

    Ok(())
}
```

## Example 5: Installing Snapshot (Streaming)

```rust
use chronik_raft::rpc::proto::*;
use tonic::Request;
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = raft_service_client::RaftServiceClient::connect(
        "http://localhost:5001"
    ).await?;

    // Load snapshot data (simulated)
    let snapshot_data = vec![0u8; 10_000]; // 10KB snapshot
    let chunk_size = 1024; // 1KB chunks

    // Create streaming request
    let chunks: Vec<InstallSnapshotRequest> = snapshot_data
        .chunks(chunk_size)
        .enumerate()
        .map(|(i, chunk)| {
            let offset = (i * chunk_size) as u64;
            let is_last = offset + chunk.len() as u64 >= snapshot_data.len() as u64;

            InstallSnapshotRequest {
                term: 1,
                leader_id: 1,
                last_included_index: 100,
                last_included_term: 1,
                offset,
                data: chunk.to_vec(),
                done: is_last,
            }
        })
        .collect();

    // Convert to stream
    let stream = tokio_stream::iter(chunks);

    // Send streaming RPC
    let response = client.install_snapshot(Request::new(stream)).await?;
    let resp = response.into_inner();

    if resp.success {
        println!("Snapshot installed successfully");
    } else {
        println!("Snapshot installation failed (term: {})", resp.term);
    }

    Ok(())
}
```

## Example 6: Registering Partition Replicas

```rust
use chronik_raft::{
    rpc::RaftServiceImpl,
    replica::PartitionReplica,
    config::RaftConfig,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create Raft service
    let service = RaftServiceImpl::new();

    // Create configuration
    let config = RaftConfig {
        node_id: 1,
        listen_addr: "0.0.0.0:5001".to_string(),
        ..Default::default()
    };

    // Create partition replicas
    let replica1 = Arc::new(PartitionReplica::new(
        "my-topic".to_string(),
        0,
        config.clone(),
    )?);

    let replica2 = Arc::new(PartitionReplica::new(
        "my-topic".to_string(),
        1,
        config.clone(),
    )?);

    // Register replicas with service
    service.register_replica(replica1);
    service.register_replica(replica2);

    println!("Registered 2 partition replicas");

    // Start server
    // start_raft_server("0.0.0.0:5001".to_string(), service).await?;

    Ok(())
}
```

## Example 7: Error Handling

```rust
use chronik_raft::rpc::proto::*;
use tonic::{Request, Status};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client_result = raft_service_client::RaftServiceClient::connect(
        "http://invalid-host:5001"
    ).await;

    match client_result {
        Ok(mut client) => {
            // Connection successful, send RPC
            let request = Request::new(AppendEntriesRequest {
                term: 1,
                leader_id: 1,
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![],
                leader_commit: 0,
            });

            match client.append_entries(request).await {
                Ok(response) => {
                    println!("RPC successful: {:?}", response.into_inner());
                }
                Err(status) => {
                    eprintln!("RPC failed: {}", status.message());
                }
            }
        }
        Err(e) => {
            eprintln!("Connection failed: {}", e);
        }
    }

    Ok(())
}
```

## Example 8: Using with Tracing

```rust
use chronik_raft::rpc::proto::*;
use tonic::Request;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    info!("Connecting to Raft node");

    let mut client = raft_service_client::RaftServiceClient::connect(
        "http://localhost:5001"
    ).await?;

    info!("Connected successfully");

    let request = Request::new(AppendEntriesRequest {
        term: 1,
        leader_id: 1,
        prev_log_index: 0,
        prev_log_term: 0,
        entries: vec![],
        leader_commit: 0,
    });

    info!("Sending AppendEntries RPC");

    match client.append_entries(request).await {
        Ok(response) => {
            let resp = response.into_inner();
            if resp.success {
                info!("AppendEntries succeeded");
            } else {
                warn!("AppendEntries failed: conflict at index {}", resp.conflict_index);
            }
        }
        Err(e) => {
            error!("RPC error: {}", e);
        }
    }

    Ok(())
}
```

## Testing

Run the test suite:
```bash
# Run all tests
cargo test --package chronik-raft

# Run specific test
cargo test --package chronik-raft test_service_creation

# Run with output
cargo test --package chronik-raft -- --nocapture

# Run only unit tests (skip integration tests)
cargo test --package chronik-raft --lib
```

## Debugging

Enable debug logging to see RPC traffic:
```bash
RUST_LOG=chronik_raft=debug cargo run --package chronik-raft
```

View protobuf generated code:
```bash
# Generated code is in target/debug/build/chronik-raft-*/out/
ls target/debug/build/chronik-raft-*/out/
```

## Common Issues

### 1. `protoc` not found

**Error**: `No suitable protoc (>= 3.1.0) found in PATH`

**Solution**: Install Protocol Buffers compiler
```bash
# macOS
brew install protobuf

# Ubuntu/Debian
sudo apt install protobuf-compiler
```

### 2. Connection refused

**Error**: `Connection refused` when connecting to Raft node

**Solution**:
- Ensure server is running: `cargo run --package chronik-raft`
- Check firewall settings
- Verify correct address/port

### 3. RPC timeout

**Error**: `Deadline exceeded`

**Solution**: Set custom timeout
```rust
let channel = tonic::transport::Channel::from_static("http://localhost:5001")
    .timeout(std::time::Duration::from_secs(30))
    .connect()
    .await?;

let mut client = raft_service_client::RaftServiceClient::new(channel);
```

## Next Steps

These examples demonstrate the Phase 1 RPC protocol. Future phases will add:

- **Phase 2**: Raft state machine (leader election, log replication)
- **Phase 3**: WAL-backed storage integration
- **Phase 4**: Cluster membership and configuration changes
- **Phase 5**: Production hardening and monitoring

For more information, see:
- `PHASE1_SUMMARY.md` - Complete Phase 1 documentation
- `proto/raft_rpc.proto` - Protocol definition
- `src/rpc.rs` - Service implementation
