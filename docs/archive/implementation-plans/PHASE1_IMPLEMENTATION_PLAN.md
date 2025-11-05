# Phase 1 Implementation Plan: CLI Redesign & Unified Config

**Timeline**: Week 1 (5 days)
**Branch**: feat/v2.5.0-kafka-cluster
**Status**: Planning Complete, Ready to Implement

---

## Goals

1. Replace confusing subcommands (`standalone`, `raft-cluster`, etc.) with simple `start` command
2. Move ALL port configuration from CLI flags to config file
3. Add WAL address to NodeConfig (first-class field)
4. Separate bind vs advertise addresses in config
5. Auto-detect single-node vs cluster mode from config

---

## Implementation Steps

### Day 1: Config Structure Redesign

**File**: `crates/chronik-config/src/cluster.rs`

**Changes**:

1. **Add new address structures** (before line 14):
```rust
/// Node addresses for binding (where server listens)
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct NodeBindAddresses {
    /// Kafka API bind address (e.g., "0.0.0.0:9092")
    #[validate(custom = "validate_addr")]
    pub kafka: String,

    /// WAL receiver bind address (e.g., "0.0.0.0:9291")
    #[validate(custom = "validate_addr")]
    pub wal: String,

    /// Raft gRPC bind address (e.g., "0.0.0.0:5001")
    #[validate(custom = "validate_addr")]
    pub raft: String,

    /// Metrics endpoint bind address (optional)
    pub metrics: Option<String>,

    /// Search API bind address (optional, requires search feature)
    pub search: Option<String>,
}

/// Node addresses for advertising (what clients connect to)
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct NodeAdvertiseAddresses {
    /// Kafka API advertised address (e.g., "node1.example.com:9092")
    #[validate(custom = "validate_addr")]
    pub kafka: String,

    /// WAL receiver advertised address (e.g., "node1.example.com:9291")
    #[validate(custom = "validate_addr")]
    pub wal: String,

    /// Raft gRPC advertised address (e.g., "node1.example.com:5001")
    #[validate(custom = "validate_addr")]
    pub raft: String,
}
```

2. **Update NodeConfig** (replace lines 55-69):
```rust
/// Configuration for a single node in the cluster
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct NodeConfig {
    /// Unique node identifier
    #[validate(custom = "validate_node_id")]
    pub id: u64,

    /// Kafka API address (advertised, for clients)
    #[validate(custom = "validate_addr")]
    pub kafka: String,

    /// WAL receiver address (advertised, for followers)
    #[validate(custom = "validate_addr")]
    pub wal: String,

    /// Raft gRPC address (advertised, for peers)
    #[validate(custom = "validate_addr")]
    pub raft: String,
}
```

3. **Add this_node_bind_addresses field to ClusterConfig** (after line 36):
```rust
/// This node's bind addresses (where to listen)
pub bind: Option<NodeBindAddresses>,

/// This node's advertise addresses (what to tell clients)
pub advertise: Option<NodeAdvertiseAddresses>,
```

4. **Update parse_peers_from_env()** (lines 125-155):
```rust
/// Parse peers from CHRONIK_CLUSTER_PEERS environment variable
/// Format: "node1:9092:9291:5001,node2:9092:9291:5001,..."
fn parse_peers_from_env() -> Result<Vec<NodeConfig>> {
    let peers_str = std::env::var("CHRONIK_CLUSTER_PEERS")
        .map_err(|_| ConfigError::Parse("CHRONIK_CLUSTER_PEERS not set".to_string()))?;

    let mut peers = Vec::new();
    for (idx, peer_str) in peers_str.split(',').enumerate() {
        let parts: Vec<&str> = peer_str.split(':').collect();
        if parts.len() != 4 {
            return Err(ConfigError::Parse(format!(
                "Invalid peer format '{}': expected 'host:kafka_port:wal_port:raft_port'",
                peer_str
            )));
        }

        let host = parts[0].to_string();
        let kafka_port = parts[1].parse::<u16>().map_err(|e| {
            ConfigError::Parse(format!("Invalid Kafka port '{}': {}", parts[1], e))
        })?;
        let wal_port = parts[2].parse::<u16>().map_err(|e| {
            ConfigError::Parse(format!("Invalid WAL port '{}': {}", parts[2], e))
        })?;
        let raft_port = parts[3].parse::<u16>().map_err(|e| {
            ConfigError::Parse(format!("Invalid Raft port '{}': {}", parts[3], e))
        })?;

        peers.push(NodeConfig {
            id: (idx + 1) as u64, // Auto-assign IDs based on order
            kafka: format!("{}:{}", host, kafka_port),
            wal: format!("{}:{}", host, wal_port),
            raft: format!("{}:{}", host, raft_port),
        });
    }

    Ok(peers)
}
```

