# Integration Workflow Tests
# Tests end-to-end scenarios combining multiple APIs
# This file tests realistic user workflows

# ============================================================================
# SCENARIO 1: Data Ingestion to SQL Query Pipeline
# ============================================================================

# Step 1: Verify cluster is healthy
GET http://localhost:6092/health
HTTP 200
[Asserts]
jsonpath "$.status" == "ok"


# Step 2: Check if our test topic exists in SQL tables
# (Topic data should exist from prior produce operations)
GET http://localhost:6092/_sql/tables
HTTP 200


# Step 3: Query the topic to verify data is accessible
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT COUNT(*) as total_messages FROM sql_test_topic",
    "limit": 1
}
HTTP 200
[Asserts]
jsonpath "$.row_count" == 1
[Captures]
message_count: jsonpath "$.rows[0].total_messages"


# Step 4: Get table schema
GET http://localhost:6092/_sql/describe/sql_test_topic
HTTP 200
[Asserts]
jsonpath "$.columns" isCollection


# Step 5: Run analytics query
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT _partition, MIN(_offset) as min_offset, MAX(_offset) as max_offset, COUNT(*) as count FROM sql_test_topic GROUP BY _partition ORDER BY _partition",
    "limit": 100
}
HTTP 200
[Asserts]
jsonpath "$.columns" includes "_partition"
jsonpath "$.columns" includes "count"


# ============================================================================
# SCENARIO 2: Schema Registry Workflow
# ============================================================================

# Step 1: Check current compatibility setting
GET http://localhost:10001/config
HTTP 200
[Captures]
original_compat: jsonpath "$.compatibilityLevel"


# Step 2: Register initial schema for a new topic
POST http://localhost:10001/subjects/integration-test-value/versions
Content-Type: application/json
{
    "schema": "{\"type\": \"record\", \"name\": \"IntegrationTest\", \"fields\": [{\"name\": \"id\", \"type\": \"long\"}, {\"name\": \"data\", \"type\": \"string\"}]}"
}
HTTP 200
[Captures]
first_schema_id: jsonpath "$.id"


# Step 3: Get the registered schema by ID
GET http://localhost:10001/schemas/ids/{{first_schema_id}}
HTTP 200
[Asserts]
jsonpath "$.schema" exists


# Step 4: Evolve the schema (add optional field)
POST http://localhost:10001/subjects/integration-test-value/versions
Content-Type: application/json
{
    "schema": "{\"type\": \"record\", \"name\": \"IntegrationTest\", \"fields\": [{\"name\": \"id\", \"type\": \"long\"}, {\"name\": \"data\", \"type\": \"string\"}, {\"name\": \"metadata\", \"type\": [\"null\", \"string\"], \"default\": null}]}"
}
HTTP 200
[Captures]
second_schema_id: jsonpath "$.id"


# Step 5: List all versions
GET http://localhost:10001/subjects/integration-test-value/versions
HTTP 200
[Asserts]
jsonpath "$" includes 1
jsonpath "$" includes 2


# Step 6: Cleanup - delete the test subject
DELETE http://localhost:10001/subjects/integration-test-value
HTTP 200


# ============================================================================
# SCENARIO 3: Search then SQL Correlation
# ============================================================================

# Step 1: Create a search index
PUT http://localhost:6092/integration_search_test
Content-Type: application/json
{
    "mappings": {
        "properties": {
            "event_id": {"type": "keyword"},
            "message": {"type": "text"},
            "level": {"type": "keyword"},
            "timestamp": {"type": "long"}
        }
    }
}
HTTP 200


# Step 2: Index some documents (returns 201 Created)
POST http://localhost:6092/integration_search_test/_doc/evt001
Content-Type: application/json
{
    "event_id": "evt001",
    "message": "User login successful for admin user",
    "level": "INFO",
    "timestamp": 1700000000
}
HTTP 201


POST http://localhost:6092/integration_search_test/_doc/evt002
Content-Type: application/json
{
    "event_id": "evt002",
    "message": "Error processing payment transaction",
    "level": "ERROR",
    "timestamp": 1700000100
}
HTTP 201


POST http://localhost:6092/integration_search_test/_doc/evt003
Content-Type: application/json
{
    "event_id": "evt003",
    "message": "User logout from admin dashboard",
    "level": "INFO",
    "timestamp": 1700000200
}
HTTP 201


# Step 3: Search for error events
POST http://localhost:6092/integration_search_test/_search
Content-Type: application/json
{
    "query": {
        "match": {
            "level": "ERROR"
        }
    }
}
HTTP 200
[Asserts]
jsonpath "$.hits.total.value" >= 1


# Step 4: Search for admin-related events
POST http://localhost:6092/integration_search_test/_search
Content-Type: application/json
{
    "query": {
        "match": {
            "message": "admin"
        }
    }
}
HTTP 200
[Asserts]
jsonpath "$.hits.total.value" >= 1


# Step 5: Cleanup - delete search index
DELETE http://localhost:6092/integration_search_test
HTTP 200


# ============================================================================
# SCENARIO 4: Health Check All Endpoints
# ============================================================================

# Unified API health
GET http://localhost:6092/health
HTTP 200
[Asserts]
jsonpath "$.status" == "ok"


# Admin API health
GET http://localhost:10001/admin/health
HTTP 200
[Asserts]
jsonpath "$.status" == "ok"


# Node 2 health (if running)
GET http://localhost:6093/health
HTTP 200
[Asserts]
jsonpath "$.status" == "ok"


# Node 3 health (if running)
GET http://localhost:6094/health
HTTP 200
[Asserts]
jsonpath "$.status" == "ok"
