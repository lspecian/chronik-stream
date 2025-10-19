# Chronik Configuration Examples

Example TOML configuration files for various deployment scenarios.

**Cluster Configs** (`cluster/`):
- `chronik-cluster.toml` - Basic 3-node cluster
- `chronik-cluster-node*.toml` - Per-node configurations

**Stress Test Configs** (`stress/`):
- Configurations for stress testing cluster deployments

**Multi-DC Configs** (`multi-dc/`):
- Multi-datacenter replication examples

**Usage**:
```bash
cargo run --features raft --bin chronik-server -- \
  --config config/examples/cluster/chronik-cluster.toml \
  standalone --raft
```

See `/docs/RAFT_CONFIGURATION_REFERENCE.md` for all configuration options.
