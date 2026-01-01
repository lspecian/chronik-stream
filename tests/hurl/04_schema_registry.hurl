# Schema Registry API Tests
# Tests Confluent-compatible Schema Registry endpoints on admin API (port 10001)
# These endpoints may or may not require authentication depending on configuration

# ============================================================================
# GLOBAL CONFIGURATION
# ============================================================================

# Test 1: Get global compatibility configuration
GET http://localhost:10001/config
HTTP 200
[Asserts]
header "Content-Type" contains "application/json"
jsonpath "$.compatibilityLevel" exists


# Test 2: Set global compatibility to BACKWARD
PUT http://localhost:10001/config
Content-Type: application/json
{
    "compatibilityLevel": "BACKWARD"
}
HTTP 200
[Asserts]
jsonpath "$.compatibilityLevel" == "BACKWARD"


# ============================================================================
# SUBJECTS - LIST AND REGISTER
# ============================================================================

# Test 3: List subjects (initially may be empty)
GET http://localhost:10001/subjects
HTTP 200
[Asserts]
header "Content-Type" contains "application/json"


# Test 4: Register an Avro schema for user-value subject
POST http://localhost:10001/subjects/user-value/versions
Content-Type: application/json
{
    "schema": "{\"type\": \"record\", \"name\": \"User\", \"namespace\": \"com.example\", \"fields\": [{\"name\": \"id\", \"type\": \"long\"}, {\"name\": \"name\", \"type\": \"string\"}, {\"name\": \"email\", \"type\": \"string\"}]}"
}
HTTP 200
[Asserts]
jsonpath "$.id" >= 1
[Captures]
schema_id: jsonpath "$.id"


# Test 5: Verify subject appears in list
GET http://localhost:10001/subjects
HTTP 200
[Asserts]
jsonpath "$" includes "user-value"


# Test 6: Register a JSON Schema
POST http://localhost:10001/subjects/order-value/versions
Content-Type: application/json
{
    "schemaType": "JSON",
    "schema": "{\"type\": \"object\", \"properties\": {\"orderId\": {\"type\": \"string\"}, \"amount\": {\"type\": \"number\"}, \"customerId\": {\"type\": \"string\"}}, \"required\": [\"orderId\", \"amount\"]}"
}
HTTP 200
[Asserts]
jsonpath "$.id" >= 1
[Captures]
json_schema_id: jsonpath "$.id"


# ============================================================================
# SUBJECTS - GET VERSIONS
# ============================================================================

# Test 7: List versions for a subject
GET http://localhost:10001/subjects/user-value/versions
HTTP 200
[Asserts]
jsonpath "$" includes 1


# Test 8: Get specific version of a schema
GET http://localhost:10001/subjects/user-value/versions/1
HTTP 200
[Asserts]
jsonpath "$.subject" == "user-value"
jsonpath "$.version" == 1
jsonpath "$.id" exists
jsonpath "$.schema" exists


# Test 9: Get latest version of a schema
GET http://localhost:10001/subjects/user-value/versions/latest
HTTP 200
[Asserts]
jsonpath "$.subject" == "user-value"
jsonpath "$.schema" exists


# ============================================================================
# SCHEMAS BY ID
# ============================================================================

# Test 10: Get schema by ID
GET http://localhost:10001/schemas/ids/{{schema_id}}
HTTP 200
[Asserts]
jsonpath "$.schema" exists


# ============================================================================
# SUBJECT-LEVEL CONFIGURATION
# ============================================================================

# Test 11: Get subject-level compatibility (inherits from global if not set)
GET http://localhost:10001/config/user-value
HTTP 200
[Asserts]
jsonpath "$.compatibilityLevel" exists


# Test 12: Set subject-level compatibility
PUT http://localhost:10001/config/user-value
Content-Type: application/json
{
    "compatibilityLevel": "FULL"
}
HTTP 200
[Asserts]
jsonpath "$.compatibilityLevel" == "FULL"


# ============================================================================
# SCHEMA EVOLUTION
# ============================================================================

# Test 13: Register evolved schema (add optional field - BACKWARD compatible)
POST http://localhost:10001/subjects/user-value/versions
Content-Type: application/json
{
    "schema": "{\"type\": \"record\", \"name\": \"User\", \"namespace\": \"com.example\", \"fields\": [{\"name\": \"id\", \"type\": \"long\"}, {\"name\": \"name\", \"type\": \"string\"}, {\"name\": \"email\", \"type\": \"string\"}, {\"name\": \"phone\", \"type\": [\"null\", \"string\"], \"default\": null}]}"
}
HTTP 200
[Asserts]
jsonpath "$.id" >= 1


# Test 14: Verify new version was created
GET http://localhost:10001/subjects/user-value/versions
HTTP 200
[Asserts]
jsonpath "$" includes 2


# ============================================================================
# CLEANUP
# ============================================================================

# Test 15: Delete specific version (soft delete)
DELETE http://localhost:10001/subjects/order-value/versions/1
HTTP 200


# Test 16: Delete entire subject
DELETE http://localhost:10001/subjects/order-value
HTTP 200


# Test 17: Verify subject deleted
GET http://localhost:10001/subjects
HTTP 200
[Asserts]
jsonpath "$" not includes "order-value"


# Test 18: Clean up user-value subject
DELETE http://localhost:10001/subjects/user-value
HTTP 200


# Test 19: Reset global config to default
PUT http://localhost:10001/config
Content-Type: application/json
{
    "compatibilityLevel": "BACKWARD"
}
HTTP 200
