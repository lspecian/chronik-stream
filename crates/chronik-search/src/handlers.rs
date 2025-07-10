//! HTTP request handlers for the search API.

use crate::api::{
    SearchApi, SearchRequest, SearchResponse, IndexDocumentRequest, IndexDocumentResponse,
    GetDocumentResponse, DeleteDocumentResponse, CatIndexInfo, ErrorResponse, ErrorInfo,
    ShardInfo, HitsInfo, TotalHits, Hit, IndexMapping, FieldMapping, QueryDsl, BoolQuery,
    HighlightConfig,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chronik_common::{Result, Error};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::Arc,
    time::SystemTime,
};
use tantivy::{
    collector::TopDocs,
    query::{Query as TantivyQuery, QueryParser, TermQuery, RangeQuery, BooleanQuery, Occur, AllQuery},
    schema::{FieldType, Value},
    Term,
};
use tracing::error;
use uuid::Uuid;

/// Health check response
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// Health check handler
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "green".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// Metrics handler (returns Prometheus metrics)
pub async fn metrics_handler() -> impl IntoResponse {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

/// Search all indices
pub async fn search_all(
    State(api): State<Arc<SearchApi>>,
    Json(request): Json<SearchRequest>,
) -> impl IntoResponse {
    let start = SystemTime::now();
    
    // Generate cache key
    let cache_key = crate::cache::QueryCache::generate_key(None, &request);
    
    // Check cache first
    if let Some(cached_response) = api.cache.get(&cache_key) {
        return Json(cached_response);
    }
    
    // Search across all indices
    let mut all_hits = Vec::new();
    let indices = api.indices.clone();
    
    for entry in indices.iter() {
        let index_name = entry.key().clone();
        let state = entry.value();
        
        match search_in_index(&index_name, &state, &request).await {
            Ok(mut hits) => all_hits.append(&mut hits),
            Err(e) => {
                error!("Error searching index {}: {}", index_name, e);
            }
        }
    }
    
    // Sort and limit results
    all_hits.sort_by(|a, b| b._score.partial_cmp(&a._score).unwrap());
    all_hits.truncate(request.size);
    
    let took = start.elapsed().unwrap_or_default().as_millis() as u64;
    
    let response = SearchResponse {
        took,
        timed_out: false,
        _shards: ShardInfo {
            total: indices.len() as u32,
            successful: indices.len() as u32,
            skipped: 0,
            failed: 0,
        },
        hits: HitsInfo {
            total: TotalHits {
                value: all_hits.len() as u64,
                relation: "eq".to_string(),
            },
            max_score: all_hits.first().and_then(|h| h._score),
            hits: all_hits,
        },
        aggregations: None,
    };
    
    // Cache the response
    api.cache.put(cache_key, response.clone());
    
    Json(response)
}

/// Search specific index
pub async fn search_index(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
    Json(request): Json<SearchRequest>,
) -> impl IntoResponse {
    let start = SystemTime::now();
    
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    let hits = search_in_index(&index, &state, &request).await
        .map_err(|e| search_error(e))?;
    
    // Execute aggregations if present
    let aggregations = if let Some(aggs) = request.aggs.as_ref().or(request.aggregations.as_ref()) {
        let query = match &request.query {
            Some(query_dsl) => build_tantivy_query(query_dsl, &state.schema)
                .map_err(|e| search_error(e))?,
            None => Box::new(AllQuery),
        };
        
        let agg_executor = crate::aggregations::AggregationExecutor::new(
            state.reader.clone(),
            state.schema.clone()
        );
        
        Some(agg_executor.execute(query, aggs.clone())
            .map_err(|e| search_error(Error::Internal(format!("Aggregation failed: {}", e))))?)
    } else {
        None
    };
    
    let took = start.elapsed().unwrap_or_default().as_millis() as u64;
    
    let response = SearchResponse {
        took,
        timed_out: false,
        _shards: ShardInfo {
            total: 1,
            successful: 1,
            skipped: 0,
            failed: 0,
        },
        hits: HitsInfo {
            total: TotalHits {
                value: hits.len() as u64,
                relation: "eq".to_string(),
            },
            max_score: hits.first().and_then(|h| h._score),
            hits,
        },
        aggregations,
    };
    
    Ok(Json(response))
}

/// Helper function to search within a single index
async fn search_in_index(
    index_name: &str,
    state: &crate::api::IndexState,
    request: &SearchRequest,
) -> Result<Vec<Hit>> {
    let searcher = state.reader.searcher();
    
    // Build Tantivy query from Elasticsearch query DSL
    let query = match &request.query {
        Some(query_dsl) => build_tantivy_query(query_dsl, &state.schema)?,
        None => Box::new(AllQuery),
    };
    
    // Execute search
    let top_docs = searcher.search(&*query, &TopDocs::with_limit(request.size).and_offset(request.from))
        .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;
    
    // Collect results
    let mut hits = Vec::new();
    for (score, doc_address) in top_docs {
        let doc: tantivy::TantivyDocument = searcher.doc(doc_address)
            .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))?;
        
        // Convert Tantivy document to JSON
        let mut source = serde_json::Map::new();
        for field in state.schema.fields() {
            let field_name = state.schema.get_field_name(field.0);
            {
                let field_values: Vec<_> = doc.get_all(field.0).collect();
                if !field_values.is_empty() {
                let json_values: Vec<serde_json::Value> = field_values.into_iter().filter_map(|v| {
                    // Convert Tantivy values to JSON
                    v.as_str().map(|s| serde_json::Value::String(s.to_string()))
                        .or_else(|| v.as_u64().map(|n| serde_json::Value::Number(n.into())))
                        .or_else(|| v.as_i64().map(|n| serde_json::Value::Number(n.into())))
                        .or_else(|| v.as_f64().and_then(|f| serde_json::Number::from_f64(f).map(serde_json::Value::Number)))
                }).collect();
                
                if json_values.len() == 1 {
                    source.insert(field_name.to_string(), json_values.into_iter().next().unwrap());
                } else if !json_values.is_empty() {
                    source.insert(field_name.to_string(), serde_json::Value::Array(json_values));
                }
                }
            }
        }
        
        // Generate document ID if not present
        let doc_id = source.get("_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&Uuid::new_v4().to_string())
            .to_string();
        
        // Generate highlights if requested
        let highlight = if request.highlight.is_some() {
            generate_highlights(&doc, &query, &state.schema, request.highlight.as_ref().unwrap())?
        } else {
            None
        };
        
        hits.push(Hit {
            _index: index_name.to_string(),
            _id: doc_id,
            _score: Some(score),
            _source: serde_json::Value::Object(source),
            highlight,
        });
    }
    
    Ok(hits)
}

