# Chronik Stream File Organization Plan

**Analysis Date**: 2025-10-19
**Total Root Files Found**: 156 files (excluding .gitignore, LICENSE, etc.)

---

## Summary

**Current State**: 156 files cluttering the root directory
**Proposed State**: 12 essential files in root, rest organized into folders

**Categories**:
- 📦 Keep in Root: 12 files (essential project files)
- 📁 Move to Archive: 92 files (historical/obsolete documentation)
- 🧪 Move to Scripts: 48 files (test scripts)
- ⚙️ Move to Config: 10 files (example configurations)
- 🗑️ Delete: 4 files (duplicates, obsolete)

---

## Essential Files (Keep in Root)

These files MUST stay in root for project functionality:

```
✅ KEEP IN ROOT (12 files):
├── README.md                          # Main project documentation
├── CHANGELOG.md                       # Release history
├── CLAUDE.md                          # AI coding guidelines
├── CONTRIBUTING.md                    # Contribution guidelines
├── Cargo.toml                         # Rust workspace manifest
├── Cargo.docker.toml                  # Docker-specific Cargo config
├── Cross.toml                         # Cross-compilation config
├── DOCKER_README.md                   # Docker setup guide
├── INVESTIGATION.md                   # Active investigation notes
├── CLUSTERING_COMPLETION_ROADMAP.md   # Current roadmap (NEW)
├── CLUSTERING_IMPLEMENTATION_PLAN.md  # Implementation plan (docs/)
└── .gitignore, LICENSE, etc.          # Standard files
```

**Rationale**: These are either:
- Build configuration (Cargo.toml, Cross.toml)
- Primary documentation (README, CHANGELOG, CONTRIBUTING)
- Active working documents (INVESTIGATION.md, new roadmap)

---

## Historical Documentation (Move to Archive)

These are completion reports, status updates, and historical records from the Raft implementation process.

### Proposed Action: Create `archive/raft-implementation/` folder

