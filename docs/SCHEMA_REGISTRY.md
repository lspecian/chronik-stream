# Schema Registry Guide

**Version**: v2.2.20
**Status**: Production-Ready with Optional HTTP Basic Auth

---

## Overview

Chronik includes a built-in **Confluent-compatible Schema Registry** that provides:

- Schema storage and retrieval with unique global IDs
- Subject management (topic-value, topic-key naming convention)
- Schema evolution with compatibility checking
- Support for Avro, JSON Schema, and Protobuf
- Optional HTTP Basic Auth (Confluent-compatible)

The Schema Registry runs as part of the Admin API on port `10000 + node_id` (e.g., port 10001 for node 1).

---

## Quick Start

### 1. Start Chronik in Cluster Mode

The Schema Registry is available in cluster mode:

```bash
# Start a cluster (Schema Registry enabled automatically)
./tests/cluster/start.sh

# Schema Registry available at:
# Node 1: http://localhost:10001
# Node 2: http://localhost:10002
# Node 3: http://localhost:10003
```

### 2. Register a Schema

```bash
# Register an Avro schema
curl -X POST http://localhost:10001/subjects/users-value/versions \
  -H "Content-Type: application/json" \
  -d '{
    "schema": "{\"type\":\"record\",\"name\":\"User\",\"fields\":[{\"name\":\"id\",\"type\":\"int\"},{\"name\":\"name\",\"type\":\"string\"}]}"
  }'

# Response: {"id":1}
```

### 3. Retrieve Schema by ID

```bash
curl http://localhost:10001/schemas/ids/1

# Response: {"schema":"{\"type\":\"record\"...}","schemaType":"AVRO","references":[]}
```

---

## REST API Reference

### Subject Management

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/subjects` | List all subjects |
| GET | `/subjects/{subject}/versions` | List versions for a subject |
| POST | `/subjects/{subject}/versions` | Register a new schema |
| GET | `/subjects/{subject}/versions/{version}` | Get schema by version |
| GET | `/subjects/{subject}/versions/latest` | Get latest schema |
| DELETE | `/subjects/{subject}` | Delete subject and all versions |
| DELETE | `/subjects/{subject}/versions/{version}` | Delete specific version |

### Schema by ID

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/schemas/ids/{id}` | Get schema by global ID |

### Compatibility Configuration

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/config` | Get global compatibility level |
| PUT | `/config` | Set global compatibility level |
| GET | `/config/{subject}` | Get subject compatibility level |
| PUT | `/config/{subject}` | Set subject compatibility level |

---

## API Examples

### Register Schema

```bash
# Avro schema (default)
curl -X POST http://localhost:10001/subjects/orders-value/versions \
  -H "Content-Type: application/json" \
  -d '{
    "schema": "{\"type\":\"record\",\"name\":\"Order\",\"fields\":[{\"name\":\"orderId\",\"type\":\"string\"},{\"name\":\"amount\",\"type\":\"double\"}]}"
  }'

# JSON Schema
curl -X POST http://localhost:10001/subjects/events-value/versions \
  -H "Content-Type: application/json" \
  -d '{
    "schemaType": "JSON",
    "schema": "{\"type\":\"object\",\"properties\":{\"event_type\":{\"type\":\"string\"}}}"
  }'

# Protobuf schema
curl -X POST http://localhost:10001/subjects/messages-value/versions \
  -H "Content-Type: application/json" \
  -d '{
    "schemaType": "PROTOBUF",
    "schema": "syntax = \"proto3\"; message Message { string content = 1; }"
  }'
```

### List All Subjects

```bash
curl http://localhost:10001/subjects
# Response: ["users-value","orders-value","events-value"]
```

### Get Schema by Subject and Version

```bash
# Get specific version
curl http://localhost:10001/subjects/users-value/versions/1

# Get latest version
curl http://localhost:10001/subjects/users-value/versions/latest

# Response:
# {
#   "subject": "users-value",
#   "version": 1,
#   "id": 1,
#   "schema": "...",
#   "schemaType": "AVRO"
# }
```

### Configure Compatibility

```bash
# Get current global compatibility
curl http://localhost:10001/config
# Response: {"compatibilityLevel":"BACKWARD"}

# Set global compatibility to FULL
curl -X PUT http://localhost:10001/config \
  -H "Content-Type: application/json" \
  -d '{"compatibilityLevel":"FULL"}'

# Set subject-specific compatibility
curl -X PUT http://localhost:10001/config/users-value \
  -H "Content-Type: application/json" \
  -d '{"compatibilityLevel":"NONE"}'
```

### Delete Schema

```bash
# Delete specific version (returns deleted version number)
curl -X DELETE http://localhost:10001/subjects/users-value/versions/1
# Response: 1

# Delete entire subject (returns all deleted versions)
curl -X DELETE http://localhost:10001/subjects/users-value
# Response: [1,2,3]
```

---

## Compatibility Levels

| Level | Description |
|-------|-------------|
| `NONE` | No compatibility checking |
| `BACKWARD` | New schema can read data written by old schema (default) |
| `BACKWARD_TRANSITIVE` | New schema can read data written by all previous schemas |
| `FORWARD` | Old schema can read data written by new schema |
| `FORWARD_TRANSITIVE` | Old schema can read data written by all new schemas |
| `FULL` | Both backward and forward compatible |
| `FULL_TRANSITIVE` | Full compatibility with all previous versions |

---

## Authentication

### HTTP Basic Auth (Optional)

Schema Registry supports optional HTTP Basic Auth, similar to Confluent Schema Registry.

#### Enable Authentication

```bash
# Set environment variables before starting Chronik
export CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED=true
export CHRONIK_SCHEMA_REGISTRY_USERS="admin:secret123,readonly:viewonly"