/// Build Tantivy query from Elasticsearch query DSL
fn build_tantivy_query(
    query_dsl: &QueryDsl,
    schema: &tantivy::schema::Schema,
) -> Result<Box<dyn TantivyQuery>> {
    match query_dsl {
        QueryDsl::MatchAll(_) => Ok(Box::new(AllQuery)),
        
        QueryDsl::Match(match_query) => {
            // Get the field and value from the match query
            let (field_name, value) = match_query.field_value.iter().next()
                .ok_or_else(|| Error::InvalidInput("Match query must specify a field".to_string()))?;
            
            let field = schema.get_field(field_name)
                .map_err(|_| Error::InvalidInput(format!("Field {} not found", field_name)))?;
            
            // Use query parser for text fields
            let query_parser = QueryParser::for_index(&tantivy::Index::create_in_ram(schema.clone()), vec![field]);
            let query = query_parser.parse_query(&value.to_string())
                .map_err(|e| Error::InvalidInput(format!("Invalid query: {}", e)))?;
            
            Ok(query)
        },
        
        QueryDsl::Term(term_query) => {
            let (field_name, value) = term_query.field_value.iter().next()
                .ok_or_else(|| Error::InvalidInput("Term query must specify a field".to_string()))?;
            
            let field = schema.get_field(field_name)
                .map_err(|_| Error::InvalidInput(format!("Field {} not found", field_name)))?;
            
            let term = match value {
                serde_json::Value::String(s) => Term::from_field_text(field, s),
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Term::from_field_i64(field, i)
                    } else if let Some(u) = n.as_u64() {
                        Term::from_field_u64(field, u)
                    } else {
                        return Err(Error::InvalidInput("Invalid numeric value".to_string()));
                    }
                },
                _ => return Err(Error::InvalidInput("Term query value must be string or number".to_string())),
            };
            
            Ok(Box::new(TermQuery::new(term, Default::default())))
        },
        
        QueryDsl::Range(range_query) => {
            let (field_name, range_clause) = range_query.field_range.iter().next()
                .ok_or_else(|| Error::InvalidInput("Range query must specify a field".to_string()))?;
            
            let field = schema.get_field(field_name)
                .map_err(|_| Error::InvalidInput(format!("Field {} not found", field_name)))?;
            
            // For now, we'll create a simple numeric range query
            // In a full implementation, this would handle dates and other types
            let field_type = schema.get_field_entry(field).field_type();
            
            match field_type {
                FieldType::I64(_) | FieldType::U64(_) => {
                    // Extract bounds
                    let lower_bound = range_clause.gte.as_ref()
                        .or(range_clause.gt.as_ref())
                        .and_then(|v| v.as_i64());
                    
                    let upper_bound = range_clause.lte.as_ref()
                        .or(range_clause.lt.as_ref())
                        .and_then(|v| v.as_i64());
                    
                    if let (Some(lower), Some(upper)) = (lower_bound, upper_bound) {
                        let lower_term = Term::from_field_i64(field, lower);
                        let upper_term = Term::from_field_i64(field, upper);
                        Ok(Box::new(RangeQuery::new(
                            std::ops::Bound::Included(lower_term),
                            std::ops::Bound::Included(upper_term)
                        )))
                    } else {
                        Err(Error::InvalidInput("Range query must specify bounds".to_string()))
                    }
                },
                _ => Err(Error::InvalidInput("Range queries only supported for numeric fields".to_string())),
            }
        },
        
        QueryDsl::Bool(bool_query) => {
            let mut clauses = Vec::new();
            
            // Add must clauses
            for clause in &bool_query.must {
                let sub_query = build_tantivy_query(clause, schema)?;
                clauses.push((Occur::Must, sub_query));
            }
            
            // Add should clauses
            for clause in &bool_query.should {
                let sub_query = build_tantivy_query(clause, schema)?;
                clauses.push((Occur::Should, sub_query));
            }
            
            // Add must_not clauses
            for clause in &bool_query.must_not {
                let sub_query = build_tantivy_query(clause, schema)?;
                clauses.push((Occur::MustNot, sub_query));
            }
            
            // Add filter clauses (similar to must but don't affect scoring)
            for clause in &bool_query.filter {
                let sub_query = build_tantivy_query(clause, schema)?;
                clauses.push((Occur::Must, sub_query));
            }
            
            Ok(Box::new(BooleanQuery::new(clauses)))
        },
        
        QueryDsl::GeoDistance(geo_query) => {
            use crate::geo::{GeoQuery, GeoPoint, DistanceType, parse_geo_point, parse_distance_string};
            
            let center = parse_geo_point(&geo_query.center)?;
            let distance = parse_distance_string(&geo_query.distance)?;
            let distance_type = match geo_query.distance_type.as_deref() {
                Some("plane") => DistanceType::Plane,
                _ => DistanceType::Arc,
            };
            
            let query = GeoQuery::Distance {
                field: geo_query.field.clone(),
                center,
                distance,
                distance_type,
            };
            
            query.to_tantivy_query(schema)
        },
        
        QueryDsl::GeoBoundingBox(geo_query) => {
            use crate::geo::{GeoQuery, parse_geo_point};
            
            let top_left = parse_geo_point(&geo_query.top_left)?;
            let bottom_right = parse_geo_point(&geo_query.bottom_right)?;
            
            let query = GeoQuery::BoundingBox {
                field: geo_query.field.clone(),
                top_left,
                bottom_right,
            };
            
            query.to_tantivy_query(schema)
        },
        
        QueryDsl::GeoPolygon(geo_query) => {
            use crate::geo::{GeoQuery, parse_geo_point};
            
            let points = geo_query.points.iter()
                .map(|p| parse_geo_point(p))
                .collect::<Result<Vec<_>>>()?;
            
            let query = GeoQuery::Polygon {
                field: geo_query.field.clone(),
                points,
            };
            
            query.to_tantivy_query(schema)
        },
    }
}

