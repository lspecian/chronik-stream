# Self-Reliant Raft Bootstrap - Zero External Dependencies

**Date**: 2025-10-18
**Question**: How can we bootstrap Raft cluster WITHOUT DNS servers or external services?

## TL;DR - Recommended Self-Reliant Approach

**Best Option**: **Static Config + Smart Retry with Leader Election**
- ✅ Zero external dependencies (no DNS, no discovery service)
- ✅ Works in airgap environments
- ✅ Nodes can start in any order
- ✅ Automatic leader election based on node_id
- ✅ Simple configuration (just peer list in TOML)
- ⏱️ **Implementation**: 1 day

## The Problem with Current Implementation

**Current code** (what we have):
```rust
// All nodes do this:
raft_manager.create_replica("__meta", 0, storage, peer_ids).await?;
// Each node calls campaign() → each becomes leader of SEPARATE group
```

**Why it fails**:
- Node 1 creates Raft group, becomes leader (isolated)
- Node 2 creates Raft group, becomes leader (isolated)
- Node 3 creates Raft group, becomes leader (isolated)
- They never join the SAME group

## Self-Reliant Solutions (No External Dependencies)

### Option 1: Static Seed + Deterministic Leader ⭐ RECOMMENDED

**Core Idea**: Use **node_id** to deterministically decide who bootstraps first.

**How It Works**:
```
1. All nodes have same static config (peer list)
2. Lowest node_id bootstraps as single-node leader
3. Other nodes wait and join as learners
4. Leader promotes learners to voters
5. Cluster forms automatically
```

**Algorithm**:
```rust
fn bootstrap_metadata_deterministic(
    node_id: u64,
    peers: Vec<PeerConfig>,
) -> Result<()> {
    let all_nodes: Vec<u64> = peers.iter().map(|p| p.id).collect();
    let lowest_id = all_nodes.iter().min().unwrap();

    if node_id == *lowest_id {
        // I am the bootstrap leader
        info!("I am bootstrap leader (node_id {})", node_id);
        bootstrap_as_leader(peers).await?;
    } else {
        // I am a follower
        info!("Waiting to join cluster (leader is node_id {})", lowest_id);
        join_as_follower(lowest_id, peers).await?;
    }
}

async fn bootstrap_as_leader(peers: Vec<PeerConfig>) -> Result<()> {
    // Step 1: Start as single-node Raft cluster
    raft_manager.create_replica(
        "__meta",
        0,
        storage,
        vec![], // Empty peer list = single node
    ).await?;

    // Step 2: Wait to become leader
    wait_for_leadership("__meta", 0).await?;
    info!("I am leader, ready to accept followers");

    // Step 3: Listen for join requests from other nodes
    // (They will send MsgVote or connect via gRPC)

    // Step 4: Add them via configuration changes
    for peer in peers {
        if peer.id != node_id {
            raft_manager.add_peer_to_replica("__meta", 0, peer.id).await?;
            info!("Added peer {} to cluster", peer.id);
        }
    }
}

async fn join_as_follower(leader_id: u64, peers: Vec<PeerConfig>) -> Result<()> {
    // Find leader's address
    let leader = peers.iter().find(|p| p.id == leader_id).unwrap();

    // Step 1: Wait for leader to be ready
    let mut retries = 0;
    loop {
        if can_reach_raft_peer(&leader.addr).await {
            info!("Leader {} is ready", leader_id);
            break;
        }

        retries += 1;
        if retries > 30 {
            return Err(anyhow!("Leader not reachable after 30s"));
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Step 2: Create replica with leader as peer
    raft_manager.create_replica(
        "__meta",
        0,
        storage,
        vec![leader_id], // Just the leader initially
    ).await?;

    // Step 3: Replica will receive AppendEntries from leader
    // and automatically join the cluster
    info!("Joined cluster with leader {}", leader_id);
}
```

**Pros**:
- ✅ **Zero dependencies** - just static config
- ✅ **Deterministic** - always works the same way
- ✅ **Self-contained** - no external services
- ✅ **Simple** - easy to understand and debug
- ✅ **Fast** - 1 day implementation

