# Admin API Security Guide

**Version**: v2.5.2
**Status**: Production-ready with authentication

---

## TL;DR

The Admin API lives on the **Unified API port (6092)** under `/admin/*`. Use:

```bash
# Liveness (no auth needed)
curl http://localhost:6092/admin/health

# Cluster status (requires API key)
curl -H "X-API-Key: $CHRONIK_ADMIN_API_KEY" http://localhost:6092/admin/status
```

Works in both **single-node** and **cluster** mode. In single-node, mutation routes (`add-node`, `remove-node`, `rebalance`) return a JSON "not supported" response; everything else works.

There is also a **legacy separate port** at `10000 + node_id` that exists for cluster-mode backward compatibility — it is **deprecated** and will be removed in a future release. See the [Legacy port appendix](#legacy-port-10000--node_id-deprecated) at the end of this document.

---

## Overview

The Chronik Admin API exposes two surfaces on the Unified API:

1. **Cluster management** under `/admin/*` — auth via `X-API-Key` header.
2. **Schema Registry (Confluent-compatible)** under `/subjects/*`, `/schemas/*`, `/config/*` — auth via HTTP Basic.

| Service | Path prefix | Auth method | Env var |
|---|---|---|---|
| Admin | `/admin/*` | `X-API-Key` header | `CHRONIK_ADMIN_API_KEY` |
| Schema Registry | `/subjects/*`, `/schemas/*`, `/config/*` | HTTP Basic | `CHRONIK_SCHEMA_REGISTRY_USERS` |

---

## Admin API (`X-API-Key`)

### Endpoints

| Method | Path | Auth | Notes |
|---|---|---|---|
| GET | `/admin/health` | **no** | Liveness probe. Works in single-node and cluster. |
| GET | `/admin/status` | yes | Cluster topology, ISR, partition assignments. |
| POST | `/admin/add-node` | yes | Cluster only. Single-node returns `success: false` with explanation. |
| POST | `/admin/remove-node` | yes | Cluster only. Same note. |
| POST | `/admin/rebalance` | yes | Cluster only. Same note. |

### Enable authentication

```bash
export CHRONIK_ADMIN_API_KEY="use-a-long-random-string-here"
./chronik-server start --advertise my-host
```

When `CHRONIK_ADMIN_API_KEY` is **not** set, a WARN is logged at startup and auth is skipped. Do not do this in production.

### Error-body contract (v2.5.2+)

Previously, auth failures returned **bare HTTP status lines with empty bodies** (axum 0.6 convention when handlers return `Err(StatusCode)`). This produced operator confusion of the form "Chronik admin port accepts TCP but returns empty HTTP." Starting in v2.5.2, every auth/route failure carries a JSON body:

```json
{
  "status": 401,
  "error": "missing X-API-Key header",
  "hint": "This route is protected. Add: -H 'X-API-Key: <key>' (value of CHRONIK_ADMIN_API_KEY).",
  "docs": "https://github.com/lspecian/chronik-stream/blob/main/docs/ADMIN_API_SECURITY.md"
}
```

Hitting `/admin` with no sub-path (or a typo like `/admin/statuss`) returns a JSON 404 listing the available routes:

```json
{
  "status": 404,
  "error": "unknown admin path",
  "available": {
    "public": ["GET /admin/health"],
    "protected": ["GET /admin/status", "POST /admin/add-node", "POST /admin/remove-node", "POST /admin/rebalance"],
    "schema_registry": ["GET /subjects", "POST /subjects/{subject}/versions", "GET /schemas/ids/{id}", "GET /config"]
  },
  "docs": "..."
}
```

### Common calls

```bash
export KEY=$CHRONIK_ADMIN_API_KEY

# Liveness
curl http://localhost:6092/admin/health

# Cluster status (single-node reports node_id=1, is_leader=true, one entry)
curl -H "X-API-Key: $KEY" http://localhost:6092/admin/status

# Add node (cluster only)
curl -X POST http://localhost:6092/admin/add-node \
  -H "X-API-Key: $KEY" \
  -H "Content-Type: application/json" \
  -d '{"node_id":4,"kafka_addr":"node4:9092","wal_addr":"node4:9291","raft_addr":"node4:5001"}'

# Remove node (cluster only)
curl -X POST http://localhost:6092/admin/remove-node \
  -H "X-API-Key: $KEY" \
  -H "Content-Type: application/json" \
  -d '{"node_id":4,"force":false}'
```

---

## Schema Registry (HTTP Basic Auth)

Enable auth:

```bash
export CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED=true
export CHRONIK_SCHEMA_REGISTRY_USERS="admin:secret123,readonly:viewonly"
```

Use:

```bash
# With -u flag
curl -u admin:secret123 http://localhost:6092/subjects

# Without credentials when auth enabled → JSON 401 (same error-body contract)
curl http://localhost:6092/subjects
```

When `CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED=false` (default), the Schema Registry is unauthenticated.

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED` | `false` | Enable HTTP Basic Auth |
| `CHRONIK_SCHEMA_REGISTRY_USERS` | (none) | Comma-separated `user:pass` pairs |

---

## Deployment patterns

### Kubernetes liveness probe

```yaml
livenessProbe:
  httpGet:
    path: /admin/health
    port: 6092
  initialDelaySeconds: 10
  periodSeconds: 10
```

`/admin/health` is always available without auth, in both single-node and cluster mode, so it's safe as a kubelet probe target.

### Docker HEALTHCHECK

```dockerfile
HEALTHCHECK --interval=30s --timeout=3s \
  CMD curl -fsS http://localhost:6092/admin/health || exit 1
```

The image as shipped uses a simpler `nc -z localhost 9092` check; upgrade to the above if you want HTTP-level verification.

### Network isolation

```bash
# Restrict the Unified API to an internal management network
iptables -A INPUT -p tcp --dport 6092 -s 10.0.1.0/24 -j ACCEPT
iptables -A INPUT -p tcp --dport 6092 -j DROP
```

Kafka (9092) stays open to clients; admin/HTTP (6092) stays behind your management network.

---

## Troubleshooting

### "Chronik admin port accepts TCP but returns empty HTTP"

This was the canonical symptom in v2.5.1 and earlier: hitting a protected route without `X-API-Key`, or hitting `/admin` with no sub-path, returned a bare status line with no body. **Fixed in v2.5.2.** If you still see an empty body, make sure the release artifact is v2.5.2+ (`curl localhost:6092/admin/health` now includes `"status": "ok"` in JSON).

### `curl` returns 404 but I'm hitting the right path

Check the path spelling. `/admin/status` is correct; `/admin/statuss` or `/admin/stats` returns JSON 404 with the route list.

### "add-node requires cluster mode (no Raft in single-node)"

This is expected in single-node mode. Start with a cluster config (see `examples/cluster-3node.toml` or [RUNNING_A_CLUSTER.md](RUNNING_A_CLUSTER.md)) if you need multi-node management.

### Logs show `⚠ CHRONIK_ADMIN_API_KEY not set`

Auth is disabled. Set the env var. Anyone on the network can hit `/admin/add-node` without a key — not safe for production.

---

## Legacy port `10000 + node_id` (DEPRECATED)

Before v2.2.22 the Admin API ran on a separate port (`10000 + node_id`, e.g. `10001` for node 1). That port is still bound **in cluster mode** for backward compatibility but logs a deprecation warning at startup:

```
⚠ DEPRECATED: Admin API running on separate port 10001
⚠ In v2.2.22+, use the Unified API on port 6092 (CHRONIK_UNIFIED_API_PORT) instead.
⚠ The separate admin port (10000+node_id) will be removed in a future version.
```

Single-node mode does **not** bind the legacy port at all (if you see "connection refused" on 10001 in single-node, that's why).

Migration:
- Replace `http://host:10001/admin/...` → `http://host:6092/admin/...`
- Replace `http://host:10001/subjects` → `http://host:6092/subjects`

Everything else — auth, request bodies, response shapes — is identical. The legacy port will be removed in a future major release.

---

## References

- [API_REFERENCE.md](API_REFERENCE.md) — all Chronik HTTP endpoints
- [SCHEMA_REGISTRY.md](SCHEMA_REGISTRY.md) — Schema Registry data model and API
- [RUNNING_A_CLUSTER.md](RUNNING_A_CLUSTER.md) — cluster bootstrap and management
