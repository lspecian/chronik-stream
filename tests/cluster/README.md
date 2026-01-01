# Chronik Local Test Cluster

**This is THE ONLY way to run a local 3-node cluster for development testing.**

## Quick Start

```bash
# From project root
cd tests/cluster

# Start cluster
./start.sh

# Test with kafka-python
python3 ../../test_node_ready.py

# Stop cluster
./stop.sh
```

## What This Does

- **Starts 3 nodes** on localhost ports 9092, 9093, 9094
- **Clean data** on every start (fresh cluster)
- **Logs to files** in `logs/`
- **Easy cleanup** with stop.sh

## Files

- `node1.toml`, `node2.toml`, `node3.toml` - Node configurations
- `start.sh` - Start the cluster
- `stop.sh` - Stop the cluster
- `data/` - Data directories (created on start, cleaned on restart)
- `logs/` - Log files

## Configuration

Each node has:
- **Kafka port**: 9092, 9093, 9094
- **WAL port**: 9291, 9292, 9293
- **Raft port**: 5001, 5002, 5003

All nodes bind to `0.0.0.0` but advertise `localhost` for local testing.

## Bootstrap Servers

```
localhost:9092,localhost:9093,localhost:9094
```

## Logs

```bash
# Tail all logs (from tests/cluster directory)
tail -f logs/node*.log

# Check specific node
cat logs/node1.log
```

## DO NOT

- ❌ Create new cluster configs elsewhere
- ❌ Start nodes manually
- ❌ Use Docker for macOS testing (slow, unreliable)
- ❌ Scatter node configs in random directories

## Use This For

- ✅ Development testing
- ✅ Protocol debugging
- ✅ Client compatibility testing
- ✅ Integration testing

## Notes

- Cluster uses Raft consensus (3-node quorum)
- Fresh data on every start ensures clean state
- Logs persist for debugging even after stop