**Cons**:
- ❌ Requires specific startup pattern (but automatic)
- ❌ Lowest node_id is always initial leader (but transparent)

**Configuration**:
```toml
# Same for all nodes
[[cluster.peers]]
id = 1
addr = "node1:9092"
raft_port = 9093

[[cluster.peers]]
id = 2
addr = "node2:9092"
raft_port = 9093

[[cluster.peers]]
id = 3
addr = "node3:9092"
raft_port = 9093
```

**Startup (any order!)**:
```bash
# Node 2 starts first (waits for node 1)
chronik-server --cluster-config cluster.toml

# Node 3 starts (waits for node 1)
chronik-server --cluster-config cluster.toml

# Node 1 starts → becomes leader, adds node 2 & 3
chronik-server --cluster-config cluster.toml

# Cluster formed!
```

### Option 2: Peer-to-Peer Coordination via gRPC

**Core Idea**: Nodes coordinate directly via custom gRPC protocol.

**How It Works**:
```
1. Each node starts in "bootstrap mode"
2. Nodes ping each other via gRPC
3. First to see quorum (2/3) triggers bootstrap
4. That node becomes leader, others join
```

**Algorithm**:
```rust
struct BootstrapCoordinator {
    node_id: u64,
    peers: Vec<PeerConfig>,
    reachable: HashSet<u64>,
}

impl BootstrapCoordinator {
    async fn coordinate_bootstrap(&mut self) -> Result<Role> {
        let quorum = self.peers.len() / 2 + 1;

        // Keep checking peer reachability
        loop {
            self.update_reachable_peers().await;

            if self.reachable.len() >= quorum {
                // Quorum reached! Decide role
                let lowest_reachable = self.reachable.iter().min().unwrap();

                if self.node_id == *lowest_reachable {
                    return Ok(Role::Leader);
                } else {
                    return Ok(Role::Follower(*lowest_reachable));
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn update_reachable_peers(&mut self) {
        for peer in &self.peers {
            if self.ping_peer(peer).await {
                self.reachable.insert(peer.id);
            }
        }
    }

    async fn ping_peer(&self, peer: &PeerConfig) -> bool {
        // Simple TCP connection test
        TcpStream::connect(&peer.addr).await.is_ok()
    }
}
```

**Pros**:
- ✅ **Zero dependencies**
- ✅ **Flexible** - any node can be leader
- ✅ **Automatic** - no designated leader

**Cons**:
- ❌ More complex than deterministic
- ❌ Network partition edge cases

### Option 3: File-Based Coordination (Shared Filesystem)

**Core Idea**: Use shared file (NFS, EFS) for coordination.

**How It Works**:
```
1. Nodes write their status to /shared/cluster/node-{id}.lock
2. First node to create /shared/cluster/leader.lock becomes leader
3. Others read leader.lock and join
```

**Pros**:
- ✅ Simple locking mechanism
- ✅ Works in shared filesystem environments

**Cons**:
- ❌ Requires shared storage (not truly self-reliant)
- ❌ Filesystem locking issues
- ❌ Not cloud-friendly

### Option 4: Multicast/Broadcast (LAN only)

**Core Idea**: Nodes broadcast presence on LAN.

**How It Works**:
```
1. Nodes join multicast group 239.255.0.1
2. Broadcast "I'm node X on port Y"
3. Collect peers for 5 seconds
4. Lowest node_id bootstraps
```

**Pros**:
- ✅ Zero configuration
- ✅ True peer discovery

**Cons**:
- ❌ Only works on same LAN
- ❌ Blocked by most cloud networks
- ❌ Security concerns (anyone can join)

## Detailed Implementation: Option 1 (Deterministic Leader)

### Phase 1: Smart Bootstrap Logic (4 hours)