/// Index or update a document
pub async fn index_document(
    Path((index, id)): Path<(String, String)>,
    State(api): State<Arc<SearchApi>>,
    Json(request): Json<IndexDocumentRequest>,
) -> impl IntoResponse {
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    // Add _id field to document
    let mut document = request.document;
    if let serde_json::Value::Object(ref mut map) = document {
        map.insert("_id".to_string(), serde_json::Value::String(id.clone()));
    }
    
    // Convert JSON to Tantivy document
    let tantivy_doc = json_to_tantivy_doc(&document, &state.schema, &state.mapping)
        .map_err(|e| indexing_error(e))?;
    
    // Add document to index
    let mut writer = state.writer.write().await;
    writer.add_document(tantivy_doc)
        .map_err(|e| Error::Internal(format!("Failed to index document: {}", e)))
        .map_err(|e| indexing_error(e))?;
    
    // Commit immediately for consistency
    writer.commit()
        .map_err(|e| Error::Internal(format!("Failed to commit: {}", e)))
        .map_err(|e| indexing_error(e))?;
    
    let response = IndexDocumentResponse {
        _index: index,
        _id: id,
        _version: 1,
        result: "created".to_string(),
        _shards: ShardInfo {
            total: 1,
            successful: 1,
            skipped: 0,
            failed: 0,
        },
        _seq_no: 0,
        _primary_term: 1,
    };
    
    Ok((StatusCode::CREATED, Json(response)))
}

