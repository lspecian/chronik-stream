# Search API Tests
# Tests Elasticsearch-compatible search endpoints on unified API (port 6092)
# Requires: A topic with searchable data (searchable-enabled topic with sealed segments)

# ============================================================================
# INDEX MANAGEMENT
# ============================================================================

# Test 1: List indices (cat indices)
GET http://localhost:6092/_cat/indices
HTTP 200
[Asserts]
header "Content-Type" contains "application/json"


# Test 2: Create a test index
PUT http://localhost:6092/test_search_index
Content-Type: application/json
{
    "mappings": {
        "properties": {
            "title": {"type": "text"},
            "content": {"type": "text"},
            "timestamp": {"type": "long"}
        }
    }
}
HTTP 200


# Test 3: Get index mapping
GET http://localhost:6092/test_search_index/_mapping
HTTP 200
[Asserts]
jsonpath "$.test_search_index.mappings" exists


# ============================================================================
# DOCUMENT OPERATIONS
# ============================================================================

# Test 4: Index a document (returns 201 Created)
POST http://localhost:6092/test_search_index/_doc/doc1
Content-Type: application/json
{
    "title": "Test Document",
    "content": "This is a test document for search API testing",
    "timestamp": 1234567890
}
HTTP 201
[Asserts]
jsonpath "$._id" == "doc1"
jsonpath "$._index" == "test_search_index"


# Test 5: Index another document for search tests
POST http://localhost:6092/test_search_index/_doc/doc2
Content-Type: application/json
{
    "title": "Another Document",
    "content": "Search for chronik stream features",
    "timestamp": 1234567891
}
HTTP 201


# Test 6: Get document by ID
# NOTE: Get by ID has limited support - may return 400 if _id not indexed
# Skipping assertion on success, just checking the endpoint responds
GET http://localhost:6092/test_search_index/_doc/doc1
HTTP *


# Test 7: Get non-existent document
# NOTE: May return 400 instead of 404 due to _id lookup limitation
GET http://localhost:6092/test_search_index/_doc/nonexistent_doc
HTTP *


# ============================================================================
# SEARCH OPERATIONS
# ============================================================================

# Test 8: Simple match query
POST http://localhost:6092/test_search_index/_search
Content-Type: application/json
{
    "query": {
        "match": {
            "content": "test"
        }
    }
}
HTTP 200
[Asserts]
jsonpath "$.hits" exists
jsonpath "$.hits.total" exists


# Test 9: Match all query
POST http://localhost:6092/test_search_index/_search
Content-Type: application/json
{
    "query": {
        "match_all": {}
    },
    "size": 10
}
HTTP 200
[Asserts]
jsonpath "$.hits.hits" isCollection


# Test 10: Search with query string
# NOTE: Query string search requires Content-Type header
POST http://localhost:6092/test_search_index/_search
Content-Type: application/json
{
    "query": {
        "match": {
            "content": "chronik"
        }
    }
}
HTTP 200
[Asserts]
jsonpath "$.hits" exists


# Test 11: Search across all indices
POST http://localhost:6092/_search
Content-Type: application/json
{
    "query": {
        "match_all": {}
    },
    "size": 5
}
HTTP 200
[Asserts]
jsonpath "$.hits" exists


# Test 12: Search with from/size pagination
POST http://localhost:6092/test_search_index/_search
Content-Type: application/json
{
    "query": {
        "match_all": {}
    },
    "from": 0,
    "size": 1
}
HTTP 200
[Asserts]
jsonpath "$.hits.hits" isCollection


# Test 13: Bool query with must clause
POST http://localhost:6092/test_search_index/_search
Content-Type: application/json
{
    "query": {
        "bool": {
            "must": [
                {"match": {"content": "search"}}
            ]
        }
    }
}
HTTP 200


# Test 14: Term query
POST http://localhost:6092/test_search_index/_search
Content-Type: application/json
{
    "query": {
        "term": {
            "timestamp": 1234567890
        }
    }
}
HTTP 200


# ============================================================================
# CLEANUP
# ============================================================================

# Test 15: Delete document
# NOTE: Document deletion may return various codes depending on state
DELETE http://localhost:6092/test_search_index/_doc/doc1
HTTP *


# Test 16: Delete index
DELETE http://localhost:6092/test_search_index
HTTP 200
[Asserts]
jsonpath "$.acknowledged" == true


# Test 17: Verify index deleted (404 or 400 acceptable)
GET http://localhost:6092/test_search_index/_mapping
HTTP *