```
📁 archive/raft-implementation/ (92 files)

Phase Completion Reports (28 files):
├── PHASE1_COMPLETE.md
├── PHASE1_TEST_RESULTS.md
├── PHASE2_COMPLETE.md
├── PHASE2_2_PARTITION_ASSIGNMENT_COMPLETE.md
├── PHASE2_4_FETCH_HANDLER_COMPLETE.md
├── PHASE2_5_MULTI_PARTITION_TEST_COMPLETE.md
├── PHASE3_COMPLETE.md
├── PHASE3_3_COMPLETE.md
├── PHASE3_3_IMPLEMENTATION_PLAN.md
├── PHASE3_3_INTEGRATION_INSTRUCTIONS.md
├── PHASE3_4_PARTITION_ASSIGNMENT_START_COMPLETE.md
├── PHASE3_5_CLUSTER_LIFECYCLE_TEST_COMPLETE.md
├── PHASE3_TEST_PLAN.md
├── PHASE3_TEST_RESULTS.md
├── PHASE4_COMPLETE.md
├── PHASE4.4_RAFT_METRICS_COMPLETE.md
├── PHASE4_1_ISR_TRACKING_COMPLETE.md
├── PHASE4_2_CONTROLLED_SHUTDOWN_COMPLETE.md
├── PHASE4_3_S3_BOOTSTRAP_COMPLETE.md
├── PHASE4_5_QUICKSTART.md
├── PHASE5_CLUSTER_INTEGRATION_COMPLETE.md
├── PHASE5_FAULT_TOLERANCE_STATUS.md
└── ... (more phase reports)

Raft Implementation History (35 files):
├── RAFT_IMPLEMENTATION_COMPLETE.md
├── RAFT_IMPLEMENTATION_PLAN.md
├── RAFT_INTEGRATION_COMPLETE.md
├── RAFT_NETWORK_LAYER_COMPLETE.md
├── RAFT_CLUSTER_COMPLETE.md
├── RAFT_CLUSTERING_E2E_COMPLETE.md
├── RAFT_CLUSTERING_ACTUAL_STATUS.md
├── RAFT_TESTS_COMPLETE.md
├── RAFT_FINAL_STATUS_REPORT.md
├── RAFT_E2E_TEST_STATUS_REPORT.md
└── ... (more raft docs)

Troubleshooting & Analysis (29 files):
├── RAFT_BOOTSTRAP_DEADLOCK.md
├── RAFT_COMMIT_FIX_SUMMARY.md
├── RAFT_CONSENSUS_ISSUE_ANALYSIS.md
├── RAFT_ISSUE_ANALYSIS_DETAIL.md
├── RAFT_MESSAGE_ISSUE_ANALYSIS.md
├── RAFT_PORT_CONFLICT_ANALYSIS.md
├── RAFT_QUORUM_ANALYSIS.md
├── RAFT_ROOT_CAUSE_ANALYSIS.md
├── RAFT_ROOT_CAUSE_FOUND.md
├── BROKER_REGISTRATION_FIX_SUMMARY.md
├── CLUSTER_STABILIZATION_COMPLETE.md
├── COMPILATION_SUCCESS_REPORT.md
└── ... (more analysis docs)

Feature Implementation Reports (10 files):
├── LEASE_IMPLEMENTATION_SUMMARY.md
├── LEASE_BASED_READS_REPORT.md
├── LEASE_PERFORMANCE_COMPARISON.md
├── SNAPSHOT_IMPLEMENTATION_COMPLETE.md
├── SNAPSHOT_IMPLEMENTATION_PLAN.md
├── SNAPSHOT_NEXT_STEPS.md
├── SNAPSHOT_TEST_RESULTS.md
├── ISR_INTEGRATION_STATUS.md
├── SEARCH_API_PORT_CHANGE.md
└── SELF_RELIANT_BOOTSTRAP.md

Session Summaries (5 files):
├── FINAL_SESSION_SUMMARY.md
├── INTEGRATION_SESSION_SUMMARY.md
├── PARALLEL_AGENTS_SUMMARY.md
├── MIGRATION_TO_MAIN_COMPLETE.md
└── HONEST_TEST_STATUS.md

Evaluation & Decision Docs (10 files):
├── CLUSTERING_EVALUATION.md
├── CLUSTERING_EVALUATION_COMPLETE.md
├── CLUSTERING_PROGRESS.md
├── CLUSTERING_STATUS_REPORT.md
├── CLUSTERING_COMPLETION_STATUS.md
├── OPENRAFT_EVALUATION.md
├── raftify-evaluation-report.md
├── RAFT_LIBRARY_ANALYSIS.md
├── RAFT_LIBRARY_COMPARISON.md
├── RAFT_LIBRARY_ACTION_PLAN.md

Design Documents (5 files):
├── FETCH_HANDLER_RAFT_DESIGN.md
├── FETCH_HANDLER_RAFT_TEST_PLAN.md
├── GOSSIP_VS_DETERMINISTIC_ANALYSIS.md
├── OPTION_C_ADVANCED_FEATURES_SUMMARY.md
├── RAFT_DECISION_SUMMARY.md

Bootstrap & Config Analysis (5 files):
├── AUTOMATIC_BOOTSTRAP_OPTIONS.md
├── BOOTSTRAP_RECOMMENDATION.md
├── CLI_EXAMPLES.md
├── RAFT_VERIFICATION_GUIDE.md
└── RAFT_MISSING_IMPLEMENTATION.md
```

**Rationale**: These documents are valuable for historical context but clutter the root. Move to archive for future reference.

---

## Test Scripts (Move to Scripts)

### Proposed Action: Create `scripts/tests/raft/` folder

```
📁 scripts/tests/raft/ (48 files)

E2E Tests (14 files):
├── test_raft_phase1_e2e.py
├── test_raft_phase2_multi_partition.py
├── test_raft_phase3_cluster_lifecycle.py
├── test_raft_phase4_production.py
├── test_raft_phase4_production_verified.py
├── test_raft_phase5_advanced_verified.py
├── test_raft_phase5_fault_injection.py
├── test_raft_e2e_simple.py
├── test_raft_e2e_failures.py
├── test_raft_cluster_basic.py
├── test_raft_cluster_lifecycle_e2e.py
├── test_raft_cluster_stress.py
├── test_raft_multi_partition_e2e.py
└── test_raft_single_partition_simple.py

Component Tests (10 files):
├── test_raft_leader_failover.py
├── test_raft_bridge_fix.py
├── test_raft_prost_bridge.py
├── test_snapshot_support.py
├── test_single_node_raft.py
├── test_flush_fix.py
├── test_segment_fetch.py
├── test_topic_create.py
├── test_metadata_simple.py
└── test_acks0_shutdown.py

Cluster Tests (8 files):
├── test_3node_cluster.sh
├── test_3node_produce_consume.py
├── test_cluster_broker_registration.sh
├── test_cluster_detailed.sh
├── test_cluster_kafka_client.py
├── test_cluster_stability.sh
├── test_healthcheck_bootstrap.sh
└── test_network_connectivity.sh

Debug/Diagnostic Tests (10 files):
├── test_raft_debug.sh
├── test_raft_diagnostic.sh
├── test_raft_simple.sh
├── test_raft_with_feature.sh
├── test_raft_messaging_only.sh
├── test_raft_conf_change.sh
├── test_raft_ports.sh
├── test_raft_ports_fixed.sh
├── test_single_node_fix.sh
└── test_broker_registration_debug.sh

Test Runners (3 files):
├── start_cluster_and_test.sh
├── run_raft_verification_suite.sh
└── test_cli_commands.sh

Benchmarks (1 file):
└── benchmark_raft_vs_standalone.py

Utilities (2 files):
├── copy_snapshot_to_main.sh
└── test_object_store_env.sh
```