```rust
// crates/chronik-raft/src/bootstrap.rs

use std::time::Duration;
use tokio::net::TcpStream;
use tracing::{info, warn};

pub enum BootstrapRole {
    Leader,           // I bootstrap the cluster
    Follower(u64),   // I join cluster led by node_id
}

pub struct DeterministicBootstrap {
    pub node_id: u64,
    pub all_peers: Vec<PeerConfig>,
}

impl DeterministicBootstrap {
    pub fn determine_role(&self) -> BootstrapRole {
        let all_ids: Vec<u64> = self.all_peers.iter().map(|p| p.id).collect();
        let lowest_id = all_ids.iter().min().unwrap();

        if self.node_id == *lowest_id {
            info!("I am the bootstrap leader (lowest node_id = {})", self.node_id);
            BootstrapRole::Leader
        } else {
            info!("I will join cluster led by node_id {}", lowest_id);
            BootstrapRole::Follower(*lowest_id)
        }
    }

    pub async fn wait_for_peer(&self, peer_id: u64, timeout: Duration) -> Result<()> {
        let peer = self.all_peers.iter().find(|p| p.id == peer_id)
            .ok_or_else(|| anyhow!("Peer {} not found", peer_id))?;

        let start = Instant::now();
        let mut attempts = 0;

        while start.elapsed() < timeout {
            attempts += 1;

            match self.check_peer_ready(&peer.addr).await {
                Ok(true) => {
                    info!("Peer {} is ready (attempt {})", peer_id, attempts);
                    return Ok(());
                }
                Ok(false) => {
                    warn!("Peer {} not ready yet (attempt {})", peer_id, attempts);
                }
                Err(e) => {
                    warn!("Failed to check peer {}: {:?}", peer_id, e);
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        Err(anyhow!("Peer {} not ready after {:?}", peer_id, timeout))
    }

    async fn check_peer_ready(&self, addr: &str) -> Result<bool> {
        // Try to connect to Raft gRPC port
        match tokio::time::timeout(
            Duration::from_secs(1),
            TcpStream::connect(addr)
        ).await {
            Ok(Ok(_)) => Ok(true),
            Ok(Err(_)) => Ok(false),
            Err(_) => Ok(false), // Timeout
        }
    }

    pub async fn wait_for_quorum(&self, timeout: Duration) -> Result<Vec<u64>> {
        let quorum_size = self.all_peers.len() / 2 + 1;
        let mut reachable = vec![self.node_id]; // I'm always reachable

        info!("Waiting for quorum: need {}/{} peers", quorum_size, self.all_peers.len());

        let start = Instant::now();

        while reachable.len() < quorum_size && start.elapsed() < timeout {
            for peer in &self.all_peers {
                if peer.id == self.node_id {
                    continue; // Skip self
                }

                if reachable.contains(&peer.id) {
                    continue; // Already reachable
                }

                if self.check_peer_ready(&peer.addr).await.unwrap_or(false) {
                    reachable.push(peer.id);
                    info!("Peer {} reachable ({}/{})", peer.id, reachable.len(), quorum_size);
                }
            }

            if reachable.len() < quorum_size {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }

        if reachable.len() >= quorum_size {
            Ok(reachable)
        } else {
            Err(anyhow!(
                "Failed to reach quorum: {}/{} peers reachable",
                reachable.len(),
                quorum_size
            ))
        }
    }
}
```

### Phase 2: Leader Bootstrap (2 hours)