5. **Remove obsolete methods** from NodeConfig:
   - `raft_addr()` (line 267) - replaced by direct `raft` field
   - Update `kafka_socket_addr()` to use `kafka` field directly

**Testing**:
```bash
cargo test --lib -p chronik-config
```

---

### Day 2: CLI Structure Redesign

**File**: `crates/chronik-server/src/main.rs`

**Changes**:

1. **Simplify top-level Cli struct** (replace lines 48-117):
```rust
#[derive(Parser, Debug, Clone)]
#[command(
    name = "chronik-server",
    about = "Chronik Stream - Kafka-compatible streaming platform",
    version,
    author,
    long_about = "A high-performance, Kafka-compatible streaming platform"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Data directory for storage
    #[arg(short = 'd', long, env = "CHRONIK_DATA_DIR", default_value = "./data", global = true)]
    data_dir: PathBuf,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, env = "RUST_LOG", default_value = "info", global = true)]
    log_level: String,
}
```

2. **Create new Commands enum** (replace lines 119-242):
```rust
#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Start a Chronik server (single-node or cluster mode)
    #[command(about = "Start server (auto-detects mode from config)")]
    Start {
        /// Cluster config file (TOML)
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: Option<PathBuf>,

        /// Bind address for all services (default: 0.0.0.0)
        #[arg(long, env = "CHRONIK_BIND", default_value = "0.0.0.0")]
        bind: String,

        /// Advertised address for clients (overrides config)
        #[arg(long, env = "CHRONIK_ADVERTISE")]
        advertise: Option<String>,

        /// Node ID (overrides config file)
        #[arg(long, env = "CHRONIK_NODE_ID")]
        node_id: Option<u64>,
    },

    /// Manage cluster membership
    #[command(about = "Cluster management commands")]
    Cluster {
        #[command(subcommand)]
        action: ClusterAction,
    },

    /// WAL compaction management
    #[command(about = "Manage WAL compaction")]
    Compact {
        #[command(subcommand)]
        action: CompactAction,
    },

    /// Show version and build information
    #[command(about = "Display version and build information")]
    Version,
}

#[derive(Subcommand, Debug, Clone)]
enum ClusterAction {
    /// Show cluster status
    #[command(about = "Display cluster status")]
    Status {
        /// Cluster config file
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: PathBuf,
    },

    /// Add a node to the cluster (zero-downtime)
    #[command(about = "Add a new node to the cluster")]
    AddNode {
        /// Node ID to add
        node_id: u64,

        /// Kafka address (host:port)
        #[arg(long)]
        kafka: String,

        /// WAL address (host:port)
        #[arg(long)]
        wal: String,

        /// Raft address (host:port)
        #[arg(long)]
        raft: String,

        /// Cluster config file
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: PathBuf,
    },

    /// Remove a node from the cluster (zero-downtime)
    #[command(about = "Remove a node from the cluster")]
    RemoveNode {
        /// Node ID to remove
        node_id: u64,

        /// Force removal (don't wait for partition reassignment)
        #[arg(long, default_value = "false")]
        force: bool,

        /// Cluster config file
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: PathBuf,
    },

    /// Trigger partition rebalancing
    #[command(about = "Manually trigger partition rebalancing")]
    Rebalance {
        /// Cluster config file
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: PathBuf,
    },
}
```