---

## Configuration Files (Move to Examples)

### Proposed Action: Create `config/examples/` folder

```
📁 config/examples/ (10 files)

Cluster Configs (7 files):
├── chronik-cluster.toml              # Main cluster config
├── chronik-cluster-node1.toml        # Node 1 specific
├── chronik-cluster-node2.toml        # Node 2 specific
├── chronik-cluster-node3.toml        # Node 3 specific
├── node1-cluster.toml                # Duplicate of above
├── node2-cluster.toml                # Duplicate of above
└── node3-cluster.toml                # Duplicate of above

Stress Test Configs (3 files):
├── chronik-stress-node1.toml
├── chronik-stress-node2.toml
└── chronik-stress-node3.toml

Multi-DC Config (1 file):
└── multi-dc-config-example.toml
```

**Note**: `node1-cluster.toml`, `node2-cluster.toml`, `node3-cluster.toml` appear to be duplicates of `chronik-cluster-node*.toml` - recommend DELETE 3 files.

---

## Files to Delete

### Duplicates (3 files)
```
🗑️ DELETE:
├── node1-cluster.toml          # Duplicate of chronik-cluster-node1.toml
├── node2-cluster.toml          # Duplicate of chronik-cluster-node2.toml
└── node3-cluster.toml          # Duplicate of chronik-cluster-node3.toml
```

### Temporary/Obsolete (4 files)
```
🗑️ DELETE:
├── TEST_IMPLEMENTATION_SUMMARY.txt      # Text summary (info in .md files)
├── single_partition_test_output.txt    # Test output (recreatable)
├── fetch_handler_raft_integration.patch # Applied patch (obsolete)
└── RAFT_PROTOC_FIX.patch               # Applied patch (obsolete)
```

### Obsolete Completion Plan (1 file - KEEP LATEST ONLY)
```
🗑️ DELETE:
└── CLUSTERING_COMPLETION_PLAN.md       # Superseded by CLUSTERING_COMPLETION_ROADMAP.md
```

**Total to Delete**: 8 files

---

## Final Root Directory Structure

After cleanup, root should contain only:

```
chronik-stream/
├── README.md
├── CHANGELOG.md
├── CLAUDE.md
├── CONTRIBUTING.md
├── DOCKER_README.md
├── INVESTIGATION.md
├── CLUSTERING_COMPLETION_ROADMAP.md    # Current roadmap
├── Cargo.toml
├── Cargo.docker.toml
├── Cross.toml
├── .gitignore
├── LICENSE
│
├── archive/                            # Historical documentation
│   └── raft-implementation/
│       ├── phases/                     # Phase completion reports
│       ├── analysis/                   # Troubleshooting & root cause analysis
│       ├── features/                   # Feature implementation reports
│       ├── evaluations/                # Library & design evaluations
│       └── sessions/                   # Session summaries
│
├── config/
│   └── examples/                       # Example configurations
│       ├── cluster/                    # Cluster configs
│       ├── stress/                     # Stress test configs
│       └── multi-dc/                   # Multi-DC examples
│
├── scripts/
│   └── tests/
│       └── raft/                       # Raft test scripts
│           ├── e2e/                    # End-to-end tests
│           ├── component/              # Component tests
│           ├── cluster/                # Cluster tests
│           ├── debug/                  # Debug/diagnostic tests
│           └── benchmarks/             # Performance benchmarks
│
├── docs/                               # Keep existing official docs
│   ├── CLUSTER_CONFIGURATION.md
│   ├── RAFT_ARCHITECTURE.md
│   ├── RAFT_DEPLOYMENT_GUIDE.md
│   └── ... (other official docs)
│
├── crates/                             # Source code
├── tests/                              # Integration tests
└── ... (other project directories)
```

---

## Migration Commands

### Step 1: Create Directories
```bash
mkdir -p archive/raft-implementation/{phases,analysis,features,evaluations,sessions,design}
mkdir -p config/examples/{cluster,stress,multi-dc}
mkdir -p scripts/tests/raft/{e2e,component,cluster,debug,benchmarks,utilities}
```

