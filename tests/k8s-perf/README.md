# Chronik K8s Performance Test Suite

End-to-end performance testing for Chronik Stream on Kubernetes using k6.

## Architecture

```
k6 TestRun (3 pods) ──HTTP POST──→ Ingestor Deployment (3 replicas) ──Kafka──→ Chronik Pod
                                                                                   ↑
                                   Consumer Deployment (1 replica) ←───────────────┘
                                   (reads + exposes /metrics)
```

## Prerequisites

- MicroK8s cluster with k6 operator installed
- `ghcr.io/chronik-stream/chronik-server:latest` image on all nodes
- `chronik-operator` binary built
- Docker for building the perf image

## Quick Start

```bash
# Run everything (build, deploy, test, collect results)
./tests/k8s-perf/run-all.sh

# Clean up when done
./tests/k8s-perf/cleanup.sh
```

## Components

| Component | Type | Purpose |
|-----------|------|---------|
| `chronik-bench` | ChronikStandalone | Chronik server under test |
| `ingestor` | Deployment (3 replicas) | HTTP → Kafka bridge |
| `consumer` | Deployment (1 replica) | Kafka consumer + metrics |
| `k6-chronik-load` | TestRun (3 pods) | k6 load generator |

## Endpoints

### Ingestor (port 8080)

- `POST /produce` — Single message: `{"key": "...", "value": "..."}`
- `POST /produce/batch` — Array of messages
- `GET /health` — Health check
- `GET /metrics` — `{"produced": N, "errors": N}`

### Consumer (port 8081)

- `GET /health` — Health check
- `GET /metrics` — `{"consumed": N, "bytes": N, "rate_msg_per_sec": N, ...}`

## Results

Results are saved to `tests/k8s-perf/results/<timestamp>/`:
- `k6-output.log` — k6 summary with req/s, latency percentiles
- `consumer-metrics.json` — Total messages consumed, rate
- `ingestor-metrics.json` — Total messages produced, errors
- `chronik-server.log` — Server logs during test