3. **Update main() function** (replace lines 435-586):
```rust
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    std::env::set_var("RUST_LOG", &cli.log_level);
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Log version information on startup
    info!("Chronik Server v{}", env!("CARGO_PKG_VERSION"));
    info!("Build features: search={}, backup={}, dynamic-config={}",
        cfg!(feature = "search"),
        cfg!(feature = "backup"),
        cfg!(feature = "dynamic-config")
    );

    match cli.command {
        Commands::Version => {
            println!("Chronik Server v{}", env!("CARGO_PKG_VERSION"));
            println!("Build features:");
            #[cfg(feature = "search")]
            println!("  - Search: enabled");
            #[cfg(feature = "backup")]
            println!("  - Backup: enabled");
            #[cfg(feature = "dynamic-config")]
            println!("  - Dynamic Config: enabled");
            Ok(())
        }

        Commands::Start { config, bind, advertise, node_id } => {
            run_start_command(&cli, config, bind, advertise, node_id).await
        }

        Commands::Cluster { action } => {
            handle_cluster_command(&cli, action).await
        }

        Commands::Compact { action } => {
            handle_compaction_command(&cli, action).await
        }
    }
}
```

4. **Create run_start_command()** (new function):
```rust
async fn run_start_command(
    cli: &Cli,
    config_path: Option<PathBuf>,
    bind: String,
    advertise: Option<String>,
    node_id_override: Option<u64>,
) -> Result<()> {
    // Load cluster config (from file or env)
    let cluster_config = if let Some(path) = config_path {
        Some(load_cluster_config_from_file(&path, node_id_override)?)
    } else {
        load_cluster_config_from_env(node_id_override)?
    };

    if let Some(config) = cluster_config {
        info!("Starting in CLUSTER mode (node_id={})", config.node_id);
        run_cluster_mode(cli, config, bind, advertise).await
    } else {
        info!("Starting in SINGLE-NODE mode");
        run_single_node_mode(cli, bind, advertise).await
    }
}
```

5. **Create helper functions**:
```rust
fn load_cluster_config_from_file(path: &PathBuf, node_id_override: Option<u64>) -> Result<ClusterConfig> {
    info!("Loading cluster configuration from file: {}", path.display());
    let contents = std::fs::read_to_string(path)?;
    let mut config: ClusterConfig = toml::from_str(&contents)?;

    if let Some(node_id) = node_id_override {
        info!("Overriding node_id from CLI: {}", node_id);
        config.node_id = node_id;
    }

    config.validate_config().map_err(|e| {
        anyhow::anyhow!("Invalid cluster configuration: {}", e)
    })?;

    Ok(config)
}

fn load_cluster_config_from_env(node_id_override: Option<u64>) -> Result<Option<ClusterConfig>> {
    if let Ok(Some(mut config)) = ClusterConfig::from_env() {
        if let Some(node_id) = node_id_override {
            config.node_id = node_id;
        }
        Ok(Some(config))
    } else {
        Ok(None)
    }
}

async fn run_cluster_mode(
    cli: &Cli,
    config: ClusterConfig,
    bind: String,
    advertise: Option<String>,
) -> Result<()> {
    // TODO: Implement cluster mode startup
    // This will replace run_raft_cluster() logic
    unimplemented!("Cluster mode startup - Phase 1.5")
}

async fn run_single_node_mode(
    cli: &Cli,
    bind: String,
    advertise: Option<String>,
) -> Result<()> {
    // Simplified version of current run_standalone_server()
    // No port auto-derivation, no cluster config complexity

    let (advertised_host, advertised_port) = parse_advertise_addr(
        advertise.as_deref(),
        &bind,
        9092, // Default Kafka port
    )?;

    let config = IntegratedServerConfig {
        node_id: 1,
        advertised_host,
        advertised_port,
        data_dir: cli.data_dir.to_string_lossy().to_string(),
        enable_indexing: cfg!(feature = "search"),
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,
        replication_factor: 1,
        enable_wal_indexing: true,
        wal_indexing_interval_secs: 30,
        object_store_config: parse_object_store_config_from_env(),
        enable_metadata_dr: true,
        metadata_upload_interval_secs: 60,
        cluster_config: None,
    };

    let server = IntegratedKafkaServer::new(config, None).await?;

    // Initialize monitoring
    let _metrics_registry = init_monitoring(
        "chronik-server",
        13092, // Default metrics port
        None,
    ).await?;

    let kafka_addr = format!("{}:9092", bind);
    info!("Kafka protocol listening on {}", kafka_addr);

    // Start server with signal handling
    let server_task = tokio::spawn({
        let server = server.clone();
        let kafka_addr = kafka_addr.clone();
        async move { server.run(&kafka_addr).await }
    });

    tokio::select! {
        result = server_task => {
            match result {
                Ok(Ok(_)) => info!("Server task completed normally"),
                Ok(Err(e)) => error!("Server task failed: {}", e),
                Err(e) => error!("Server task panicked: {}", e),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down...");
        }
    }

    server.shutdown().await?;
    Ok(())
}
```

