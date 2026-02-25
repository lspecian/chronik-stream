//! Query translation layer that converts various query formats to Tantivy queries.
//!
//! This module provides comprehensive query translation supporting:
//! - Elasticsearch-compatible JSON query DSL
//! - SQL-like queries for Kafka data
//! - Custom Kafka query patterns
//!
//! The translator handles field searches, range queries, boolean logic, wildcards,
//! phrase searches, and more complex query constructs.

use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tantivy::{
    query::{
        AllQuery, BooleanQuery, Occur, Query as TantivyQuery, TermQuery, 
        RangeQuery, RegexQuery, PhraseQuery, FuzzyTermQuery, BoostQuery,
    },
    schema::{Field, Schema, FieldType},
    Term,
};
use tracing::warn;

/// Main query translator that handles multiple query formats
pub struct QueryTranslator {
    schema: Schema,
    field_cache: HashMap<String, Field>,
}

impl QueryTranslator {
    /// Create a new query translator with the given schema
    pub fn new(schema: Schema) -> Self {
        let mut field_cache = HashMap::new();
        
        // Cache all fields for faster lookup
        for (field, field_entry) in schema.fields() {
            field_cache.insert(field_entry.name().to_string(), field);
        }
        
        Self {
            schema,
            field_cache,
        }
    }
    
    /// Translate a query from various formats to Tantivy query
    pub fn translate(&self, query: &QueryInput) -> Result<Box<dyn TantivyQuery>> {
        match query {
            QueryInput::Json(json) => self.translate_json_query(json),
            QueryInput::Sql(sql) => self.translate_sql_query(sql),
            QueryInput::Simple(text) => self.translate_simple_query(text),
            QueryInput::Kafka(kafka_query) => self.translate_kafka_query(kafka_query),
        }
    }
    