# Start cluster
./chronik-server start --config cluster.toml
```

**Expected log output:**
```
✓ Schema Registry HTTP Basic Auth enabled (2 users)
```

#### Using Authentication

```bash
# With curl -u flag (recommended)
curl -u admin:secret123 http://localhost:10001/subjects

# With Authorization header
curl -H "Authorization: Basic YWRtaW46c2VjcmV0MTIz" http://localhost:10001/subjects

# Without credentials (returns 401 Unauthorized)
curl http://localhost:10001/subjects
```

#### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED` | Enable HTTP Basic Auth | `false` |
| `CHRONIK_SCHEMA_REGISTRY_USERS` | Comma-separated `user:pass` pairs | (none) |
| `CHRONIK_SCHEMA_REGISTRY_COMPATIBILITY` | Default compatibility level | `BACKWARD` |

---

## Schema Types

### Avro (Default)

```json
{
  "schema": "{\"type\":\"record\",\"name\":\"User\",\"namespace\":\"com.example\",\"fields\":[{\"name\":\"id\",\"type\":\"int\"},{\"name\":\"name\",\"type\":\"string\"},{\"name\":\"email\",\"type\":[\"null\",\"string\"],\"default\":null}]}"
}
```

### JSON Schema

```json
{
  "schemaType": "JSON",
  "schema": "{\"$schema\":\"http://json-schema.org/draft-07/schema#\",\"type\":\"object\",\"properties\":{\"id\":{\"type\":\"integer\"},\"name\":{\"type\":\"string\"}},\"required\":[\"id\",\"name\"]}"
}
```

### Protobuf

```json
{
  "schemaType": "PROTOBUF",
  "schema": "syntax = \"proto3\";\npackage example;\n\nmessage User {\n  int32 id = 1;\n  string name = 2;\n}"
}
```

---

## Error Codes

| HTTP Status | Error Code | Description |
|-------------|------------|-------------|
| 404 | 40401 | Subject not found |
| 404 | 40402 | Version not found |
| 404 | 40403 | Schema not found |
| 409 | 409 | Incompatible schema or subject already exists |
| 422 | 42201 | Invalid schema |
| 401 | - | Unauthorized (when auth enabled) |

---

## Integration with Kafka Clients

### Python (confluent-kafka)

```python
from confluent_kafka.schema_registry import SchemaRegistryClient
from confluent_kafka.schema_registry.avro import AvroSerializer

# Without auth
sr_client = SchemaRegistryClient({'url': 'http://localhost:10001'})

# With auth
sr_client = SchemaRegistryClient({
    'url': 'http://localhost:10001',
    'basic.auth.user.info': 'admin:secret123'
})

# Use with producer
serializer = AvroSerializer(sr_client, schema_str)
```

### Java

```java
import io.confluent.kafka.schemaregistry.client.CachedSchemaRegistryClient;

// Without auth
SchemaRegistryClient client = new CachedSchemaRegistryClient(
    "http://localhost:10001", 100);

// With auth
Map<String, String> configs = new HashMap<>();
configs.put("basic.auth.credentials.source", "USER_INFO");
configs.put("basic.auth.user.info", "admin:secret123");

SchemaRegistryClient client = new CachedSchemaRegistryClient(
    "http://localhost:10001", 100, configs);
```

---

## Best Practices

### 1. Subject Naming Convention

Follow Kafka's convention:
- `{topic}-key` for key schemas
- `{topic}-value` for value schemas

```bash
# For topic "orders"
curl -X POST http://localhost:10001/subjects/orders-key/versions ...
curl -X POST http://localhost:10001/subjects/orders-value/versions ...
```

### 2. Schema Evolution

1. Start with `BACKWARD` compatibility (default)
2. Add fields with defaults
3. Never remove required fields
4. Test compatibility before deploying

### 3. Security

1. Enable auth in production: `CHRONIK_SCHEMA_REGISTRY_AUTH_ENABLED=true`
2. Use strong passwords in `CHRONIK_SCHEMA_REGISTRY_USERS`
3. Use TLS for Admin API in production
4. Restrict network access to Admin API port

---

## Troubleshooting

### Schema Registration Fails

```bash
# Check if schema is valid JSON
echo '{"type":"record"...}' | jq .

# Check compatibility
curl http://localhost:10001/config
```

### Authentication Errors

```bash
# Verify auth is enabled in logs
grep "Schema Registry HTTP Basic Auth" logs/node1.log

# Test with credentials
curl -v -u admin:secret123 http://localhost:10001/subjects
```

### Subject Not Found

```bash
# List all subjects to verify
curl http://localhost:10001/subjects

# Check exact subject name (case-sensitive)
```

---

## See Also

- [Admin API Security](ADMIN_API_SECURITY.md) - Admin API authentication
- [Running a Cluster](RUNNING_A_CLUSTER.md) - Cluster setup guide
- [Kafka Client Compatibility](kafka-client-compatibility.md) - Client integration