6. **Create handle_cluster_command()** (stub for Phase 3):
```rust
async fn handle_cluster_command(cli: &Cli, action: ClusterAction) -> Result<()> {
    match action {
        ClusterAction::Status { config } => {
            println!("Cluster status - TODO: Phase 3");
            Ok(())
        }
        ClusterAction::AddNode { node_id, kafka, wal, raft, config } => {
            println!("Add node {} - TODO: Phase 3", node_id);
            Ok(())
        }
        ClusterAction::RemoveNode { node_id, force, config } => {
            println!("Remove node {} - TODO: Phase 3", node_id);
            Ok(())
        }
        ClusterAction::Rebalance { config } => {
            println!("Rebalance partitions - TODO: Phase 3");
            Ok(())
        }
    }
}
```

**Testing**:
```bash
cargo check --bin chronik-server
cargo build --bin chronik-server
```

---

### Day 3: Example Config Files & Documentation

**Files to create**:

1. **examples/cluster-3node.toml**:
```toml
# 3-node cluster configuration example
# Copy this file and customize for your deployment

node_id = 1  # CHANGE THIS on each node (1, 2, 3)

replication_factor = 3
min_insync_replicas = 2

# This node's addresses
[node.addresses]
kafka = "0.0.0.0:9092"
wal = "0.0.0.0:9291"
raft = "0.0.0.0:5001"
metrics = "0.0.0.0:13092"
search = "0.0.0.0:6092"

[node.advertise]
kafka = "node1.example.com:9092"
wal = "node1.example.com:9291"
raft = "node1.example.com:5001"

# All cluster peers (including this node)
[[peers]]
id = 1
kafka = "node1.example.com:9092"
wal = "node1.example.com:9291"
raft = "node1.example.com:5001"

[[peers]]
id = 2
kafka = "node2.example.com:9092"
wal = "node2.example.com:9291"
raft = "node2.example.com:5001"

[[peers]]
id = 3
kafka = "node3.example.com:9092"
wal = "node3.example.com:9291"
raft = "node3.example.com:5001"
```

2. **examples/cluster-local-3node.toml** (for local testing):
```toml
# Local 3-node cluster (all on localhost)
# Use this for development/testing

node_id = 1  # CHANGE THIS: 1, 2, or 3

replication_factor = 3
min_insync_replicas = 2

[node.addresses]
kafka = "0.0.0.0:9092"   # Node 1: 9092, Node 2: 9093, Node 3: 9094
wal = "0.0.0.0:9291"     # Node 1: 9291, Node 2: 9292, Node 3: 9293
raft = "0.0.0.0:5001"    # Node 1: 5001, Node 2: 5002, Node 3: 5003
metrics = "0.0.0.0:13092"
search = "0.0.0.0:6092"

[node.advertise]
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 1
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 2
kafka = "localhost:9093"
wal = "localhost:9292"
raft = "localhost:5002"

[[peers]]
id = 3
kafka = "localhost:9094"
wal = "localhost:9293"
raft = "localhost:5003"
```