    /// Translate JSON query DSL (Elasticsearch-compatible)
    pub fn translate_json_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        match query {
            Value::Object(map) => {
                // Check for query types
                if let Some(match_query) = map.get("match") {
                    self.translate_match_query(match_query)
                } else if let Some(_match_all) = map.get("match_all") {
                    Ok(Box::new(AllQuery))
                } else if let Some(term_query) = map.get("term") {
                    self.translate_term_query(term_query)
                } else if let Some(terms_query) = map.get("terms") {
                    self.translate_terms_query(terms_query)
                } else if let Some(range_query) = map.get("range") {
                    self.translate_range_query(range_query)
                } else if let Some(bool_query) = map.get("bool") {
                    self.translate_bool_query(bool_query)
                } else if let Some(wildcard_query) = map.get("wildcard") {
                    self.translate_wildcard_query(wildcard_query)
                } else if let Some(regexp_query) = map.get("regexp") {
                    self.translate_regexp_query(regexp_query)
                } else if let Some(phrase_query) = map.get("match_phrase") {
                    self.translate_phrase_query(phrase_query)
                } else if let Some(fuzzy_query) = map.get("fuzzy") {
                    self.translate_fuzzy_query(fuzzy_query)
                } else if let Some(exists_query) = map.get("exists") {
                    self.translate_exists_query(exists_query)
                } else {
                    Err(Error::InvalidInput(format!(
                        "Unsupported query type: {:?}",
                        map.keys().collect::<Vec<_>>()
                    )))
                }
            }
            _ => Err(Error::InvalidInput("Query must be a JSON object".to_string())),
        }
    }
    
    /// Translate match query
    fn translate_match_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let query_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Match query must be an object".to_string()))?;
        
        let (field_name, value) = query_obj.iter().next()
            .ok_or_else(|| Error::InvalidInput("Match query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        
        // Handle different value formats
        match value {
            Value::String(text) => {
                // For text fields, we'll use a phrase query if multiple words
                let terms: Vec<&str> = text.split_whitespace().collect();
                if terms.len() > 1 {
                    self.create_phrase_query(field, &terms)
                } else {
                    self.create_term_query(field, text)
                }
            }
            Value::Object(options) => {
                // Extended match query with options
                let query_text = options.get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::InvalidInput("Match query must have 'query' field".to_string()))?;
                
                let operator = options.get("operator")
                    .and_then(|v| v.as_str())
                    .unwrap_or("or");
                
                let boost = options.get("boost")
                    .and_then(|v| v.as_f64())
                    .map(|b| b as f32);
                
                let base_query = self.create_match_query_with_operator(field, query_text, operator)?;
                
                if let Some(boost_val) = boost {
                    Ok(Box::new(BoostQuery::new(base_query, boost_val)))
                } else {
                    Ok(base_query)
                }
            }
            _ => Err(Error::InvalidInput("Match query value must be string or object".to_string())),
        }
    }
    
    /// Create match query with operator (AND/OR)
    #[allow(unused_variables)]
    fn create_match_query_with_operator(
        &self,
        field: Field,
        text: &str,
        operator: &str,
    ) -> Result<Box<dyn TantivyQuery>> {
        let terms: Vec<&str> = text.split_whitespace().collect();
        
        if terms.is_empty() {
            return Ok(Box::new(AllQuery));
        }
        
        if terms.len() == 1 {
            return self.create_term_query(field, terms[0]);
        }
        
        let mut subqueries = Vec::new();
        let occur = match operator.to_lowercase().as_str() {
            "and" => Occur::Must,
            "or" => Occur::Should,
            _ => Occur::Should,
        };
        
        for term in terms {
            let term_query = self.create_term_query(field, term)?;
            subqueries.push((occur.clone(), term_query));
        }
        
        Ok(Box::new(BooleanQuery::new(subqueries)))
    }
    
    /// Translate term query
    fn translate_term_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let query_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Term query must be an object".to_string()))?;
        
        let (field_name, value) = query_obj.iter().next()
            .ok_or_else(|| Error::InvalidInput("Term query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        
        match value {
            Value::String(s) => self.create_term_query(field, s),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Box::new(TermQuery::new(
                        Term::from_field_i64(field, i),
                        Default::default(),
                    )))
                } else if let Some(u) = n.as_u64() {
                    Ok(Box::new(TermQuery::new(
                        Term::from_field_u64(field, u),
                        Default::default(),
                    )))
                } else {
                    Err(Error::InvalidInput("Invalid numeric value".to_string()))
                }
            }
            Value::Bool(b) => {
                let term = if *b { "true" } else { "false" };
                self.create_term_query(field, term)
            }
            _ => Err(Error::InvalidInput("Term query value must be string, number, or boolean".to_string())),
        }
    }
    
    /// Translate terms query (match any of multiple values)
    fn translate_terms_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let query_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Terms query must be an object".to_string()))?;
        
        let (field_name, values) = query_obj.iter().next()
            .ok_or_else(|| Error::InvalidInput("Terms query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        let values_array = values.as_array()
            .ok_or_else(|| Error::InvalidInput("Terms query values must be an array".to_string()))?;
        
        if values_array.is_empty() {
            return Ok(Box::new(AllQuery));
        }
        
        let mut subqueries = Vec::new();
        
        for value in values_array {
            match value {
                Value::String(s) => {
                    let term_query = self.create_term_query(field, s)?;
                    subqueries.push((Occur::Should, term_query));
                }
                Value::Number(n) => {
                    let term_query = if let Some(i) = n.as_i64() {
                        Box::new(TermQuery::new(Term::from_field_i64(field, i), Default::default()))
                    } else if let Some(u) = n.as_u64() {
                        Box::new(TermQuery::new(Term::from_field_u64(field, u), Default::default()))
                    } else {
                        continue;
                    };
                    subqueries.push((Occur::Should, term_query));
                }
                _ => continue,
            }
        }
        
        Ok(Box::new(BooleanQuery::new(subqueries)))
    }
    
    /// Translate range query
    fn translate_range_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let query_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Range query must be an object".to_string()))?;
        
        let (field_name, range_spec) = query_obj.iter().next()
            .ok_or_else(|| Error::InvalidInput("Range query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        let range_obj = range_spec.as_object()
            .ok_or_else(|| Error::InvalidInput("Range specification must be an object".to_string()))?;
        
        // Check field type
        let field_entry = self.schema.get_field_entry(field);
        match field_entry.field_type() {
            FieldType::I64(_) => self.create_i64_range_query(field, field_name, range_obj),
            FieldType::U64(_) => self.create_u64_range_query(field, field_name, range_obj),
            FieldType::F64(_) => self.create_f64_range_query(field, field_name, range_obj),
            FieldType::Date(_) => self.create_date_range_query(field, field_name, range_obj),
            _ => Err(Error::InvalidInput(format!(
                "Range queries not supported for field type of '{}'",
                field_name
            ))),
        }
    }
    
    /// Create i64 range query
    fn create_i64_range_query(
        &self,
        field: Field,
        field_name: &str,
        range_obj: &serde_json::Map<String, Value>,
    ) -> Result<Box<dyn TantivyQuery>> {
        let mut lower_bound: Option<i64> = None;
        let mut upper_bound: Option<i64> = None;
        let mut include_lower = true;
        let mut include_upper = true;
        
        // Parse bounds
        if let Some(gte) = range_obj.get("gte").and_then(|v| v.as_i64()) {
            lower_bound = Some(gte);
            include_lower = true;
        } else if let Some(gt) = range_obj.get("gt").and_then(|v| v.as_i64()) {
            lower_bound = Some(gt);
            include_lower = false;
        }
        
        if let Some(lte) = range_obj.get("lte").and_then(|v| v.as_i64()) {
            upper_bound = Some(lte);
            include_upper = true;
        } else if let Some(lt) = range_obj.get("lt").and_then(|v| v.as_i64()) {
            upper_bound = Some(lt);
            include_upper = false;
        }
        
        // Create range query using Bound<Term> (tantivy 0.24 API)
        use std::ops::Bound;

        let lower = match lower_bound {
            Some(val) => {
                let term = Term::from_field_i64(field, if include_lower { val } else { val + 1 });
                Bound::Included(term)
            }
            None => Bound::Unbounded,
        };

        let upper = match upper_bound {
            Some(val) => {
                let term = Term::from_field_i64(field, if include_upper { val } else { val - 1 });
                Bound::Included(term)
            }
            None => Bound::Unbounded,
        };

        if matches!((&lower, &upper), (Bound::Unbounded, Bound::Unbounded)) {
            return Err(Error::InvalidInput("Range query must specify at least one bound".to_string()));
        }

        Ok(Box::new(RangeQuery::new(lower, upper)))
    }

    /// Create u64 range query
    fn create_u64_range_query(
        &self,
        field: Field,
        field_name: &str,
        range_obj: &serde_json::Map<String, Value>,
    ) -> Result<Box<dyn TantivyQuery>> {
        let mut lower_bound: Option<u64> = None;
        let mut upper_bound: Option<u64> = None;
        let mut include_lower = true;
        let mut include_upper = true;

        // Parse bounds
        if let Some(gte) = range_obj.get("gte").and_then(|v| v.as_u64()) {
            lower_bound = Some(gte);
            include_lower = true;
        } else if let Some(gt) = range_obj.get("gt").and_then(|v| v.as_u64()) {
            lower_bound = Some(gt);
            include_lower = false;
        }

        if let Some(lte) = range_obj.get("lte").and_then(|v| v.as_u64()) {
            upper_bound = Some(lte);
            include_upper = true;
        } else if let Some(lt) = range_obj.get("lt").and_then(|v| v.as_u64()) {
            upper_bound = Some(lt);
            include_upper = false;
        }

        // Create range query using Bound<Term> (tantivy 0.24 API)
        use std::ops::Bound;

        let lower = match lower_bound {
            Some(val) => {
                let term = Term::from_field_u64(field, if include_lower { val } else { val + 1 });
                Bound::Included(term)
            }
            None => Bound::Unbounded,
        };

        let upper = match upper_bound {
            Some(val) => {
                let term = Term::from_field_u64(field, if include_upper { val } else { val - 1 });
                Bound::Included(term)
            }
            None => Bound::Unbounded,
        };

        if matches!((&lower, &upper), (Bound::Unbounded, Bound::Unbounded)) {
            return Err(Error::InvalidInput("Range query must specify at least one bound".to_string()));
        }

        Ok(Box::new(RangeQuery::new(lower, upper)))
    }
    
    /// Create f64 range query
    fn create_f64_range_query(
        &self,
        field: Field,
        _field_name: &str,
        range_obj: &serde_json::Map<String, Value>,
    ) -> Result<Box<dyn TantivyQuery>> {
        let mut lower_bound: Option<f64> = None;
        let mut upper_bound: Option<f64> = None;
        let mut include_lower = true;
        let mut include_upper = true;

        if let Some(gte) = range_obj.get("gte").and_then(|v| v.as_f64()) {
            lower_bound = Some(gte);
            include_lower = true;
        } else if let Some(gt) = range_obj.get("gt").and_then(|v| v.as_f64()) {
            lower_bound = Some(gt);
            include_lower = false;
        }

        if let Some(lte) = range_obj.get("lte").and_then(|v| v.as_f64()) {
            upper_bound = Some(lte);
            include_upper = true;
        } else if let Some(lt) = range_obj.get("lt").and_then(|v| v.as_f64()) {
            upper_bound = Some(lt);
            include_upper = false;
        }

        use std::ops::Bound;

        let lower = match lower_bound {
            Some(val) => {
                if include_lower {
                    Bound::Included(Term::from_field_f64(field, val))
                } else {
                    Bound::Excluded(Term::from_field_f64(field, val))
                }
            }
            None => Bound::Unbounded,
        };

        let upper = match upper_bound {
            Some(val) => {
                if include_upper {
                    Bound::Included(Term::from_field_f64(field, val))
                } else {
                    Bound::Excluded(Term::from_field_f64(field, val))
                }
            }
            None => Bound::Unbounded,
        };

        if matches!((&lower, &upper), (Bound::Unbounded, Bound::Unbounded)) {
            return Err(Error::InvalidInput("Range query must specify at least one bound".to_string()));
        }

        Ok(Box::new(RangeQuery::new(lower, upper)))
    }
    
    /// Create date range query
    fn create_date_range_query(
        &self,
        field: Field,
        field_name: &str,
        range_obj: &serde_json::Map<String, Value>,
    ) -> Result<Box<dyn TantivyQuery>> {
        // For now, assume dates are stored as i64 timestamps
        self.create_i64_range_query(field, field_name, range_obj)
    }
    
    /// Translate bool query
    fn translate_bool_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let bool_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Bool query must be an object".to_string()))?;
        
        let mut subqueries = Vec::new();
        
        // Process must clauses
        if let Some(must_clauses) = bool_obj.get("must").and_then(|v| v.as_array()) {
            for clause in must_clauses {
                let sub_query = self.translate_json_query(clause)?;
                subqueries.push((Occur::Must, sub_query));
            }
        }
        
        // Process should clauses
        if let Some(should_clauses) = bool_obj.get("should").and_then(|v| v.as_array()) {
            for clause in should_clauses {
                let sub_query = self.translate_json_query(clause)?;
                subqueries.push((Occur::Should, sub_query));
            }
        }
        
        // Process must_not clauses
        if let Some(must_not_clauses) = bool_obj.get("must_not").and_then(|v| v.as_array()) {
            for clause in must_not_clauses {
                let sub_query = self.translate_json_query(clause)?;
                subqueries.push((Occur::MustNot, sub_query));
            }
        }
        
        // Process filter clauses (treated as must but don't affect scoring)
        if let Some(filter_clauses) = bool_obj.get("filter").and_then(|v| v.as_array()) {
            for clause in filter_clauses {
                let sub_query = self.translate_json_query(clause)?;
                subqueries.push((Occur::Must, sub_query));
            }
        }
        
        // Handle minimum_should_match
        if let Some(_min_should_match) = bool_obj.get("minimum_should_match") {
            // This would require custom scoring logic in Tantivy
            // For now, we'll log a warning
            warn!("minimum_should_match is not yet supported in bool queries");
        }
        
        Ok(Box::new(BooleanQuery::new(subqueries)))
    }
    
    /// Translate wildcard query
    fn translate_wildcard_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let query_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Wildcard query must be an object".to_string()))?;
        
        let (field_name, pattern) = query_obj.iter().next()
            .ok_or_else(|| Error::InvalidInput("Wildcard query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        let pattern_str = pattern.as_str()
            .ok_or_else(|| Error::InvalidInput("Wildcard pattern must be a string".to_string()))?;
        
        // Convert wildcard pattern to regex
        let regex_pattern = self.wildcard_to_regex(pattern_str);
        
        Ok(Box::new(RegexQuery::from_pattern(&regex_pattern, field)
            .map_err(|e| Error::InvalidInput(format!("Invalid regex pattern: {}", e)))?))
    }
    
    /// Translate regexp query
    fn translate_regexp_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let query_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Regexp query must be an object".to_string()))?;
        
        let (field_name, pattern) = query_obj.iter().next()
            .ok_or_else(|| Error::InvalidInput("Regexp query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        let pattern_str = pattern.as_str()
            .ok_or_else(|| Error::InvalidInput("Regexp pattern must be a string".to_string()))?;
        
        Ok(Box::new(RegexQuery::from_pattern(pattern_str, field)
            .map_err(|e| Error::InvalidInput(format!("Invalid regex pattern: {}", e)))?))
    }
    
    /// Translate match_phrase query
    fn translate_phrase_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let query_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Match phrase query must be an object".to_string()))?;
        
        let (field_name, phrase) = query_obj.iter().next()
            .ok_or_else(|| Error::InvalidInput("Match phrase query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        let phrase_str = phrase.as_str()
            .ok_or_else(|| Error::InvalidInput("Phrase must be a string".to_string()))?;
        
        let terms: Vec<&str> = phrase_str.split_whitespace().collect();
        self.create_phrase_query(field, &terms)
    }
    
    /// Translate fuzzy query
    fn translate_fuzzy_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let query_obj = query.as_object()
            .ok_or_else(|| Error::InvalidInput("Fuzzy query must be an object".to_string()))?;
        
        let (field_name, value) = query_obj.iter().next()
            .ok_or_else(|| Error::InvalidInput("Fuzzy query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        
        match value {
            Value::String(text) => {
                let term = Term::from_field_text(field, text);
                Ok(Box::new(FuzzyTermQuery::new(term, 2, true))) // distance of 2
            }
            Value::Object(options) => {
                let text = options.get("value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::InvalidInput("Fuzzy query must have 'value' field".to_string()))?;
                
                let distance = options.get("fuzziness")
                    .and_then(|v| v.as_u64())
                    .map(|d| d as u8)
                    .unwrap_or(2);
                
                let prefix_length = options.get("prefix_length")
                    .and_then(|v| v.as_u64())
                    .map(|p| p as usize)
                    .unwrap_or(0);
                
                let term = Term::from_field_text(field, text);
                Ok(Box::new(FuzzyTermQuery::new_prefix(term, distance, prefix_length != 0)))
            }
            _ => Err(Error::InvalidInput("Fuzzy query value must be string or object".to_string())),
        }
    }
    
    /// Translate exists query
    fn translate_exists_query(&self, query: &Value) -> Result<Box<dyn TantivyQuery>> {
        let field_name = query.get("field")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::InvalidInput("Exists query must specify a field".to_string()))?;
        
        let field = self.get_field(field_name)?;
        
        // For exists query, we need to find documents where the field has any value
        // Use ExistsQuery approach: match any content with a character class
        Ok(Box::new(RegexQuery::from_pattern("[\\s\\S]+", field)
            .map_err(|e| Error::InvalidInput(format!("Failed to create exists query: {}", e)))?))
    }
    
    /// Translate SQL-like query to Tantivy query
    pub fn translate_sql_query(&self, sql: &str) -> Result<Box<dyn TantivyQuery>> {
        // This is a simplified SQL parser
        // In a production system, you'd want a proper SQL parser
        
        let sql_lower = sql.to_lowercase();
        
        // Extract WHERE clause
        if let Some(where_pos) = sql_lower.find("where") {
            let where_clause = &sql[where_pos + 5..].trim();
            self.parse_sql_where_clause(where_clause)
        } else {
            // No WHERE clause means match all
            Ok(Box::new(AllQuery))
        }
    }
    
    /// Parse SQL WHERE clause
    fn parse_sql_where_clause(&self, where_clause: &str) -> Result<Box<dyn TantivyQuery>> {
        // This is a very simplified parser
        // It handles basic conditions like:
        // - field = 'value'
        // - field > 100
        // - field LIKE '%pattern%'
        // - field IN ('a', 'b', 'c')
        // - Combinations with AND/OR
        
        let tokens = self.tokenize_sql(where_clause);
        self.parse_sql_tokens(&tokens)
    }
    
    /// Tokenize SQL string
    fn tokenize_sql(&self, sql: &str) -> Vec<String> {
        // Simple tokenizer that handles quoted strings
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut quote_char = ' ';
        
        for ch in sql.chars() {
            match ch {
                '\'' | '"' if !in_quotes => {
                    if !current.is_empty() {
                        tokens.push(current.trim().to_string());
                        current.clear();
                    }
                    in_quotes = true;
                    quote_char = ch;
                }
                ch if ch == quote_char && in_quotes => {
                    tokens.push(format!("'{}'", current));
                    current.clear();
                    in_quotes = false;
                }
                ' ' | '\t' | '\n' if !in_quotes => {
                    if !current.is_empty() {
                        tokens.push(current.trim().to_string());
                        current.clear();
                    }
                }
                _ => current.push(ch),
            }
        }
        
        if !current.is_empty() {
            tokens.push(current.trim().to_string());
        }
        
        tokens
    }
    
    /// Parse SQL tokens into Tantivy query
    fn parse_sql_tokens(&self, tokens: &[String]) -> Result<Box<dyn TantivyQuery>> {
        if tokens.is_empty() {
            return Ok(Box::new(AllQuery));
        }
        
        // Look for AND/OR operators to split into subqueries
        let mut subqueries = Vec::new();
        let mut i = 0;
        let mut current_condition = Vec::new();
        let mut last_operator = "AND".to_string();
        
        let mut in_between = false;
        while i < tokens.len() {
            let token = &tokens[i];
            let upper = token.to_uppercase();

            // Track BETWEEN state: "field BETWEEN x AND y" — the AND is part of BETWEEN
            if upper == "BETWEEN" {
                in_between = true;
                current_condition.push(token.clone());
            } else if in_between && upper == "AND" {
                // This AND belongs to BETWEEN...AND, not a boolean operator
                current_condition.push(token.clone());
                in_between = false;
            } else if (upper == "AND" || upper == "OR") && !in_between {
                // Process current condition
                if !current_condition.is_empty() {
                    let sub_query = self.parse_sql_condition(&current_condition)?;
                    let occur = match last_operator.as_str() {
                        "AND" => Occur::Must,
                        "OR" => Occur::Should,
                        _ => Occur::Must,
                    };
                    subqueries.push((occur, sub_query));
                    current_condition.clear();
                }
                last_operator = upper;
            } else {
                current_condition.push(token.clone());
            }

            i += 1;
        }
        
        // Process final condition
        if !current_condition.is_empty() {
            let sub_query = self.parse_sql_condition(&current_condition)?;
            let occur = match last_operator.as_str() {
                "AND" => Occur::Must,
                "OR" => Occur::Should,
                _ => Occur::Must,
            };
            subqueries.push((occur, sub_query));
        }
        
        Ok(Box::new(BooleanQuery::new(subqueries)))
    }
    
    /// Parse a single SQL condition
    fn parse_sql_condition(&self, tokens: &[String]) -> Result<Box<dyn TantivyQuery>> {
        if tokens.len() < 3 {
            return Err(Error::InvalidInput("Invalid SQL condition".to_string()));
        }
        
        let field_name = &tokens[0];
        let operator = &tokens[1].to_uppercase();
        
        let field = self.get_field(field_name)?;
        
        match operator.as_str() {
            "=" | "==" => {
                let value = self.parse_sql_value(&tokens[2])?;
                self.create_term_query_from_value(field, &value)
            }
            "!=" | "<>" => {
                let value = self.parse_sql_value(&tokens[2])?;
                let term_query = self.create_term_query_from_value(field, &value)?;
                let mut subqueries = Vec::new();
                subqueries.push((Occur::Must, Box::new(AllQuery) as Box<dyn TantivyQuery>));
                subqueries.push((Occur::MustNot, term_query));
                Ok(Box::new(BooleanQuery::new(subqueries)))
            }
            ">" | ">=" | "<" | "<=" => {
                let value = self.parse_sql_value(&tokens[2])?;
                self.create_sql_range_query(field, field_name, operator, &value)
            }
            "LIKE" => {
                let pattern = self.parse_sql_value(&tokens[2])?;
                if let Value::String(s) = pattern {
                    let regex_pattern = self.sql_like_to_regex(&s);
                    Ok(Box::new(RegexQuery::from_pattern(&regex_pattern, field)
                        .map_err(|e| Error::InvalidInput(format!("Invalid pattern: {}", e)))?))
                } else {
                    Err(Error::InvalidInput("LIKE pattern must be a string".to_string()))
                }
            }
            "IN" => {
                // Parse IN clause: field IN ('a', 'b', 'c')
                let values = self.parse_sql_in_values(&tokens[2..])?;
                self.create_in_query(field, values)
            }
            "BETWEEN" => {
                // Parse BETWEEN clause: field BETWEEN 1 AND 10
                if tokens.len() < 5 || tokens[3].to_uppercase() != "AND" {
                    return Err(Error::InvalidInput("Invalid BETWEEN syntax".to_string()));
                }
                let lower = self.parse_sql_value(&tokens[2])?;
                let upper = self.parse_sql_value(&tokens[4])?;
                self.create_between_query(field, field_name, lower, upper)
            }
            _ => Err(Error::InvalidInput(format!("Unsupported SQL operator: {}", operator))),
        }
    }
    
    /// Parse SQL value (handle quoted strings, numbers, etc.)
    fn parse_sql_value(&self, token: &str) -> Result<Value> {
        if token.starts_with('\'') && token.ends_with('\'') {
            // Quoted string
            Ok(Value::String(token[1..token.len()-1].to_string()))
        } else if let Ok(n) = token.parse::<i64>() {
            Ok(Value::Number(n.into()))
        } else if let Ok(f) = token.parse::<f64>() {
            match serde_json::Number::from_f64(f) {
                Some(n) => Ok(Value::Number(n)),
                None => Err(Error::InvalidInput(format!("Invalid numeric value: {}", token))),
            }
        } else if token.to_lowercase() == "true" {
            Ok(Value::Bool(true))
        } else if token.to_lowercase() == "false" {
            Ok(Value::Bool(false))
        } else if token.to_lowercase() == "null" {
            Ok(Value::Null)
        } else {
            // Unquoted string
            Ok(Value::String(token.to_string()))
        }
    }
    
    /// Parse SQL IN values
    fn parse_sql_in_values(&self, tokens: &[String]) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        let mut in_parens = false;
        
        for token in tokens {
            if token.starts_with('(') {
                in_parens = true;
                let clean_token = token.trim_start_matches('(').trim_end_matches(',');
                if !clean_token.is_empty() {
                    values.push(self.parse_sql_value(clean_token)?);
                }
            } else if token.ends_with(')') {
                let clean_token = token.trim_end_matches(')').trim_end_matches(',');
                if !clean_token.is_empty() {
                    values.push(self.parse_sql_value(clean_token)?);
                }
                break;
            } else if in_parens {
                let clean_token = token.trim_end_matches(',');
                if !clean_token.is_empty() {
                    values.push(self.parse_sql_value(clean_token)?);
                }
            }
        }
        
        Ok(values)
    }
    
    /// Create SQL range query
    fn create_sql_range_query(
        &self,
        field: Field,
        field_name: &str,
        operator: &str,
        value: &Value,
    ) -> Result<Box<dyn TantivyQuery>> {
        let mut range_spec = serde_json::Map::new();
        
        match operator {
            ">" => {
                range_spec.insert("gt".to_string(), value.clone());
            }
            ">=" => {
                range_spec.insert("gte".to_string(), value.clone());
            }
            "<" => {
                range_spec.insert("lt".to_string(), value.clone());
            }
            "<=" => {
                range_spec.insert("lte".to_string(), value.clone());
            }
            _ => return Err(Error::InvalidInput(format!("Invalid range operator: {}", operator))),
        }
        
        let field_entry = self.schema.get_field_entry(field);
        match field_entry.field_type() {
            FieldType::I64(_) => self.create_i64_range_query(field, field_name, &range_spec),
            FieldType::U64(_) => self.create_u64_range_query(field, field_name, &range_spec),
            FieldType::F64(_) => self.create_f64_range_query(field, field_name, &range_spec),
            FieldType::Date(_) => self.create_date_range_query(field, field_name, &range_spec),
            _ => Err(Error::InvalidInput("Range queries only supported for numeric fields".to_string())),
        }
    }
    
    /// Create IN query
    fn create_in_query(&self, field: Field, values: Vec<Value>) -> Result<Box<dyn TantivyQuery>> {
        let mut subqueries = Vec::new();
        
        for value in values {
            let term_query = self.create_term_query_from_value(field, &value)?;
            subqueries.push((Occur::Should, term_query));
        }
        
        Ok(Box::new(BooleanQuery::new(subqueries)))
    }
    
    /// Create BETWEEN query
    fn create_between_query(
        &self,
        field: Field,
        field_name: &str,
        lower: Value,
        upper: Value,
    ) -> Result<Box<dyn TantivyQuery>> {
        let mut range_spec = serde_json::Map::new();
        range_spec.insert("gte".to_string(), lower);
        range_spec.insert("lte".to_string(), upper);
        
        let field_entry = self.schema.get_field_entry(field);
        match field_entry.field_type() {
            FieldType::I64(_) => self.create_i64_range_query(field, field_name, &range_spec),
            FieldType::U64(_) => self.create_u64_range_query(field, field_name, &range_spec),
            FieldType::F64(_) => self.create_f64_range_query(field, field_name, &range_spec),
            FieldType::Date(_) => self.create_date_range_query(field, field_name, &range_spec),
            _ => Err(Error::InvalidInput("BETWEEN only supported for numeric fields".to_string())),
        }
    }
    
    /// Convert SQL LIKE pattern to regex
    ///
    /// Note: tantivy 0.24 rejects `.*` as "empty match operators", so we use
    /// `[\\s\\S]*` for `%` wildcards (matches 0+ of any char).
    fn sql_like_to_regex(&self, pattern: &str) -> String {
        let mut regex = String::new();

        for ch in pattern.chars() {
            match ch {
                '%' => regex.push_str("[\\s\\S]*"),
                '_' => regex.push_str("[\\s\\S]"),
                '.' | '+' | '*' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                    regex.push('\\');
                    regex.push(ch);
                }
                _ => regex.push(ch),
            }
        }

        regex
    }
    
    /// Translate simple text query
    pub fn translate_simple_query(&self, text: &str) -> Result<Box<dyn TantivyQuery>> {
        // For simple queries, search across all text fields
        let mut subqueries = Vec::new();
        let mut has_text_fields = false;
        
        for (field, field_entry) in self.schema.fields() {
            if matches!(field_entry.field_type(), FieldType::Str(_)) {
                has_text_fields = true;
                let terms: Vec<&str> = text.split_whitespace().collect();
                
                if terms.len() == 1 {
                    let term_query = self.create_term_query(field, terms[0])?;
                    subqueries.push((Occur::Should, term_query));
                } else {
                    // Create a sub-bool query for this field
                    let field_query = self.create_match_query_with_operator(field, text, "or")?;
                    subqueries.push((Occur::Should, field_query));
                }
            }
        }
        
        if !has_text_fields {
            return Err(Error::InvalidInput("No text fields found in schema".to_string()));
        }
        
        Ok(Box::new(BooleanQuery::new(subqueries)))
    }
    
    /// Translate Kafka-specific query format
    pub fn translate_kafka_query(&self, kafka_query: &KafkaQuery) -> Result<Box<dyn TantivyQuery>> {
        let mut subqueries = Vec::new();
        
        // Add topic filter
        if let Some(topic) = &kafka_query.topic {
            if let Ok(topic_field) = self.get_field("topic") {
                let topic_query = self.create_term_query(topic_field, topic)?;
                subqueries.push((Occur::Must, topic_query));
            }
        }
        
        // Add partition filter
        if let Some(partition) = kafka_query.partition {
            if let Ok(partition_field) = self.get_field("partition") {
                let partition_query = Box::new(TermQuery::new(
                    Term::from_field_i64(partition_field, partition as i64),
                    Default::default(),
                ));
                subqueries.push((Occur::Must, partition_query));
            }
        }
        
        // Add offset range
        if kafka_query.start_offset.is_some() || kafka_query.end_offset.is_some() {
            if let Ok(offset_field) = self.get_field("offset") {
                let mut range_spec = serde_json::Map::new();
                
                if let Some(start) = kafka_query.start_offset {
                    range_spec.insert("gte".to_string(), Value::Number(start.into()));
                }
                
                if let Some(end) = kafka_query.end_offset {
                    range_spec.insert("lte".to_string(), Value::Number(end.into()));
                }
                
                let offset_query = self.create_i64_range_query(offset_field, "offset", &range_spec)?;
                subqueries.push((Occur::Must, offset_query));
            }
        }
        
        // Add timestamp range
        if kafka_query.start_timestamp.is_some() || kafka_query.end_timestamp.is_some() {
            if let Ok(timestamp_field) = self.get_field("timestamp") {
                let mut range_spec = serde_json::Map::new();
                
                if let Some(start) = kafka_query.start_timestamp {
                    range_spec.insert("gte".to_string(), Value::Number(start.into()));
                }
                
                if let Some(end) = kafka_query.end_timestamp {
                    range_spec.insert("lte".to_string(), Value::Number(end.into()));
                }
                
                let timestamp_query = self.create_i64_range_query(timestamp_field, "timestamp", &range_spec)?;
                subqueries.push((Occur::Must, timestamp_query));
            }
        }
        
        // Add key filter
        if let Some(key_pattern) = &kafka_query.key_pattern {
            if let Ok(key_field) = self.get_field("key") {
                let key_query = if key_pattern.contains('*') || key_pattern.contains('?') {
                    let regex_pattern = self.wildcard_to_regex(key_pattern);
                    Box::new(RegexQuery::from_pattern(&regex_pattern, key_field)
                        .map_err(|e| Error::InvalidInput(format!("Invalid key pattern: {}", e)))?)
                } else {
                    self.create_term_query(key_field, key_pattern)?
                };
                subqueries.push((Occur::Must, key_query));
            }
        }
        
        // Add value query
        if let Some(value_query) = &kafka_query.value_query {
            match value_query {
                ValueQuery::Text(text) => {
                    if let Ok(value_field) = self.get_field("value") {
                        let value_query = self.create_match_query_with_operator(value_field, text, "or")?;
                        subqueries.push((Occur::Must, value_query));
                    }
                }
                ValueQuery::Json(json_query) => {
                    let sub_query = self.translate_json_query(json_query)?;
                    subqueries.push((Occur::Must, sub_query));
                }
                ValueQuery::Regex(pattern) => {
                    if let Ok(value_field) = self.get_field("value") {
                        let regex_query = Box::new(RegexQuery::from_pattern(pattern, value_field)
                            .map_err(|e| Error::InvalidInput(format!("Invalid regex: {}", e)))?);
                        subqueries.push((Occur::Must, regex_query));
                    }
                }
            }
        }
        
        // Add header filters
        for (header_name, header_value) in &kafka_query.headers {
            if let Ok(headers_field) = self.get_field("headers") {
                // Headers are stored as JSON, so we search for the pattern
                let header_pattern = format!(r#""{}"\s*:\s*"{}""#, header_name, header_value);
                let header_query = Box::new(RegexQuery::from_pattern(&header_pattern, headers_field)
                    .map_err(|e| Error::InvalidInput(format!("Invalid header pattern: {}", e)))?);
                subqueries.push((Occur::Must, header_query));
            }
        }
        
        Ok(Box::new(BooleanQuery::new(subqueries)))
    }
    
    // Helper methods
    
    /// Get field by name
    fn get_field(&self, name: &str) -> Result<Field> {
        self.field_cache.get(name)
            .copied()
            .ok_or_else(|| Error::InvalidInput(format!("Field '{}' not found in schema", name)))
    }
    
    /// Create term query
    fn create_term_query(&self, field: Field, text: &str) -> Result<Box<dyn TantivyQuery>> {
        let term = Term::from_field_text(field, text);
        Ok(Box::new(TermQuery::new(term, Default::default())))
    }
    
    /// Create term query from JSON value
    fn create_term_query_from_value(&self, field: Field, value: &Value) -> Result<Box<dyn TantivyQuery>> {
        match value {
            Value::String(s) => self.create_term_query(field, s),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Box::new(TermQuery::new(
                        Term::from_field_i64(field, i),
                        Default::default(),
                    )))
                } else if let Some(u) = n.as_u64() {
                    Ok(Box::new(TermQuery::new(
                        Term::from_field_u64(field, u),
                        Default::default(),
                    )))
                } else {
                    Err(Error::InvalidInput("Invalid numeric value".to_string()))
                }
            }
            Value::Bool(b) => {
                let term = if *b { "true" } else { "false" };
                self.create_term_query(field, term)
            }
            _ => Err(Error::InvalidInput("Term value must be string, number, or boolean".to_string())),
        }
    }
    
    /// Create phrase query
    fn create_phrase_query(&self, field: Field, terms: &[&str]) -> Result<Box<dyn TantivyQuery>> {
        if terms.is_empty() {
            return Ok(Box::new(AllQuery));
        }
        
        if terms.len() == 1 {
            return self.create_term_query(field, terms[0]);
        }
        
        let terms_vec: Vec<Term> = terms.iter()
            .map(|term| Term::from_field_text(field, term))
            .collect();
        
        Ok(Box::new(PhraseQuery::new(terms_vec)))
    }
    
    /// Convert wildcard pattern to regex
    ///
    /// Note: tantivy 0.24 rejects `.*` as "empty match operators", so we use
    /// `[\\s\\S]+` for `*` wildcards (matches 1+ of any char including newlines).
    fn wildcard_to_regex(&self, pattern: &str) -> String {
        let mut regex = String::new();

        for ch in pattern.chars() {
            match ch {
                '*' => regex.push_str("[\\s\\S]+"),
                '?' => regex.push_str("[\\s\\S]"),
                '.' | '+' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                    regex.push('\\');
                    regex.push(ch);
                }
                _ => regex.push(ch),
            }
        }

        regex
    }
}

