# Admin API Security Guide

**Version**: v2.2.20
**Status**: Production-Ready with Authentication

---

## Overview

The Chronik Admin API provides:
1. **Cluster management endpoints** for adding/removing nodes and querying cluster status
2. **Schema Registry endpoints** for Confluent-compatible schema management

Both services share the same port (`10000 + node_id`) but use **different authentication methods**:

| Service | Auth Method | Header | Environment Variable |
|---------|-------------|--------|---------------------|
| Admin API | API Key | `X-API-Key` | `CHRONIK_ADMIN_API_KEY` |
| Schema Registry | HTTP Basic | `Authorization: Basic` | `CHRONIK_SCHEMA_REGISTRY_USERS` |

---

## Schema Registry Authentication

The Schema Registry supports optional HTTP Basic Auth (Confluent-compatible).

### Enable Authentication

```bash
export CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED=true
export CHRONIK_SCHEMA_REGISTRY_USERS="admin:secret123,readonly:viewonly"
```

### Usage

```bash
# With curl -u flag
curl -u admin:secret123 http://localhost:10001/subjects

# Without credentials (returns 401 when auth enabled)
curl http://localhost:10001/subjects
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED` | `false` | Enable HTTP Basic Auth |
| `CHRONIK_SCHEMA_REGISTRY_USERS` | (none) | Comma-separated `user:pass` pairs |

**See [Schema Registry Guide](SCHEMA_REGISTRY.md) for full documentation.**

---

## Admin API Authentication

### API Key Authentication

The admin API uses API key authentication via the `X-API-Key` HTTP header.

#### Setup

**1. Generate a secure API key:**

```bash
# Generate a random API key
API_KEY=$(openssl rand -hex 32)
echo "Generated API key: $API_KEY"
```

**2. Configure all cluster nodes with the same API key:**

```bash
export CHRONIK_ADMIN_API_KEY="your-secret-api-key-here"
```

**3. Start cluster:**

```bash
./chronik-server start --config cluster.toml
```

**Expected log output:**
```
✓ Admin API authentication enabled
Starting Admin API HTTP server on 0.0.0.0:10001
```

#### Usage

**Add node with authentication:**

```bash
export CHRONIK_ADMIN_API_KEY="your-secret-api-key-here"

./chronik-server cluster add-node 4 \
  --kafka localhost:9095 \
  --wal localhost:9294 \
  --raft localhost:5004 \
  --config cluster.toml
```

**Or with curl:**

```bash
curl -X POST http://localhost:10001/admin/add-node \
  -H "Content-Type: application/json" \
  -H "X-API-Key: your-secret-api-key-here" \
  -d '{
    "node_id": 4,
    "kafka_addr": "localhost:9095",
    "wal_addr": "localhost:9294",
    "raft_addr": "localhost:5004"
  }'
```

### Without Authentication (Development Only)

**⚠️ WARNING: Running without authentication is INSECURE and should NEVER be used in production!**

If `CHRONIK_ADMIN_API_KEY` is not set, the admin API will run without authentication:

```bash
# INSECURE - Development only
./chronik-server start --config cluster.toml
```

**Log output:**
```
⚠ CHRONIK_ADMIN_API_KEY not set - Admin API will run WITHOUT authentication!
⚠ This is INSECURE for production. Set CHRONIK_ADMIN_API_KEY to enable auth.
```

---

## TLS Support

### Current Status

**TLS support is documented but not currently implemented** (requires `axum-server` crate).

The admin API will warn if you try to enable TLS:

```
⚠ TLS configuration detected but axum-server crate not available
⚠ Admin API will run over HTTP. To enable TLS, add axum-server dependency.
```

### Future TLS Implementation

When TLS is implemented, configuration will be:

```bash
export CHRONIK_ADMIN_TLS_CERT="/path/to/cert.pem"
export CHRONIK_ADMIN_TLS_KEY="/path/to/key.pem"
```

---

## Endpoints

### Public Endpoints (No Auth Required)

#### GET /admin/health

Health check endpoint - always accessible without authentication.

**Response:**
```json
{
  "status": "ok",
  "node_id": 1,
  "is_leader": true,
  "cluster_nodes": [1, 2, 3]
}
```

### Protected Endpoints (Auth Required)

#### POST /admin/add-node

Add a new node to the cluster.

**Request:**
```json
{
  "node_id": 4,
  "kafka_addr": "localhost:9095",
  "wal_addr": "localhost:9294",
  "raft_addr": "localhost:5004"
}
```

**Response (Success):**
```json
{
  "success": true,
  "message": "Node 4 addition proposed. Waiting for Raft consensus...",
  "node_id": 4
}
```

**Response (Error - No Auth):**
```
HTTP 401 Unauthorized
```

**Response (Error - Invalid Key):**
```
HTTP 401 Unauthorized
```

**Response (Error - Not Leader):**
```json
{
  "success": false,
  "message": "Cannot add node: this node is not the leader"
}
```

---

## Security Best Practices

