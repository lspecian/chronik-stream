# Automatic Raft Bootstrap - Production Options Analysis

**Date**: 2025-10-18
**Question**: How do production systems automatically bootstrap Raft clusters?

## TL;DR - Recommended Approach for Chronik

**Best Option**: **DNS SRV Discovery** (etcd's production method)
- ✅ Zero manual coordination
- ✅ Works in cloud environments (AWS, GCP, Azure)
- ✅ Standard protocol (RFC 2782)
- ✅ Kubernetes-native
- ✅ No external dependencies
- ⏱️ **Implementation**: 1-2 days

## Production-Proven Automatic Bootstrap Methods

### 1. DNS SRV Discovery (etcd) ⭐ RECOMMENDED

**How It Works**:
```
1. Set DNS SRV records for cluster:
   _etcd-server._tcp.chronik.local.  IN SRV 0 0 9093 node1.chronik.local.
   _etcd-server._tcp.chronik.local.  IN SRV 0 0 9093 node2.chronik.local.
   _etcd-server._tcp.chronik.local.  IN SRV 0 0 9093 node3.chronik.local.

2. Start nodes with --discovery-srv flag:
   chronik-server --discovery-srv chronik.local standalone

3. Each node:
   - Queries DNS for SRV records
   - Discovers all peers (node1, node2, node3)
   - Calculates cluster size (3 nodes)
   - Elects leader when quorum present
   - Automatically forms cluster
```

**Pros**:
- ✅ **Zero manual steps** - nodes auto-discover each other
- ✅ **Standard protocol** - DNS SRV (RFC 2782)
- ✅ **Cloud-native** - works in AWS Route53, GCP DNS, Azure DNS
- ✅ **Kubernetes-ready** - K8s StatefulSets create DNS automatically
- ✅ **No external service** - just DNS (already required)
- ✅ **Production-proven** - etcd's recommended method

**Cons**:
- ❌ Requires DNS infrastructure
- ❌ DNS propagation delays (usually < 5s)

**Implementation Complexity**: 🟢 Low (1-2 days)

**Example etcd command**:
```bash
etcd --name infra0 \
  --discovery-srv example.com \
  --initial-advertise-peer-urls http://infra0.example.com:2380
```

**For Chronik**:
```bash
chronik-server --node-id 1 \
  --discovery-srv chronik.local \
  --advertised-addr node1.chronik.local \
  standalone
```

### 2. Gossip Protocol + Serf (Consul)

**How It Works**:
```
1. Start nodes with seed addresses:
   chronik-server --join node1:7946,node2:7946,node3:7946

2. Nodes use Serf gossip to:
   - Discover all cluster members
   - Detect failures
   - Propagate metadata

3. Once quorum detected (3 servers found):
   - Trigger Raft bootstrap
   - Elect leader
   - Form cluster
```

**Pros**:
- ✅ **Fast convergence** - gossip spreads in O(log N) time
- ✅ **Failure detection** - automatic health checks
- ✅ **Flexible** - nodes can join/leave dynamically
- ✅ **Partition-tolerant** - gossip eventually converges

**Cons**:
- ❌ Additional dependency (Serf library)
- ❌ More complex than DNS
- ❌ Extra network traffic (gossip messages)

**Implementation Complexity**: 🟡 Medium (3-5 days)

**Serf library**: https://github.com/hashicorp/serf (Go)
**Rust alternative**: https://github.com/zowens/memberlist-rs

**Example Consul command**:
```bash
consul agent -server -bootstrap-expect=3 \
  -retry-join node1 \
  -retry-join node2 \
  -retry-join node3
```

### 3. etcd Discovery Service

**How It Works**:
```
1. Create discovery URL from public service:
   curl https://discovery.etcd.io/new?size=3
   https://discovery.etcd.io/3e86b59982e49066c5d813af1c2e2579cbf573de

2. All nodes start with same URL:
   chronik-server --discovery https://discovery.etcd.io/3e86b...

3. Each node:
   - Registers itself with discovery service
   - Polls for other members
   - Once 3 members found, bootstraps cluster
```

**Pros**:
- ✅ **Zero infrastructure** - uses public service
- ✅ **Simple** - just one URL
- ✅ **Works anywhere** - no DNS required

**Cons**:
- ❌ **Requires internet** - not suitable for airgap
- ❌ **External dependency** - discovery.etcd.io must be up
- ❌ **Security concern** - cluster info exposed to third party
- ❌ **One-time use** - URL can't be reused

**Implementation Complexity**: 🟢 Low (1-2 days)

**Production use**: ❌ NOT recommended by etcd team
**Better for**: Development, testing, demos

### 4. Static Configuration with Retry (Baseline)

**How It Works**:
```
1. Configure all peers in TOML:
   [[cluster.peers]]
   id = 1
   addr = "node1:9092"

2. Start with retry logic:
   chronik-server --cluster-config cluster.toml --retry-join

3. Node keeps retrying to connect to peers until quorum found
```

**Pros**:
- ✅ **Simple** - no external dependencies
- ✅ **Predictable** - fully deterministic
- ✅ **Secure** - no external communication

**Cons**:
- ❌ **Manual configuration** - must know all IPs in advance
- ❌ **Not cloud-friendly** - IPs change in autoscaling
- ❌ **Startup order matters** - first node must be designated

**Implementation Complexity**: 🟢 Very Low (1 day)

**Current Status**: We have this, but need retry + quorum detection logic

### 5. Kubernetes Headless Service (K8s-specific)

**How It Works**:
```
1. Create StatefulSet with headless service:
   apiVersion: v1
   kind: Service
   metadata:
     name: chronik
   spec:
     clusterIP: None  # Headless

2. K8s creates DNS automatically:
   chronik-0.chronik.default.svc.cluster.local
   chronik-1.chronik.default.svc.cluster.local
   chronik-2.chronik.default.svc.cluster.local

3. Chronik uses DNS SRV discovery (method #1)
```

**Pros**:
- ✅ **Automatic** - K8s handles DNS
- ✅ **Standard** - uses DNS SRV
- ✅ **Cloud-native** - built for K8s

**Cons**:
- ❌ **K8s-only** - doesn't work outside Kubernetes

**Implementation**: Same as DNS SRV (#1)

## Comparison Matrix

| Method | Auto | Complexity | Prod-Ready | Cloud | On-Prem | K8s | Impl Time |
|--------|------|------------|------------|-------|---------|-----|-----------|
| **DNS SRV** | ✅ | Low | ✅ | ✅ | ✅ | ✅ | 1-2 days |
| **Gossip/Serf** | ✅ | Medium | ✅ | ✅ | ✅ | ✅ | 3-5 days |
| **etcd Discovery** | ✅ | Low | ❌ | ✅ | ❌ | ✅ | 1-2 days |
| **Static + Retry** | ⚠️ | Very Low | ⚠️ | ❌ | ✅ | ❌ | 1 day |
| **K8s Headless** | ✅ | Low | ✅ | ✅ | ❌ | ✅ | 1-2 days |

## Detailed Implementation: DNS SRV Discovery

### Why DNS SRV?

1. **Standard Protocol**: RFC 2782 (1996) - battle-tested
2. **Universal Support**: Every cloud provider has managed DNS
3. **Zero Dependencies**: No extra services required
4. **Kubernetes Native**: StatefulSets create SRV records automatically
5. **etcd's Choice**: Used by one of the most reliable distributed systems

### How DNS SRV Records Work

**SRV Record Format**:
```
_service._proto.name. TTL class SRV priority weight port target.
```

**Example for Chronik**:
```
_chronik-raft._tcp.chronik.local. 60 IN SRV 0 0 9093 node1.chronik.local.
_chronik-raft._tcp.chronik.local. 60 IN SRV 0 0 9093 node2.chronik.local.
_chronik-raft._tcp.chronik.local. 60 IN SRV 0 0 9093 node3.chronik.local.
```

**Plus A Records**:
```
node1.chronik.local. 60 IN A 10.0.1.10
node2.chronik.local. 60 IN A 10.0.1.11
node3.chronik.local. 60 IN A 10.0.1.12
```

### Implementation Plan

#### Phase 1: DNS Resolution (4 hours)
```rust
// crates/chronik-config/src/discovery.rs
use trust_dns_resolver::TokioAsyncResolver;

pub async fn discover_peers_from_dns(domain: &str) -> Result<Vec<PeerConfig>> {
    let resolver = TokioAsyncResolver::tokio_from_system_conf()?;

    // Query SRV records
    let srv_name = format!("_chronik-raft._tcp.{}", domain);
    let srv_records = resolver.srv_lookup(&srv_name).await?;

    let mut peers = Vec::new();
    for record in srv_records {
        let host = record.target().to_string().trim_end_matches('.').to_string();
        let port = record.port();

        // Resolve A record to IP
        let a_records = resolver.ipv4_lookup(&host).await?;
        for ip in a_records {
            peers.push(PeerConfig {
                id: calculate_peer_id(&host), // Hash hostname to ID
                addr: format!("{}:{}", ip, port),
                raft_port: port,
            });
        }
    }

    Ok(peers)
}
```

#### Phase 2: Bootstrap Coordinator (6 hours)
```rust
// crates/chronik-raft/src/bootstrap.rs
pub struct BootstrapCoordinator {
    node_id: u64,
    discovered_peers: Vec<PeerConfig>,
    quorum_size: usize,
}

impl BootstrapCoordinator {
    pub async fn wait_for_quorum(&mut self) -> Result<Vec<PeerConfig>> {
        let expected = self.discovered_peers.len();
        let quorum = expected / 2 + 1;

        info!("Waiting for quorum: {}/{} peers", quorum, expected);

        let mut reachable = vec![];
        let timeout = Duration::from_secs(30);
        let start = Instant::now();

        while reachable.len() < quorum && start.elapsed() < timeout {
            for peer in &self.discovered_peers {
                if self.can_reach_peer(peer).await {
                    if !reachable.contains(&peer.id) {
                        reachable.push(peer.id);
                        info!("Peer {} reachable ({}/{})", peer.id, reachable.len(), quorum);
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if reachable.len() >= quorum {
            Ok(self.discovered_peers.clone())
        } else {
            Err(anyhow!("Failed to reach quorum within {}s", timeout.as_secs()))
        }
    }

    async fn can_reach_peer(&self, peer: &PeerConfig) -> bool {
        // Try to connect to peer's Raft gRPC
        match TcpStream::connect(&peer.addr).await {
            Ok(_) => true,
            Err(_) => false,
        }
    }
}
```

#### Phase 3: Coordinated Bootstrap (6 hours)
```rust
// Modify integrated_server.rs
async fn bootstrap_metadata_with_discovery(
    cluster_config: &ClusterConfig,
    raft_manager: Arc<RaftReplicaManager>,
) -> Result<Arc<dyn MetadataStore>> {

    // Discover peers via DNS
    let discovered_peers = if let Some(dns_domain) = &cluster_config.discovery_srv {
        discover_peers_from_dns(dns_domain).await?
    } else {
        cluster_config.peers.clone()
    };

    info!("Discovered {} peers via DNS", discovered_peers.len());

    // Wait for quorum to be reachable
    let mut coordinator = BootstrapCoordinator::new(
        cluster_config.node_id,
        discovered_peers.clone(),
    );

    let ready_peers = coordinator.wait_for_quorum().await?;
    info!("Quorum reached! {} peers ready", ready_peers.len());

    // Determine if this node should bootstrap or join
    let am_i_first = cluster_config.node_id == ready_peers.iter().min().unwrap();

    let peer_ids: Vec<u64> = ready_peers.iter()
        .filter(|p| p.id != cluster_config.node_id)
        .map(|p| p.id)
        .collect();

    if am_i_first {
        info!("I am the bootstrap leader (lowest node_id)");

        // Start as single-node temporarily
        raft_manager.create_replica(
            "__meta".to_string(),
            0,
            Arc::new(MemoryLogStorage::new()),
            vec![], // Empty initially
        ).await?;

        // Wait for replica to become leader
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Add other peers via configuration changes
        for peer_id in peer_ids {
            raft_manager.add_peer_to_replica("__meta", 0, peer_id).await?;
            info!("Added peer {} to __meta", peer_id);
        }
    } else {
        info!("I am a follower, waiting for leader to add me");

        // Create replica with full peer list (will join existing group)
        raft_manager.create_replica(
            "__meta".to_string(),
            0,
            Arc::new(MemoryLogStorage::new()),
            peer_ids.clone(),
        ).await?;
    }

    let meta_replica = raft_manager.get_replica("__meta", 0)
        .ok_or_else(|| anyhow!("Failed to get __meta replica"))?;

    let raft_meta_log = RaftMetaLog::from_replica(
        cluster_config.node_id,
        meta_replica,
    ).await?;

    Ok(Arc::new(raft_meta_log) as Arc<dyn MetadataStore>)
}
```

#### Phase 4: Configuration (2 hours)
```toml
# cluster.toml (updated)
[cluster]
enabled = true
node_id = 1

# DNS-based discovery (automatic)
discovery_srv = "chronik.local"

# OR static configuration (manual)
# [[cluster.peers]]
# id = 1
# addr = "10.0.1.10:9092"
# raft_port = 9093
```

```bash
# Start with DNS discovery
chronik-server --cluster-config cluster.toml standalone

# OR environment variable
CHRONIK_DISCOVERY_SRV=chronik.local chronik-server standalone
```

#### Phase 5: DNS Setup Examples

**Local Testing (dnsmasq)**:
```bash
# /etc/dnsmasq.conf
srv-host=_chronik-raft._tcp.chronik.local,node1.chronik.local,9093,0,0
srv-host=_chronik-raft._tcp.chronik.local,node2.chronik.local,9093,0,0
srv-host=_chronik-raft._tcp.chronik.local,node3.chronik.local,9093,0,0

address=/node1.chronik.local/127.0.0.1
address=/node2.chronik.local/127.0.0.1
address=/node3.chronik.local/127.0.0.1
```

**AWS Route53**:
```bash
# Create SRV records
aws route53 change-resource-record-sets \
  --hosted-zone-id Z123456 \
  --change-batch '{
    "Changes": [{
      "Action": "CREATE",
      "ResourceRecordSet": {
        "Name": "_chronik-raft._tcp.chronik.internal",
        "Type": "SRV",
        "TTL": 60,
        "ResourceRecords": [
          {"Value": "0 0 9093 node1.chronik.internal"},
          {"Value": "0 0 9093 node2.chronik.internal"},
          {"Value": "0 0 9093 node3.chronik.internal"}
        ]
      }
    }]
  }'
```

**Kubernetes (automatic)**:
```yaml
# StatefulSet automatically creates DNS
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: chronik
spec:
  serviceName: chronik
  replicas: 3
  template:
    spec:
      containers:
      - name: chronik
        env:
        - name: CHRONIK_DISCOVERY_SRV
          value: "chronik.default.svc.cluster.local"
---
apiVersion: v1
kind: Service
metadata:
  name: chronik
spec:
  clusterIP: None  # Headless
  ports:
  - name: raft
    port: 9093
```

### Dependencies

```toml
# Cargo.toml
[dependencies]
trust-dns-resolver = "0.23"  # DNS resolver
```

### Testing

```rust
#[tokio::test]
async fn test_dns_discovery() {
    // Requires local DNS setup
    let peers = discover_peers_from_dns("chronik.local").await.unwrap();
    assert_eq!(peers.len(), 3);
}

#[tokio::test]
async fn test_bootstrap_coordinator() {
    let peers = vec![
        PeerConfig { id: 1, addr: "node1:9093".to_string(), raft_port: 9093 },
        PeerConfig { id: 2, addr: "node2:9093".to_string(), raft_port: 9093 },
        PeerConfig { id: 3, addr: "node3:9093".to_string(), raft_port: 9093 },
    ];

    let mut coordinator = BootstrapCoordinator::new(1, peers);
    let ready = coordinator.wait_for_quorum().await.unwrap();
    assert!(ready.len() >= 2); // Quorum
}
```

## Timeline & Effort

**DNS SRV Discovery (Recommended)**:
- Day 1 (8 hours):
  - ✅ DNS resolution logic (4h)
  - ✅ Bootstrap coordinator (4h)
- Day 2 (8 hours):
  - ✅ Coordinated bootstrap (6h)
  - ✅ Testing & debugging (2h)

**Total**: 2 days

**Alternative: Gossip/Serf**:
- Day 1-2: Integrate memberlist-rs
- Day 3: Gossip-based peer discovery
- Day 4: Raft bootstrap trigger
- Day 5: Testing

**Total**: 5 days

## Recommendation

**Implement DNS SRV Discovery** for these reasons:

1. ✅ **Fastest to implement** (2 days vs 5 days for gossip)
2. ✅ **etcd's production method** (battle-tested)
3. ✅ **Cloud-native** (works everywhere)
4. ✅ **Kubernetes-ready** (auto-configured)
5. ✅ **Minimal dependencies** (just trust-dns-resolver)
6. ✅ **Standard protocol** (RFC 2782)

## References

- [etcd Clustering Guide](https://etcd.io/docs/v3.5/op-guide/clustering/)
- [DNS SRV RFC 2782](https://www.rfc-editor.org/rfc/rfc2782)
- [Consul Gossip Architecture](https://www.hashicorp.com/resources/everybody-talks-gossip-serf-memberlist-raft-swim-hashicorp-consul)
- [TiKV Raft Implementation](https://tikv.org/blog/implement-raft-in-rust/)
- [Kubernetes StatefulSets](https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/)