/// Get a document by ID
pub async fn get_document(
    Path((index, id)): Path<(String, String)>,
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    // Search for document with matching _id
    let searcher = state.reader.searcher();
    let id_field = state.schema.get_field("_id")
        .map_err(|_| Error::Internal("_id field not found".to_string()))
        .map_err(|e| search_error(e))?;
    
    let term = Term::from_field_text(id_field, &id);
    let query = TermQuery::new(term, Default::default());
    
    let top_docs = searcher.search(&query, &TopDocs::with_limit(1))
        .map_err(|e| Error::Internal(format!("Search failed: {}", e)))
        .map_err(|e| search_error(e))?;
    
    if let Some((_score, doc_address)) = top_docs.first() {
        let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)
            .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))
            .map_err(|e| search_error(e))?;
        
        // Convert to JSON
        let mut source = serde_json::Map::new();
        for field in state.schema.fields() {
            let field_name = state.schema.get_field_name(field.0);
            
            if field_name == "_id" {
                continue; // Skip internal _id field
            }
            
            {
                let field_values: Vec<_> = doc.get_all(field.0).collect();
                if !field_values.is_empty() {
                let json_values: Vec<serde_json::Value> = field_values.into_iter().filter_map(|v| {
                    // Convert Tantivy values to JSON
                    v.as_str().map(|s| serde_json::Value::String(s.to_string()))
                        .or_else(|| v.as_u64().map(|n| serde_json::Value::Number(n.into())))
                        .or_else(|| v.as_i64().map(|n| serde_json::Value::Number(n.into())))
                        .or_else(|| v.as_f64().and_then(|f| serde_json::Number::from_f64(f).map(serde_json::Value::Number)))
                }).collect();
                
                if json_values.len() == 1 {
                    source.insert(field_name.to_string(), json_values.into_iter().next().unwrap());
                } else if !json_values.is_empty() {
                    source.insert(field_name.to_string(), serde_json::Value::Array(json_values));
                }
                }
            }
        }
        
        let response = GetDocumentResponse {
            _index: index,
            _id: id,
            _version: 1,
            _seq_no: 0,
            _primary_term: 1,
            found: true,
            _source: Some(serde_json::Value::Object(source)),
        };
        
        Ok(Json(response))
    } else {
        let response = GetDocumentResponse {
            _index: index,
            _id: id,
            _version: 1,
            _seq_no: 0,
            _primary_term: 1,
            found: false,
            _source: None,
        };
        
        Ok(Json(response))
    }
}

/// Delete a document
pub async fn delete_document(
    Path((index, id)): Path<(String, String)>,
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    // Delete document by _id
    let id_field = state.schema.get_field("_id")
        .map_err(|_| Error::Internal("_id field not found".to_string()))
        .map_err(|e| deletion_error(e))?;
    
    let term = Term::from_field_text(id_field, &id);
    
    let mut writer = state.writer.write().await;
    writer.delete_term(term);
    writer.commit()
        .map_err(|e| Error::Internal(format!("Failed to commit: {}", e)))
        .map_err(|e| deletion_error(e))?;
    
    let response = DeleteDocumentResponse {
        _index: index,
        _id: id,
        _version: 1,
        result: "deleted".to_string(),
        _shards: ShardInfo {
            total: 1,
            successful: 1,
            skipped: 0,
            failed: 0,
        },
        _seq_no: 0,
        _primary_term: 1,
    };
    
    Ok(Json(response))
}

/// Create an index
pub async fn create_index(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Extract mappings from request body
    let mapping = if let Some(mappings) = body.get("mappings") {
        match serde_json::from_value::<IndexMapping>(mappings.clone()) {
            Ok(m) => m,
            Err(e) => return Err(mapping_error(Error::InvalidInput(format!("Invalid mapping: {}", e)))),
        }
    } else {
        // Default mapping with _id field
        let mut properties = HashMap::new();
        properties.insert("_id".to_string(), FieldMapping {
            field_type: "keyword".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        IndexMapping { properties }
    };
    
    match api.create_index_with_mapping(index.clone(), mapping).await {
        Ok(_) => {},
        Err(e) => return Err(index_creation_error(e)),
    }
    
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "acknowledged": true,
            "shards_acknowledged": true,
            "index": index
        }))
    ))
}