### 1. Always Use Authentication in Production

```bash
# ✓ GOOD - Authentication enabled
export CHRONIK_ADMIN_API_KEY="$(openssl rand -hex 32)"
./chronik-server start --config cluster.toml

# ✗ BAD - No authentication
./chronik-server start --config cluster.toml
```

### 2. Use Strong API Keys

```bash
# ✓ GOOD - Random 256-bit key
API_KEY=$(openssl rand -hex 32)

# ✗ BAD - Weak key
API_KEY="password123"
```

### 3. Rotate Keys Regularly

```bash
# Generate new key
NEW_KEY=$(openssl rand -hex 32)

# Update on all nodes (requires restart)
export CHRONIK_ADMIN_API_KEY="$NEW_KEY"
```

### 4. Restrict Network Access

**Use firewall rules to restrict admin API access:**

```bash
# Only allow from management subnet
iptables -A INPUT -p tcp --dport 10001:10010 -s 10.0.1.0/24 -j ACCEPT
iptables -A INPUT -p tcp --dport 10001:10010 -j DROP
```

**Or bind to specific interface:**

```toml
# cluster.toml
[bind]
admin = "10.0.1.10:10001"  # Internal management network only
```

### 5. Use TLS (When Available)

```bash
# Future: TLS support
export CHRONIK_ADMIN_TLS_CERT="/etc/chronik/tls/cert.pem"
export CHRONIK_ADMIN_TLS_KEY="/etc/chronik/tls/key.pem"
export CHRONIK_ADMIN_API_KEY="$(openssl rand -hex 32)"
```

### 6. Log Monitoring

Monitor authentication failures:

```bash
# Watch for auth failures
tail -f /var/log/chronik/server.log | grep "Invalid API key\|No API key"
```

### 7. Principle of Least Privilege

- Only expose admin API to operations team
- Use separate API keys for different environments (dev/staging/prod)
- Never commit API keys to version control

---

## Troubleshooting

### Issue: "401 Unauthorized" when adding node

**Cause**: API key mismatch or not provided.

**Solution:**
```bash
# Ensure CHRONIK_ADMIN_API_KEY is set
echo $CHRONIK_ADMIN_API_KEY

# Check server logs for authentication errors
grep "Admin API" /var/log/chronik/server.log
```

### Issue: CLI can't find leader

**Cause**: All nodes are followers or unreachable.

**Solution:**
```bash
# Check cluster health on all nodes
for port in 10001 10002 10003; do
  echo "Node $port:"
  curl http://localhost:$port/admin/health | jq
done
```

### Issue: Server warns about missing authentication

**Cause**: `CHRONIK_ADMIN_API_KEY` not set.

**Solution:**
```bash
export CHRONIK_ADMIN_API_KEY="your-secret-key"
```

---

## Production Deployment Checklist

- [ ] API key generated with `openssl rand -hex 32`
- [ ] `CHRONIK_ADMIN_API_KEY` set on all nodes
- [ ] API key stored securely (env var, secrets manager)
- [ ] Firewall rules restrict admin API access
- [ ] TLS enabled (when available)
- [ ] Monitoring configured for auth failures
- [ ] API key rotation process documented
- [ ] Backup admin access method available

---

## Environment Variables Reference

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `CHRONIK_ADMIN_API_KEY` | **Recommended** | None | API key for authentication |
| `CHRONIK_ADMIN_TLS_CERT` | No | None | Path to TLS certificate (future) |
| `CHRONIK_ADMIN_TLS_KEY` | No | None | Path to TLS private key (future) |

---

## Examples

### Example 1: Production Setup (3-Node Cluster)

```bash
# Generate API key (save this securely!)
API_KEY=$(openssl rand -hex 32)
echo "API Key: $API_KEY" > /secure/location/api-key.txt

# Node 1
export CHRONIK_ADMIN_API_KEY="$API_KEY"
./chronik-server start --config cluster.toml --node-id 1 &

# Node 2
export CHRONIK_ADMIN_API_KEY="$API_KEY"
./chronik-server start --config cluster.toml --node-id 2 &

# Node 3
export CHRONIK_ADMIN_API_KEY="$API_KEY"
./chronik-server start --config cluster.toml --node-id 3 &

# Add node 4
export CHRONIK_ADMIN_API_KEY="$API_KEY"
./chronik-server cluster add-node 4 \
  --kafka node4:9092 \
  --wal node4:9291 \
  --raft node4:5001 \
  --config cluster.toml
```

### Example 2: Development Setup (No Auth)

```bash
# Start cluster without authentication (INSECURE)
./chronik-server start --config cluster.toml --node-id 1 &
./chronik-server start --config cluster.toml --node-id 2 &
./chronik-server start --config cluster.toml --node-id 3 &

# Add node 4 (no API key needed)
./chronik-server cluster add-node 4 \
  --kafka localhost:9095 \
  --wal localhost:9294 \
  --raft localhost:5004 \
  --config cluster.toml
```

---

**Security is not optional for production deployments. Always use authentication!**
