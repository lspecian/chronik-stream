# Chronik Stream API Tests (Hurl)

This directory contains comprehensive API tests for Chronik Stream using [Hurl](https://hurl.dev/), a command-line tool for running HTTP requests.

## Prerequisites

1. **Install Hurl**:
   ```bash
   # Using cargo
   cargo install hurl

   # Or on Ubuntu/Debian
   curl -LO https://github.com/Orange-OpenSource/hurl/releases/download/4.2.0/hurl_4.2.0_amd64.deb
   sudo apt install ./hurl_4.2.0_amd64.deb

   # Or on macOS
   brew install hurl
   ```

2. **Start the test cluster**:
   ```bash
   ./tests/cluster/start.sh
   ```

3. **Produce test data** (for SQL tests):
   ```bash
   python3 -c "
   from kafka import KafkaProducer
   import json
   producer = KafkaProducer(
       bootstrap_servers='localhost:9092',
       value_serializer=lambda v: json.dumps(v).encode('utf-8'),
       api_version=(0, 10, 0)
   )
   for i in range(100):
       producer.send('sql_test_topic', {'id': i, 'name': f'user_{i}'})
   producer.flush()
   print('Sent 100 messages')
   "
   ```

## Test Files

| File | Description | Port |
|------|-------------|------|
| `01_health.hurl` | Health check endpoints | 6092, 10001 |
| `02_sql_api.hurl` | SQL query API (DataFusion) | 6092 |
| `03_search_api.hurl` | Elasticsearch-compatible search | 6092 |
| `04_schema_registry.hurl` | Confluent-compatible Schema Registry | 10001 |
| `05_admin_api.hurl` | Cluster management (auth required) | 10001 |
| `06_vector_search.hurl` | Vector similarity search | 6092 |
| `07_integration_workflow.hurl` | End-to-end integration tests | Various |

## Running Tests

### Run All Tests
```bash
./tests/hurl/run_tests.sh
```

### Run Specific Test Suite
```bash
./tests/hurl/run_tests.sh health
./tests/hurl/run_tests.sh sql
./tests/hurl/run_tests.sh search
./tests/hurl/run_tests.sh schema
./tests/hurl/run_tests.sh admin
./tests/hurl/run_tests.sh vector
./tests/hurl/run_tests.sh integration
```

### Run with Verbose Output
```bash
./tests/hurl/run_tests.sh --verbose
```

### Generate HTML Report
```bash
./tests/hurl/run_tests.sh --report
# Opens report.html in the tests/hurl directory
```

### Run with Admin API Key
```bash
# Set via environment variable
export CHRONIK_ADMIN_API_KEY="your-api-key"
./tests/hurl/run_tests.sh admin

# Or pass directly
./tests/hurl/run_tests.sh --api-key "your-api-key" admin
```

### Run Individual .hurl File
```bash
hurl --test tests/hurl/01_health.hurl
hurl --test tests/hurl/02_sql_api.hurl --verbose
```

## API Endpoints Tested

### Unified API (Port 6092)

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Service health check |
| `/_sql` | POST | Execute SQL query |
| `/_sql/explain` | POST | Explain query plan |
| `/_sql/tables` | GET | List SQL tables |
| `/_sql/describe/:table` | GET | Describe table schema |
| `/_search` | GET/POST | Search all indices |
| `/:index/_search` | GET/POST | Search specific index |
| `/:index/_doc/:id` | GET/POST/PUT/DELETE | Document CRUD |
| `/:index` | PUT/DELETE | Index management |
| `/:index/_mapping` | GET | Get index mapping |
| `/_cat/indices` | GET | List all indices |
| `/_vector/topics` | GET | List vector-enabled topics |
| `/_vector/:topic/search` | POST | Text-based vector search |
| `/_vector/:topic/search_by_vector` | POST | Raw vector search |
| `/_vector/:topic/hybrid` | POST | Hybrid text+vector search |
| `/_vector/:topic/stats` | GET | Vector index statistics |

### Admin API (Port 10001)

| Endpoint | Method | Auth | Description |
|----------|--------|------|-------------|
| `/admin/health` | GET | No | Health check |
| `/admin/status` | GET | Yes | Cluster status |
| `/admin/add-node` | POST | Yes | Add node to cluster |
| `/admin/remove-node` | POST | Yes | Remove node from cluster |
| `/admin/rebalance` | POST | Yes | Trigger partition rebalance |

### Schema Registry (Port 10001)

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/subjects` | GET | List all subjects |
| `/subjects/:subject/versions` | GET | List schema versions |
| `/subjects/:subject/versions` | POST | Register new schema |
| `/subjects/:subject/versions/:version` | GET | Get specific version |
| `/subjects/:subject/versions/:version` | DELETE | Delete version |
| `/subjects/:subject` | DELETE | Delete subject |
| `/schemas/ids/:id` | GET | Get schema by ID |
| `/config` | GET/PUT | Global compatibility config |
| `/config/:subject` | GET/PUT | Subject compatibility config |
| `/compatibility/subjects/:subject/versions/:version` | POST | Check compatibility |

## Test Data Requirements

### SQL Tests
- Requires topic `sql_test_topic` with Parquet data
- Topics must have `CHRONIK_DEFAULT_COLUMNAR=true` enabled
- WAL segments must be sealed (wait ~30s after produce)

### Search Tests
- Creates and cleans up test indices automatically
- No pre-existing data required

### Schema Registry Tests
- Creates and cleans up test subjects automatically
- No pre-existing data required

### Vector Search Tests
- Requires topic with vector embeddings configured
- Requires `embedding_field` in topic configuration
- May require embeddings service for text queries

### Admin Tests
- Requires valid API key for most endpoints
- Get API key from server logs or environment variable

## Troubleshooting

### Cluster Not Running
```
Error: Cluster does not appear to be running
```
**Solution**: Start the cluster with `./tests/cluster/start.sh`

### SQL Table Not Found
```
{"error": "Table 'sql_test_topic' not found"}
```
**Solution**: Produce data and wait for Parquet files to be created (~30s)

### Admin API 401 Unauthorized
```
HTTP 401
```
**Solution**: Provide API key with `--api-key` or set `CHRONIK_ADMIN_API_KEY`

### Vector Search 404
```
HTTP 404 for /_vector/topic/search
```
**Solution**: Ensure topic has vector indexing configured

## Writing New Tests

### Basic Structure
```hurl
# Comment describing test
GET http://localhost:6092/endpoint
HTTP 200
[Asserts]
jsonpath "$.field" == "expected_value"
jsonpath "$.array" isCollection
```

### With Request Body
```hurl
POST http://localhost:6092/endpoint
Content-Type: application/json
{
    "key": "value"
}
HTTP 200
[Asserts]
jsonpath "$.result" exists
```

### With Variables
```hurl
# Capture value
[Captures]
my_id: jsonpath "$.id"

# Use captured value
GET http://localhost:6092/endpoint/{{my_id}}
HTTP 200
```

### With Headers
```hurl
GET http://localhost:6092/admin/status
X-API-Key: {{api_key}}
HTTP 200
```

See [Hurl documentation](https://hurl.dev/docs/manual.html) for more details.