### Step 2: Move Phase Completion Reports
```bash
mv PHASE*.md archive/raft-implementation/phases/
```

### Step 3: Move Raft Implementation Docs
```bash
mv RAFT_IMPLEMENTATION*.md archive/raft-implementation/
mv RAFT_INTEGRATION*.md archive/raft-implementation/
mv RAFT_NETWORK*.md archive/raft-implementation/
mv RAFT_CLUSTER*.md archive/raft-implementation/
mv RAFT_CLUSTERING*.md archive/raft-implementation/
mv RAFT_TESTS*.md archive/raft-implementation/
mv RAFT_FINAL*.md archive/raft-implementation/
mv RAFT_E2E*.md archive/raft-implementation/
```

### Step 4: Move Analysis Documents
```bash
mv RAFT_*ANALYSIS*.md archive/raft-implementation/analysis/
mv RAFT_*FIX*.md archive/raft-implementation/analysis/
mv RAFT_*ISSUE*.md archive/raft-implementation/analysis/
mv RAFT_ROOT_CAUSE*.md archive/raft-implementation/analysis/
mv RAFT_BOOTSTRAP_DEADLOCK.md archive/raft-implementation/analysis/
mv RAFT_QUORUM*.md archive/raft-implementation/analysis/
mv BROKER_REGISTRATION*.md archive/raft-implementation/analysis/
mv CLUSTER_STABILIZATION*.md archive/raft-implementation/analysis/
mv COMPILATION_SUCCESS*.md archive/raft-implementation/analysis/
```

### Step 5: Move Feature Reports
```bash
mv LEASE_*.md archive/raft-implementation/features/
mv SNAPSHOT_*.md archive/raft-implementation/features/
mv ISR_*.md archive/raft-implementation/features/
mv SEARCH_API*.md archive/raft-implementation/features/
mv SELF_RELIANT*.md archive/raft-implementation/features/
```

### Step 6: Move Session Summaries
```bash
mv *SESSION*.md archive/raft-implementation/sessions/
mv MIGRATION_TO_MAIN*.md archive/raft-implementation/sessions/
mv HONEST_TEST*.md archive/raft-implementation/sessions/
```

### Step 7: Move Evaluations
```bash
mv CLUSTERING_EVALUATION*.md archive/raft-implementation/evaluations/
mv CLUSTERING_PROGRESS*.md archive/raft-implementation/evaluations/
mv CLUSTERING_STATUS*.md archive/raft-implementation/evaluations/
mv OPENRAFT*.md archive/raft-implementation/evaluations/
mv raftify*.md archive/raft-implementation/evaluations/
mv RAFT_LIBRARY*.md archive/raft-implementation/evaluations/
mv RAFT_DECISION*.md archive/raft-implementation/evaluations/
```

### Step 8: Move Design Documents
```bash
mv FETCH_HANDLER*.md archive/raft-implementation/design/
mv GOSSIP_VS*.md archive/raft-implementation/design/
mv OPTION_C*.md archive/raft-implementation/design/
```

### Step 9: Move Bootstrap Docs
```bash
mv AUTOMATIC_BOOTSTRAP*.md archive/raft-implementation/
mv BOOTSTRAP_RECOMMENDATION*.md archive/raft-implementation/
mv CLI_EXAMPLES*.md archive/raft-implementation/
mv RAFT_VERIFICATION*.md archive/raft-implementation/
mv RAFT_MISSING*.md archive/raft-implementation/
```