/// Delete an index
pub async fn delete_index(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    if api.indices.remove(&index).is_none() {
        return Err(index_not_found_error(&index));
    }
    
    Ok(Json(serde_json::json!({
        "acknowledged": true
    })))
}

/// Get index mapping
pub async fn get_mapping(
    Path(index): Path<String>,
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    let state = match api.indices.get(&index) {
        Some(state) => state,
        None => return Err(index_not_found_error(&index)),
    };
    
    Ok(Json(serde_json::json!({
        index: {
            "mappings": state.mapping.clone()
        }
    })))
}

/// List all indices
pub async fn cat_indices(
    State(api): State<Arc<SearchApi>>,
) -> impl IntoResponse {
    let mut indices = Vec::new();
    
    for entry in api.indices.iter() {
        let index_name = entry.key();
        let state = entry.value();
        
        // Get document count
        let searcher = state.reader.searcher();
        let doc_count = searcher.num_docs();
        
        indices.push(CatIndexInfo {
            health: "green".to_string(),
            status: "open".to_string(),
            index: index_name.clone(),
            uuid: Uuid::new_v4().to_string(),
            pri: 1,
            rep: 0,
            docs_count: doc_count,
            docs_deleted: 0,
            store_size: format!("{}kb", doc_count / 10), // Rough estimate
            pri_store_size: format!("{}kb", doc_count / 10),
        });
    }
    
    Json(indices)
}

// Helper functions

fn json_to_tantivy_doc(
    json: &serde_json::Value,
    schema: &tantivy::schema::Schema,
    mapping: &IndexMapping,
) -> Result<tantivy::TantivyDocument> {
    let mut doc = tantivy::TantivyDocument::new();
    
    if let serde_json::Value::Object(map) = json {
        for (field_name, value) in map {
            if let Some(field_mapping) = mapping.properties.get(field_name) {
                if let Ok(field) = schema.get_field(field_name) {
                    match field_mapping.field_type.as_str() {
                        "text" | "keyword" => {
                            if let serde_json::Value::String(s) = value {
                                doc.add_text(field, s);
                            }
                        },
                        "long" | "integer" => {
                            if let Some(n) = value.as_i64() {
                                doc.add_i64(field, n);
                            }
                        },
                        "geo_point" => {
                            use crate::geo::parse_geo_point;
                            
                            if let Ok(point) = parse_geo_point(value) {
                                // Add lat and lon as separate fields
                                if let Ok(lat_field) = schema.get_field(&format!("{}_lat", field_name)) {
                                    doc.add_f64(lat_field, point.lat);
                                }
                                if let Ok(lon_field) = schema.get_field(&format!("{}_lon", field_name)) {
                                    doc.add_f64(lon_field, point.lon);
                                }
                            }
                        },
                        _ => {},
                    }
                }
            }
        }
    }
    
    Ok(doc)
}


// Error response helpers

fn error_response(error_type: &str, reason: &str, status: StatusCode) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            error: ErrorInfo {
                error_type: error_type.to_string(),
                reason: reason.to_string(),
                caused_by: None,
            },
            status: status.as_u16(),
        })
    )
}

fn index_not_found_error(index: &str) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "index_not_found_exception",
        &format!("no such index [{}]", index),
        StatusCode::NOT_FOUND
    )
}

fn search_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "search_phase_execution_exception",
        &error.to_string(),
        StatusCode::BAD_REQUEST
    )
}

fn indexing_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "mapper_parsing_exception",
        &error.to_string(),
        StatusCode::BAD_REQUEST
    )
}

fn deletion_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "version_conflict_engine_exception",
        &error.to_string(),
        StatusCode::CONFLICT
    )
}

fn mapping_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "mapper_parsing_exception",
        &error.to_string(),
        StatusCode::BAD_REQUEST
    )
}

fn index_creation_error(error: Error) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        "resource_already_exists_exception",
        &error.to_string(),
        StatusCode::BAD_REQUEST
    )
}

/// Generate highlights for matching fields
fn generate_highlights(
    _doc: &tantivy::TantivyDocument,
    _query: &Box<dyn TantivyQuery>,
    _schema: &tantivy::schema::Schema,
    _config: &HighlightConfig,
) -> Result<Option<HashMap<String, Vec<String>>>> {
    // TODO: Implement highlighting with updated Tantivy API
    // The snippet API has changed in recent versions of Tantivy
    // For now, return None to allow compilation
    Ok(None)
}