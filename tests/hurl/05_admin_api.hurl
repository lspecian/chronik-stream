# Admin API Tests
# Tests cluster management endpoints on admin API (port 10001)
# Note: Most endpoints require X-API-Key header for authentication
# Set CHRONIK_ADMIN_API_KEY environment variable or read from server logs

# ============================================================================
# HEALTH CHECK (No auth required)
# ============================================================================

# Test 1: Admin health check (no auth required)
GET http://localhost:10001/admin/health
HTTP 200
[Asserts]
jsonpath "$.status" == "ok"
jsonpath "$.node_id" isInteger


# ============================================================================
# CLUSTER STATUS
# Note: Auth may or may not be required depending on CHRONIK_ADMIN_API_KEY config
# ============================================================================

# Test 2: Get cluster status (works if no API key configured)
GET http://localhost:10001/admin/status
HTTP 200
[Asserts]
jsonpath "$.nodes" exists
jsonpath "$.leader_id" exists


# Test 3: Get cluster status with API key (if configured)
# This test requires api_key variable to be set
# GET http://localhost:10001/admin/status
# X-API-Key: {{api_key}}
# HTTP 200


# ============================================================================
# NODE MANAGEMENT (Requires auth)
# ============================================================================

# Test 4: Add node with missing required field (should fail)
# Note: Test cluster doesn't require auth, so we test validation directly
POST http://localhost:10001/admin/add-node
Content-Type: application/json
{
    "kafka_addr": "localhost:9199"
}
HTTP 422


# Test 5: Add duplicate node (should return success=false)
POST http://localhost:10001/admin/add-node
Content-Type: application/json
{
    "node_id": 1,
    "kafka_addr": "localhost:9092",
    "wal_addr": "localhost:9291",
    "raft_addr": "localhost:5001"
}
HTTP 200
[Asserts]
jsonpath "$.success" == false


# Test 6: Remove non-existent node (should return success=false)
POST http://localhost:10001/admin/remove-node
Content-Type: application/json
{
    "node_id": 999,
    "force": false
}
HTTP 200
[Asserts]
jsonpath "$.success" == false


# ============================================================================
# REBALANCE
# Note: Test cluster doesn't require auth
# ============================================================================

# Test 7: Trigger rebalance
POST http://localhost:10001/admin/rebalance
Content-Type: application/json
{}
HTTP 200
[Asserts]
jsonpath "$.success" == true


# ============================================================================
# INTEGRATION SCENARIO: Add and Remove Node
# ============================================================================

# Note: These tests add a temporary node and then remove it
# Only run these if you have a 3+ node cluster

# Test 10: Add node 4 (commented out to avoid affecting cluster state)
# POST http://localhost:10001/admin/add-node
# Content-Type: application/json
# X-API-Key: {{api_key}}
# {
#     "node_id": 4,
#     "kafka_addr": "localhost:9095",
#     "wal_addr": "localhost:9295",
#     "raft_addr": "localhost:5004"
# }
# HTTP 200

# Test 11: Verify node 4 in cluster status
# GET http://localhost:10001/admin/status
# X-API-Key: {{api_key}}
# HTTP 200

# Test 12: Remove node 4
# POST http://localhost:10001/admin/remove-node
# Content-Type: application/json
# X-API-Key: {{api_key}}
# {
#     "node_id": 4,
#     "force": false
# }
# HTTP 200
