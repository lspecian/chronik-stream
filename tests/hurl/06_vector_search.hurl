# Vector Search API Tests
# Tests vector similarity search endpoints on unified API (port 6092)
# NOTE: Full vector search tests require a topic with vector indexing enabled.
# This file contains basic tests that work without vector-enabled topics.

# ============================================================================
# LIST VECTOR-ENABLED TOPICS
# ============================================================================

# Test 1: List topics with vector indexes
# This should always work even if no topics have vectors
GET http://localhost:6092/_vector/topics
HTTP 200
[Asserts]
header "Content-Type" contains "application/json"
jsonpath "$.topics" isCollection


# ============================================================================
# ERROR CASES (These work without vector-enabled topics)
# ============================================================================

# Test 2: Search on non-existent topic returns error (404 or 503)
POST http://localhost:6092/_vector/nonexistent_vector_topic/search
Content-Type: application/json
{
    "query": "test query",
    "top_k": 10
}
HTTP *
[Asserts]
status >= 400


# Test 3: Get stats for non-existent topic returns empty stats (200)
# Note: Stats endpoint returns 200 with zero vectors for non-existent topics
GET http://localhost:6092/_vector/nonexistent_vector_topic/stats
HTTP 200
[Asserts]
jsonpath "$.total_vectors" == 0
jsonpath "$.partition_count" == 0


# ============================================================================
# TESTS REQUIRING VECTOR-ENABLED TOPIC
# Uncomment these when running with a topic that has vector indexing.
# To enable vector indexing, configure embedding_field in topic config.
# ============================================================================

# # Replace 'my_vector_topic' with your actual vector-enabled topic name
#
# # Test: Search by text query (requires embeddings service)
# POST http://localhost:6092/_vector/my_vector_topic/search
# Content-Type: application/json
# {
#     "query": "machine learning algorithms",
#     "top_k": 10
# }
# HTTP 200
# [Asserts]
# jsonpath "$.results" isCollection
# jsonpath "$.query_time_ms" >= 0
#
# # Test: Search with filters
# POST http://localhost:6092/_vector/my_vector_topic/search
# Content-Type: application/json
# {
#     "query": "data processing",
#     "top_k": 5,
#     "filters": {
#         "partition": 0
#     }
# }
# HTTP 200
#
# # Test: Search by raw vector
# POST http://localhost:6092/_vector/my_vector_topic/search_by_vector
# Content-Type: application/json
# {
#     "vector": [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8],
#     "top_k": 5
# }
# HTTP 200
# [Asserts]
# jsonpath "$.results" isCollection
#
# # Test: Hybrid search combining text and vector
# POST http://localhost:6092/_vector/my_vector_topic/hybrid
# Content-Type: application/json
# {
#     "query": "distributed systems",
#     "top_k": 10,
#     "text_weight": 0.3,
#     "vector_weight": 0.7
# }
# HTTP 200
# [Asserts]
# jsonpath "$.results" isCollection
#
# # Test: Get stats for a topic
# GET http://localhost:6092/_vector/my_vector_topic/stats
# HTTP 200
# [Asserts]
# jsonpath "$.topic" == "my_vector_topic"
# jsonpath "$.total_vectors" >= 0