3. **Update CLAUDE.md** (replace cluster sections):
```markdown
## Starting Chronik

### Single-Node Mode (Default)
```bash
# Simplest startup
cargo run --bin chronik-server start

# With custom data directory
cargo run --bin chronik-server start --data-dir ./my-data

# Custom advertised address (for Docker/remote clients)
cargo run --bin chronik-server start --advertise my-hostname.com:9092
```

### 3-Node Cluster Mode
```bash
# Build server
cargo build --release --bin chronik-server

# Node 1
./target/release/chronik-server start --config examples/cluster-local-3node.toml

# Node 2 (in separate terminal)
./target/release/chronik-server start --config examples/cluster-local-3node.toml --node-id 2

# Node 3 (in separate terminal)
./target/release/chronik-server start --config examples/cluster-local-3node.toml --node-id 3
```

### Environment Variable Config (Docker/K8s)
```bash
# Set via env vars instead of config file
export CHRONIK_NODE_ID=1
export CHRONIK_CLUSTER_PEERS="node1:9092:9291:5001,node2:9092:9291:5001,node3:9092:9291:5001"
export CHRONIK_REPLICATION_FACTOR=3
export CHRONIK_MIN_INSYNC_REPLICAS=2

cargo run --bin chronik-server start
```
```

**Testing**:
- Validate TOML files: `toml-test examples/*.toml` (if toml-test installed)
- Manual review of examples

---

### Day 4: Integration & Backward Compatibility

**Goals**:
- Ensure old code paths still work (with deprecation warnings)
- Update integrated_server.rs to accept new config format
- Create migration guide

**Changes**:

1. **Add deprecation warnings** (main.rs):
```rust
// In main() after logging setup
if std::env::var("CHRONIK_KAFKA_PORT").is_ok() {
    warn!("CHRONIK_KAFKA_PORT is deprecated. Use cluster config file instead.");
}
if std::env::var("CHRONIK_REPLICATION_FOLLOWERS").is_ok() {
    warn!("CHRONIK_REPLICATION_FOLLOWERS is deprecated. WAL replication now auto-discovers from cluster membership.");
}
if std::env::var("CHRONIK_WAL_RECEIVER_ADDR").is_ok() {
    warn!("CHRONIK_WAL_RECEIVER_ADDR is deprecated. Use cluster config file with 'wal' address instead.");
}
```

2. **Create migration guide** (docs/MIGRATION_v2.5.0.md):
```markdown
# Migration Guide: v2.4.x → v2.5.0

## Breaking Changes

### CLI Changes

**REMOVED FLAGS**:
- `--kafka-port`, `--admin-port`, `--metrics-port`, `--search-port` → Use config file
- `--raft-addr`, `--peers`, `--bootstrap` → Use config file
- `--cluster-config` → Renamed to `--config`
- `--bind-addr` → Renamed to `--bind`
- `--advertised-addr`, `--advertised-port` → Merged into `--advertise`

**REMOVED SUBCOMMANDS**:
- `standalone` → Use `start` (auto-detects mode)
- `raft-cluster` → Use `start --config cluster.toml`
- `ingest`, `search`, `all` → Not implemented, removed

### Environment Variables

**REMOVED**:
- `CHRONIK_KAFKA_PORT` → Use config file
- `CHRONIK_REPLICATION_FOLLOWERS` → Auto-discovered from cluster
- `CHRONIK_WAL_RECEIVER_ADDR` → Use config file
- `CHRONIK_WAL_RECEIVER_PORT` → Use config file

**NEW**:
- `CHRONIK_CONFIG` - Path to cluster config file
- `CHRONIK_BIND` - Bind address (replaces CHRONIK_BIND_ADDR)
- `CHRONIK_ADVERTISE` - Advertised address

### Migration Examples

**Before (v2.4.x)**:
```bash
# Single node
./chronik-server --advertised-addr localhost standalone

