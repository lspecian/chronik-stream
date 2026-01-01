# Health Check Tests
# Tests basic service availability on unified API (port 6092) and admin API (port 10001)

# ============================================================================
# UNIFIED API HEALTH (Port 6092)
# ============================================================================

# Test 1: Basic health check
GET http://localhost:6092/health
HTTP 200
[Asserts]
jsonpath "$.status" == "ok"
jsonpath "$.version" exists

# Test 2: Health check returns proper content type and features
GET http://localhost:6092/health
HTTP 200
[Asserts]
header "Content-Type" contains "application/json"
jsonpath "$.sql_enabled" isBoolean
jsonpath "$.vector_enabled" isBoolean
jsonpath "$.search_enabled" isBoolean


# ============================================================================
# ADMIN API HEALTH (Port 10001 for node 1)
# This endpoint doesn't require authentication
# ============================================================================

# Test 3: Admin health check
GET http://localhost:10001/admin/health
HTTP 200
[Asserts]
jsonpath "$.status" == "ok"
jsonpath "$.node_id" isInteger
jsonpath "$.is_leader" isBoolean

# Test 4: Admin health returns cluster info
GET http://localhost:10001/admin/health
HTTP 200
[Asserts]
jsonpath "$.cluster_nodes" isCollection