### Step 10: Move Test Scripts
```bash
# E2E tests
mv test_raft_phase*.py scripts/tests/raft/e2e/
mv test_raft_e2e*.py scripts/tests/raft/e2e/
mv test_raft_cluster_*.py scripts/tests/raft/e2e/
mv test_raft_multi_partition*.py scripts/tests/raft/e2e/
mv test_raft_single_partition*.py scripts/tests/raft/e2e/

# Component tests
mv test_raft_leader*.py scripts/tests/raft/component/
mv test_raft_bridge*.py scripts/tests/raft/component/
mv test_raft_prost*.py scripts/tests/raft/component/
mv test_snapshot*.py scripts/tests/raft/component/
mv test_single_node*.py scripts/tests/raft/component/
mv test_flush*.py scripts/tests/raft/component/
mv test_segment*.py scripts/tests/raft/component/
mv test_topic*.py scripts/tests/raft/component/
mv test_metadata*.py scripts/tests/raft/component/
mv test_acks*.py scripts/tests/raft/component/

# Cluster tests
mv test_3node*.* scripts/tests/raft/cluster/
mv test_cluster*.* scripts/tests/raft/cluster/
mv test_healthcheck*.sh scripts/tests/raft/cluster/
mv test_network*.sh scripts/tests/raft/cluster/

# Debug tests
mv test_raft_debug*.sh scripts/tests/raft/debug/
mv test_raft_diagnostic*.sh scripts/tests/raft/debug/
mv test_raft_simple*.sh scripts/tests/raft/debug/
mv test_raft_with_feature*.sh scripts/tests/raft/debug/
mv test_raft_messaging*.sh scripts/tests/raft/debug/
mv test_raft_conf_change*.sh scripts/tests/raft/debug/
mv test_raft_ports*.sh scripts/tests/raft/debug/
mv test_single_node_fix*.sh scripts/tests/raft/debug/
mv test_broker_registration*.sh scripts/tests/raft/debug/

# Test runners
mv start_cluster*.sh scripts/tests/raft/
mv run_raft*.sh scripts/tests/raft/
mv test_cli*.sh scripts/tests/raft/

# Benchmarks
mv benchmark*.py scripts/tests/raft/benchmarks/

# Utilities
mv copy_snapshot*.sh scripts/tests/raft/utilities/
mv test_object_store*.sh scripts/tests/raft/utilities/
```

### Step 11: Move Config Files
```bash
# Cluster configs
mv chronik-cluster*.toml config/examples/cluster/

# Stress configs
mv chronik-stress*.toml config/examples/stress/

# Multi-DC config
mv multi-dc*.toml config/examples/multi-dc/
```

### Step 12: Delete Duplicates & Obsolete Files
```bash
# Delete duplicate configs
rm node1-cluster.toml node2-cluster.toml node3-cluster.toml

# Delete temporary files
rm TEST_IMPLEMENTATION_SUMMARY.txt
rm single_partition_test_output.txt
rm fetch_handler_raft_integration.patch
rm RAFT_PROTOC_FIX.patch

# Delete superseded plan
rm CLUSTERING_COMPLETION_PLAN.md
```

### Step 13: Create README Files
```bash
# Archive README
cat > archive/raft-implementation/README.md << 'EOF'
# Raft Implementation Archive

This directory contains historical documentation from the Raft clustering implementation (v2.0.0 development).

**Organization**:
- `phases/` - Phase completion reports (Phase 1-5)
- `analysis/` - Troubleshooting, bug fixes, root cause analysis
- `features/` - Feature implementation reports (ISR, snapshots, leases)
- `evaluations/` - Library comparisons, design decisions
- `sessions/` - Development session summaries
- `design/` - Design documents and architecture proposals

**Current Documentation**: See `/docs/RAFT_*.md` for official Raft documentation.
EOF

# Scripts README
cat > scripts/tests/raft/README.md << 'EOF'
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
EOF

# Config examples README
cat > config/examples/README.md << 'EOF'
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
cargo run --features raft --bin chronik-server -- --config config/examples/cluster/chronik-cluster.toml standalone --raft
```

See `/docs/RAFT_CONFIGURATION_REFERENCE.md` for all configuration options.
EOF
```

---

## Verification Checklist

After running migration:

- [ ] Root directory contains only 12-15 files
- [ ] All historical docs in `archive/raft-implementation/`
- [ ] All test scripts in `scripts/tests/raft/`
- [ ] All example configs in `config/examples/`
- [ ] 8 obsolete files deleted
- [ ] README files created in new directories
- [ ] Git history preserved (use `git mv` instead of `mv`)
- [ ] No broken links in remaining documentation

---

## Git-Safe Migration Script

**IMPORTANT**: Use `git mv` to preserve file history!

See [EXECUTE_FILE_ORGANIZATION.sh](../scripts/EXECUTE_FILE_ORGANIZATION.sh) for the complete migration script.

---

## Rollback Plan

If something goes wrong:
```bash
git reset --hard HEAD  # Discard all changes
git clean -fd          # Remove untracked files/directories
```

---

## Impact Assessment

**Before**:
- 156 files in root
- Difficult to find important files
- Unclear what's current vs. historical

**After**:
- 12-15 essential files in root
- Clean, organized structure
- Clear separation of current vs. archived content

**Benefits**:
- ✅ Easier to navigate project
- ✅ Clear what's important vs. historical
- ✅ Better for new contributors
- ✅ Maintains git history (if using `git mv`)

---

**Status**: Ready for execution
**Next Step**: Review this plan, then run migration script