```rust
// Modify integrated_server.rs

async fn bootstrap_as_leader(
    cluster_config: &ClusterConfig,
    raft_manager: Arc<RaftReplicaManager>,
) -> Result<Arc<dyn MetadataStore>> {

    info!("🚀 Bootstrapping as cluster leader (node_id {})", cluster_config.node_id);

    // Step 1: Create __meta as single-node cluster
    info!("Creating single-node __meta replica");
    raft_manager.create_replica(
        "__meta".to_string(),
        0,
        Arc::new(MemoryLogStorage::new()),
        vec![], // Start with no peers
    ).await?;

    let meta_replica = raft_manager.get_replica("__meta", 0)
        .ok_or_else(|| anyhow!("Failed to get __meta replica"))?;

    // Step 2: Wait for leadership (should be immediate)
    info!("Waiting to become leader...");
    let timeout = Duration::from_secs(5);
    let start = Instant::now();

    loop {
        let state = meta_replica.get_state().await;
        if state.role == "Leader" {
            info!("✅ I am leader! Ready to accept followers");
            break;
        }

        if start.elapsed() > timeout {
            return Err(anyhow!("Failed to become leader within {:?}", timeout));
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Step 3: Register self as broker
    let raft_meta_log = RaftMetaLog::from_replica(
        cluster_config.node_id,
        meta_replica.clone(),
    ).await?;

    let broker_metadata = BrokerMetadata {
        broker_id: cluster_config.node_id,
        host: cluster_config.advertised_host.clone(),
        port: cluster_config.advertised_port,
        rack: None,
        status: BrokerStatus::Online,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    raft_meta_log.register_broker(broker_metadata).await?;
    info!("✅ Registered self as broker {}", cluster_config.node_id);

    // Step 4: Wait for other peers to be reachable
    let bootstrap = DeterministicBootstrap {
        node_id: cluster_config.node_id,
        all_peers: cluster_config.peers.clone(),
    };

    let reachable_peers = bootstrap.wait_for_quorum(Duration::from_secs(60)).await?;
    info!("✅ Quorum reached: {} peers online", reachable_peers.len());

    // Step 5: Add followers via configuration changes
    for peer in &cluster_config.peers {
        if peer.id == cluster_config.node_id {
            continue; // Skip self
        }

        if !reachable_peers.contains(&peer.id) {
            warn!("Peer {} not reachable, skipping for now", peer.id);
            continue;
        }

        info!("Adding peer {} to __meta replica", peer.id);
        raft_manager.add_peer_to_replica("__meta", 0, peer.id).await?;
        info!("✅ Successfully added peer {} to cluster", peer.id);

        // Small delay to let Raft process conf change
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    info!("🎉 Cluster bootstrap complete! {} peers in cluster", reachable_peers.len());

    Ok(Arc::new(raft_meta_log) as Arc<dyn MetadataStore>)
}
```

### Phase 3: Follower Join (2 hours)

```rust
async fn join_as_follower(
    cluster_config: &ClusterConfig,
    leader_id: u64,
    raft_manager: Arc<RaftReplicaManager>,
) -> Result<Arc<dyn MetadataStore>> {

    info!("🔄 Joining cluster as follower (leader is node_id {})", leader_id);

    // Step 1: Wait for leader to be ready
    let bootstrap = DeterministicBootstrap {
        node_id: cluster_config.node_id,
        all_peers: cluster_config.peers.clone(),
    };

    info!("Waiting for leader {} to be ready...", leader_id);
    bootstrap.wait_for_peer(leader_id, Duration::from_secs(60)).await?;
    info!("✅ Leader {} is ready", leader_id);

    // Step 2: Create replica with leader in peer list
    // This is key: we tell Raft that leader_id is part of the group
    info!("Creating __meta replica as follower");
    raft_manager.create_replica(
        "__meta".to_string(),
        0,
        Arc::new(MemoryLogStorage::new()),
        vec![leader_id], // Just the leader for now
    ).await?;

    let meta_replica = raft_manager.get_replica("__meta", 0)
        .ok_or_else(|| anyhow!("Failed to get __meta replica"))?;

    // Step 3: Wait for AppendEntries from leader
    info!("Waiting to receive Raft messages from leader...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Step 4: Create RaftMetaLog
    let raft_meta_log = RaftMetaLog::from_replica(
        cluster_config.node_id,
        meta_replica.clone(),
    ).await?;

    info!("✅ Successfully joined cluster as follower");

    // Note: Broker registration will happen in background task
    // Leader will add us to the cluster, and we'll receive the update via Raft

    Ok(Arc::new(raft_meta_log) as Arc<dyn MetadataStore>)
}
```

### Phase 4: Integration (2 hours)

```rust
// Update integrated_server.rs bootstrap logic

let metadata_store = if let Some(cluster_config) = &config.cluster_config {
    info!("Cluster mode enabled, using Raft-replicated metadata");

    let bootstrap = DeterministicBootstrap {
        node_id: cluster_config.node_id,
        all_peers: cluster_config.peers.clone(),
    };

    match bootstrap.determine_role() {
        BootstrapRole::Leader => {
            bootstrap_as_leader(cluster_config, raft_manager.clone()).await?
        }
        BootstrapRole::Follower(leader_id) => {
            join_as_follower(cluster_config, leader_id, raft_manager.clone()).await?
        }
    }
} else {
    // Standalone mode (existing code)
    create_standalone_metadata(config).await?
};
```