# Cluster
./chronik-server --node-id 1 --kafka-port 9092 raft-cluster \
  --raft-addr 0.0.0.0:5001 \
  --peers "2@node2:5001,3@node3:5001" \
  --bootstrap
```

**After (v2.5.0)**:
```bash
# Single node
./chronik-server start --advertise localhost:9092

# Cluster
./chronik-server start --config cluster.toml
```
```

**Testing**:
- Check deprecation warnings appear correctly
- Verify old env vars are ignored (with warnings)

---

### Day 5: Testing & Documentation

**Test Plan**:

1. **Unit Tests**:
```bash
# Config parsing
cargo test --lib -p chronik-config

# CLI parsing
cargo test --lib --bin chronik-server
```

2. **Integration Tests**:
```bash
# Single-node startup
cargo run --bin chronik-server start
# Verify: localhost:9092 accessible, metrics on 13092

# Cluster startup (3 terminals)
cargo run --bin chronik-server start --config examples/cluster-local-3node.toml
cargo run --bin chronik-server start --config examples/cluster-local-3node.toml --node-id 2
cargo run --bin chronik-server start --config examples/cluster-local-3node.toml --node-id 3
# Verify: All nodes start, connect to each other
```

3. **Kafka Client Tests**:
```bash
# Single-node produce/consume
kafka-console-producer --bootstrap-server localhost:9092 --topic test
kafka-console-consumer --bootstrap-server localhost:9092 --topic test --from-beginning

# Cluster produce/consume (should work on any node)
kafka-console-producer --bootstrap-server localhost:9092,localhost:9093,localhost:9094 --topic test
kafka-console-consumer --bootstrap-server localhost:9092,localhost:9093,localhost:9094 --topic test --from-beginning
```

**Documentation Updates**:

1. **Update CLAUDE.md** (already done in Day 3)
2. **Update README.md**:
   - Replace old CLI examples
   - Add cluster config examples
   - Update quick start guide

3. **Create docs/CLUSTER_QUICKSTART.md**:
```markdown
# Cluster Quick Start

## Local 3-Node Cluster

1. Build:
```bash
cargo build --release --bin chronik-server
```

2. Start nodes:
```bash
# Terminal 1 (Node 1)
./target/release/chronik-server start --config examples/cluster-local-3node.toml

# Terminal 2 (Node 2)
./target/release/chronik-server start --config examples/cluster-local-3node.toml --node-id 2

# Terminal 3 (Node 3)
./target/release/chronik-server start --config examples/cluster-local-3node.toml --node-id 3
```

3. Test:
```bash
kafka-console-producer --bootstrap-server localhost:9092 --topic test
```
```

**Acceptance Criteria**:
- [ ] Single-node mode starts without config file
- [ ] Cluster mode starts from config file
- [ ] Cluster mode starts from env vars
- [ ] Deprecation warnings appear for old env vars
- [ ] All unit tests pass
- [ ] Kafka clients can produce/consume
- [ ] Documentation is clear and accurate

---

## Success Metrics

**Before (v2.4.x)**:
- 16 CLI flags
- 5 subcommands
- 6+ env vars to configure manually
- Port conflicts easy to hit

**After (v2.5.0)**:
- 2 CLI flags (--config, --bind)
- 1 subcommand for most users (`start`)
- 0 manual env vars (optional for overrides)
- All ports in config file, no conflicts

**Migration Impact**:
- Breaking changes justified by massive UX improvement
- Clear migration guide
- Deprecation warnings for smooth transition
- No backward compatibility (userbase = just us)

---

## Phase 2 Preview

After Phase 1 is complete and tested:
- Phase 2: Automatic WAL replication discovery (remove manual follower config)
- Phase 3: Zero-downtime node add/remove (cluster management commands)
- Phase 4: Testing & cleanup

---

## Rollout Plan

1. Implement on `feat/v2.5.0-kafka-cluster` branch
2. Test thoroughly with local 3-node cluster
3. Update all docs
4. Merge to main
5. Tag v2.5.0-beta1
6. Deploy to staging
7. Final testing
8. Tag v2.5.0
