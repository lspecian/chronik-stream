# Raft Test Scripts

Python and Bash test scripts for Raft clustering functionality.

**Usage**:
```bash
# Run E2E tests
python3 scripts/tests/raft/e2e/test_raft_phase4_production_verified.py

# Run cluster tests
bash scripts/tests/raft/cluster/test_3node_cluster.sh

# Run benchmarks
python3 scripts/tests/raft/benchmarks/benchmark_raft_vs_standalone.py
```

**Organization**:
- `e2e/` - End-to-end tests (full cluster scenarios)
- `component/` - Component tests (individual features)
- `cluster/` - Multi-node cluster tests
- `debug/` - Debug and diagnostic utilities
- `benchmarks/` - Performance benchmarks
- `utilities/` - Helper scripts

**Note**: Most of these are historical test scripts. Modern tests are in `/tests/integration/raft_*.rs`.