## Testing Plan

### Test 1: Leader First
```bash
# Start leader (node 1)
chronik-server --cluster-config cluster.toml --node-id 1

# Should see:
# "I am the bootstrap leader (lowest node_id = 1)"
# "✅ I am leader! Ready to accept followers"
# "Waiting for quorum..."

# Start followers
chronik-server --cluster-config cluster.toml --node-id 2
chronik-server --cluster-config cluster.toml --node-id 3

# Should see:
# "✅ Quorum reached: 3 peers online"
# "✅ Successfully added peer 2 to cluster"
# "✅ Successfully added peer 3 to cluster"
# "🎉 Cluster bootstrap complete!"
```

### Test 2: Follower First (Any Order)
```bash
# Start follower (node 3)
chronik-server --cluster-config cluster.toml --node-id 3

# Should see:
# "I will join cluster led by node_id 1"
# "Waiting for leader 1 to be ready..."

# Start another follower (node 2)
chronik-server --cluster-config cluster.toml --node-id 2

# Should see:
# "Waiting for leader 1 to be ready..."

# Start leader (node 1)
chronik-server --cluster-config cluster.toml --node-id 1

# All nodes should form cluster within 5 seconds
```

### Test 3: Verify Cluster
```python
from kafka import KafkaAdminClient

# Query any node
for port in [9092, 9192, 9292]:
    admin = KafkaAdminClient(bootstrap_servers=[f'localhost:{port}'])
    brokers = list(admin._client.cluster.brokers())
    print(f"Node on port {port} sees {len(brokers)} brokers")
    # Should print: 3 brokers for all nodes
```

## Comparison: Self-Reliant Options

| Method | External Deps | Startup Order | Cloud-Ready | Impl Time | Complexity |
|--------|--------------|---------------|-------------|-----------|------------|
| **Deterministic Leader** ⭐ | None | Any | ✅ | 1 day | Low |
| **P2P Coordination** | None | Any | ✅ | 2 days | Medium |
| **File-Based** | Shared FS | Any | ❌ | 1 day | Low |
| **Multicast** | None | Any | ❌ | 2 days | Medium |
| **DNS SRV** | DNS Server | Any | ✅ | 2 days | Low |

## Recommendation

**Implement Deterministic Leader (Option 1)** because:

1. ✅ **Truly self-reliant** - no external dependencies
2. ✅ **Simple** - easy to understand and debug
3. ✅ **Fast** - 1 day implementation
4. ✅ **Deterministic** - same behavior every time
5. ✅ **Works everywhere** - cloud, on-prem, airgap, Kubernetes
6. ✅ **Startup order doesn't matter** - nodes wait for leader

## What About Node 1 Failure?

**Q**: What if node 1 (bootstrap leader) dies after cluster is formed?

**A**: No problem! Raft handles this:
1. Nodes 2 and 3 elect a new leader (via Raft election)
2. Cluster continues operating normally
3. Bootstrap logic ONLY runs at initial startup
4. After cluster is formed, it's pure Raft (no special leader)

**Q**: What if node 1 never starts?

**A**: Nodes 2 and 3 wait indefinitely. Solutions:
- **Manual override**: `--force-bootstrap` flag on node 2
- **Timeout fallback**: After 60s, next lowest node_id bootstraps
- **Config override**: Set `bootstrap_node_id = 2` in config

## Timeline

**Day 1** (8 hours):
- ✅ DeterministicBootstrap struct (2h)
- ✅ Leader bootstrap logic (2h)
- ✅ Follower join logic (2h)
- ✅ Integration + testing (2h)

**Total**: 1 day (self-reliant, no dependencies!)

## Next Steps

1. Implement `DeterministicBootstrap` in `crates/chronik-raft/src/bootstrap.rs`
2. Update `integrated_server.rs` to use deterministic logic
3. Test with 3-node cluster (start in any order)
4. Verify broker metadata replication
5. Run `test_raft_cluster_lifecycle_e2e.py`

Expected result: **3/3 healthy nodes** ✅