/// Query input formats
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueryInput {
    /// JSON query DSL (Elasticsearch-compatible)
    Json(Value),
    /// SQL-like query
    Sql(String),
    /// Simple text search
    Simple(String),
    /// Kafka-specific query format
    Kafka(KafkaQuery),
}

/// Kafka-specific query format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KafkaQuery {
    /// Topic filter
    pub topic: Option<String>,
    /// Partition filter
    pub partition: Option<i32>,
    /// Start offset (inclusive)
    pub start_offset: Option<i64>,
    /// End offset (inclusive)
    pub end_offset: Option<i64>,
    /// Start timestamp (epoch millis)
    pub start_timestamp: Option<i64>,
    /// End timestamp (epoch millis)
    pub end_timestamp: Option<i64>,
    /// Key pattern (supports wildcards)
    pub key_pattern: Option<String>,
    /// Value query
    pub value_query: Option<ValueQuery>,
    /// Header filters
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Value query types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValueQuery {
    /// Text search
    #[serde(rename = "text")]
    Text(String),
    /// JSON query for structured data
    #[serde(rename = "json")]
    Json(Value),
    /// Regular expression
    #[serde(rename = "regex")]
    Regex(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::{Schema, TEXT, STORED, STRING};
    
    fn create_test_schema() -> Schema {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("topic", STRING | STORED);
        schema_builder.add_i64_field("partition", STORED);
        schema_builder.add_i64_field("offset", STORED);
        schema_builder.add_i64_field("timestamp", STORED);
        schema_builder.add_text_field("key", TEXT | STORED);
        schema_builder.add_text_field("value", TEXT | STORED);
        schema_builder.add_text_field("headers", TEXT | STORED);
        schema_builder.build()
    }
    
    #[test]
    fn test_match_query() {
        let schema = create_test_schema();
        let translator = QueryTranslator::new(schema);
        
        let query = serde_json::json!({
            "match": {
                "value": "hello world"
            }
        });
        
        let result = translator.translate_json_query(&query);
        assert!(result.is_ok());
    }
    
    #[test]
    fn test_term_query() {
        let schema = create_test_schema();
        let translator = QueryTranslator::new(schema);
        
        let query = serde_json::json!({
            "term": {
                "topic": "events"
            }
        });
        
        let result = translator.translate_json_query(&query);
        assert!(result.is_ok());
    }
    
    #[test]
    fn test_range_query() {
        let schema = create_test_schema();
        let translator = QueryTranslator::new(schema);
        
        let query = serde_json::json!({
            "range": {
                "timestamp": {
                    "gte": 1000,
                    "lte": 2000
                }
            }
        });
        
        let result = translator.translate_json_query(&query);
        assert!(result.is_ok());
    }
    
    #[test]
    fn test_bool_query() {
        let schema = create_test_schema();
        let translator = QueryTranslator::new(schema);
        
        let query = serde_json::json!({
            "bool": {
                "must": [
                    {"term": {"topic": "events"}},
                    {"range": {"timestamp": {"gte": 1000}}}
                ],
                "should": [
                    {"match": {"value": "error"}}
                ],
                "must_not": [
                    {"term": {"key": "test"}}
                ]
            }
        });
        
        let result = translator.translate_json_query(&query);
        assert!(result.is_ok());
    }
    
    #[test]
    fn test_sql_query() {
        let schema = create_test_schema();
        let translator = QueryTranslator::new(schema);
        
        let sql = "SELECT * FROM messages WHERE topic = 'events' AND timestamp > 1000";
        let query = QueryInput::Sql(sql.to_string());
        
        let result = translator.translate(&query);
        assert!(result.is_ok());
    }
    
    #[test]
    fn test_kafka_query() {
        let schema = create_test_schema();
        let translator = QueryTranslator::new(schema);
        
        let kafka_query = KafkaQuery {
            topic: Some("events".to_string()),
            partition: Some(0),
            start_offset: Some(100),
            end_offset: Some(200),
            start_timestamp: None,
            end_timestamp: None,
            key_pattern: Some("user:*".to_string()),
            value_query: Some(ValueQuery::Text("error".to_string())),
            headers: HashMap::new(),
        };
        
        let query = QueryInput::Kafka(kafka_query);
        let result = translator.translate(&query);
        assert!(result.is_ok(), "Kafka query translation failed: {:?}", result.err());
    }
}