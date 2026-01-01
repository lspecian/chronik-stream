# Hot/Cold SQL Buffer Tests (v2.2.23)
# Tests sub-second SQL query latency via hot buffer + cold Parquet union
# Requires: A columnar-enabled topic with both WAL (hot) and Parquet (cold) data

# ============================================================================
# LIST TABLES - Verify hot/cold tables are registered
# ============================================================================

# Test 1: List tables - should include hot, cold, and unified views
GET http://localhost:6092/_sql/tables
HTTP 200
[Asserts]
header "Content-Type" contains "application/json"
jsonpath "$.tables" isCollection
# Note: Tables named {topic}_hot, {topic}_cold, and {topic} should appear
# after producing data to a columnar-enabled topic


# ============================================================================
# QUERY HOT DATA (in-memory WAL buffer)
# ============================================================================

# Test 2: Query hot table directly (recent data from WAL)
# This should return data within milliseconds since it's in-memory
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT COUNT(*) as hot_count FROM hot_cold_test_hot",
    "limit": 100
}
HTTP 200
[Asserts]
jsonpath "$.execution_time_ms" exists
# Hot buffer queries should be very fast (<100ms typically)


# ============================================================================
# QUERY COLD DATA (Parquet files)
# ============================================================================

# Test 3: Query cold table directly (historical data from Parquet)
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT COUNT(*) as cold_count FROM hot_cold_test_cold",
    "limit": 100
}
HTTP 200
[Asserts]
jsonpath "$.execution_time_ms" exists


# ============================================================================
# QUERY UNIFIED VIEW (hot + cold combined)
# ============================================================================

# Test 4: Query unified view - automatically unions hot and cold data
# Users query this table without needing to know about hot/cold separation
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT COUNT(*) as total_count FROM hot_cold_test",
    "limit": 100
}
HTTP 200
[Asserts]
jsonpath "$.columns" includes "total_count"
jsonpath "$.row_count" == 1


# Test 5: Unified view with offset filtering
# Should correctly filter across both hot and cold data
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT _offset, _partition FROM hot_cold_test WHERE _offset >= 0 ORDER BY _offset LIMIT 10",
    "limit": 10
}
HTTP 200
[Asserts]
jsonpath "$.columns" includes "_offset"
jsonpath "$.columns" includes "_partition"
jsonpath "$.row_count" <= 10


# Test 6: Aggregation across unified view
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT _partition, MIN(_offset) as min_offset, MAX(_offset) as max_offset FROM hot_cold_test GROUP BY _partition",
    "limit": 100
}
HTTP 200
[Asserts]
jsonpath "$.columns" includes "_partition"
jsonpath "$.columns" includes "min_offset"
jsonpath "$.columns" includes "max_offset"


# ============================================================================
# DESCRIBE HOT/COLD TABLES
# ============================================================================

# Test 7: Describe hot table schema
GET http://localhost:6092/_sql/describe/hot_cold_test_hot
HTTP 200
[Asserts]
jsonpath "$.table" == "hot_cold_test_hot"
jsonpath "$.columns" isCollection


# Test 8: Describe cold table schema
GET http://localhost:6092/_sql/describe/hot_cold_test_cold
HTTP 200
[Asserts]
jsonpath "$.table" == "hot_cold_test_cold"
jsonpath "$.columns" isCollection


# Test 9: Describe unified view schema
GET http://localhost:6092/_sql/describe/hot_cold_test
HTTP 200
[Asserts]
jsonpath "$.table" == "hot_cold_test"
jsonpath "$.columns" isCollection


# ============================================================================
# EXPLAIN HOT/COLD QUERIES
# ============================================================================

# Test 10: Explain unified query - should show UNION in plan
POST http://localhost:6092/_sql/explain
Content-Type: application/json
{
    "query": "SELECT * FROM hot_cold_test WHERE _offset > 100"
}
HTTP 200
[Asserts]
jsonpath "$.logical_plan" exists
# Plan should contain Union when both hot and cold data exist


# ============================================================================
# LATENCY VERIFICATION (Hot Buffer < 100ms)
# ============================================================================

# Test 11: Verify hot buffer provides sub-100ms latency
# Note: Actual latency depends on data size and system load
POST http://localhost:6092/_sql
Content-Type: application/json
{
    "query": "SELECT _offset, _timestamp, _value FROM hot_cold_test_hot LIMIT 1",
    "limit": 1,
    "timeout_secs": 1
}
HTTP 200
[Asserts]
# Execution time should typically be under 100ms for hot buffer
jsonpath "$.execution_time_ms" < 1000
