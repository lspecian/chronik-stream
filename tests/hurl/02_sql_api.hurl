# SQL API Tests
# Tests SQL query endpoints on unified API (port 6092)
# Requires: A topic with Parquet data (columnar-enabled topic with sealed segments)

# ============================================================================
# LIST TABLES
# ============================================================================

# Test 1: List available SQL tables
GET http://localhost:6092/_sql/tables
HTTP 200
[Asserts]
header "Content-Type" contains "application/json"
jsonpath "$.tables" isCollection


# ============================================================================
# EXECUTE SQL QUERIES
# ============================================================================

# Test 2: Execute simple SELECT query
# Note: Replace 'sql_test_topic' with an actual topic that has Parquet data
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT COUNT(*) as total FROM sql_test_topic",
    "limit": 100
}
HTTP 200
[Asserts]
jsonpath "$.columns" isCollection
jsonpath "$.rows" isCollection
jsonpath "$.row_count" >= 0
jsonpath "$.execution_time_ms" >= 0
jsonpath "$.truncated" isBoolean


# Test 3: Execute SELECT with LIMIT
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT * FROM sql_test_topic LIMIT 5",
    "limit": 5
}
HTTP 200
[Asserts]
jsonpath "$.row_count" <= 5
jsonpath "$.truncated" == false


# Test 4: Execute SELECT with WHERE clause
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT _offset, _timestamp FROM sql_test_topic WHERE _offset > 0 LIMIT 10",
    "limit": 10
}
HTTP 200
[Asserts]
jsonpath "$.columns" includes "_offset"
jsonpath "$.columns" includes "_timestamp"


# Test 5: Execute aggregation query
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT _partition, COUNT(*) as msg_count FROM sql_test_topic GROUP BY _partition",
    "limit": 100
}
HTTP 200
[Asserts]
jsonpath "$.columns" includes "_partition"
jsonpath "$.columns" includes "msg_count"


# Test 6: Invalid SQL query returns error
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELEC * FROM invalid_syntax",
    "limit": 10
}
HTTP 400
[Asserts]
jsonpath "$.error" exists
jsonpath "$.error_type" == "QueryError"


# Test 7: Query non-existent table returns error
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT * FROM nonexistent_table_xyz",
    "limit": 10
}
HTTP 400
[Asserts]
jsonpath "$.error" exists


# Test 8: Query with default limit (should use 1000)
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT * FROM sql_test_topic"
}
HTTP 200
[Asserts]
jsonpath "$.row_count" >= 0


# ============================================================================
# EXPLAIN QUERY
# ============================================================================

# Test 9: Explain query plan
POST http://localhost:6092/_sql/explain
Content-Type: application/json
{
    "query": "SELECT * FROM sql_test_topic WHERE _offset > 10"
}
HTTP 200
[Asserts]
jsonpath "$.logical_plan" exists


# Test 10: Explain aggregation query
POST http://localhost:6092/_sql/explain
Content-Type: application/json
{
    "query": "SELECT COUNT(*), AVG(_offset) FROM sql_test_topic",
    "physical": false
}
HTTP 200
[Asserts]
jsonpath "$.logical_plan" exists


# ============================================================================
# DESCRIBE TABLE
# ============================================================================

# Test 11: Describe table schema
GET http://localhost:6092/_sql/describe/sql_test_topic
HTTP 200
[Asserts]
jsonpath "$.table" == "sql_test_topic"
jsonpath "$.columns" isCollection
jsonpath "$.columns[0].name" exists
jsonpath "$.columns[0].data_type" exists
jsonpath "$.columns[0].nullable" isBoolean


# Test 12: Describe non-existent table returns 404
GET http://localhost:6092/_sql/describe/nonexistent_table_xyz
HTTP 404
[Asserts]
jsonpath "$.error" exists
jsonpath "$.error_type" == "TableNotFound"
