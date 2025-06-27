use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, BTreeMap};
use tantivy::{
    collector::{Count, TopDocs, DocSetCollector},
    query::{Query, EnableScoring},
    schema::{Field, FieldType},
    IndexReader, Searcher, DocAddress,
};
use tantivy::schema::Schema;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AggregationRequest {
    Terms(TermsAggregation),
    Stats(StatsAggregation),
    ValueCount(ValueCountAggregation),
    DateHistogram(DateHistogramAggregation),
    Range(RangeAggregation),
    Avg(AvgAggregation),
    Sum(SumAggregation),
    Min(MinAggregation),
    Max(MaxAggregation),
    Cardinality(CardinalityAggregation),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermsAggregation {
    pub field: String,
    #[serde(default = "default_size")]
    pub size: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_doc_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsAggregation {
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueCountAggregation {
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateHistogramAggregation {
    pub field: String,
    pub interval: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeAggregation {
    pub field: String,
    pub ranges: Vec<RangeSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvgAggregation {
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SumAggregation {
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinAggregation {
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaxAggregation {
    pub field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardinalityAggregation {
    pub field: String,
}

fn default_size() -> u32 {
    10
}

pub struct AggregationExecutor {
    reader: IndexReader,
    schema: Schema,
}

impl AggregationExecutor {
    pub fn new(reader: IndexReader, schema: Schema) -> Self {
        Self { reader, schema }
    }

    pub fn execute(
        &self,
        query: Box<dyn Query>,
        aggregations: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let searcher = self.reader.searcher();
        let aggs = self.parse_aggregations(aggregations)?;
        let mut results = serde_json::Map::new();
        
        for (name, agg_req) in aggs {
            let result = self.execute_single_aggregation(&searcher, &query, &agg_req)?;
            results.insert(name, result);
        }
        
        Ok(serde_json::Value::Object(results))
    }
    
    fn parse_aggregations(&self, aggs: serde_json::Value) -> Result<HashMap<String, AggregationRequest>> {
        let mut result = HashMap::new();
        
        if let serde_json::Value::Object(map) = aggs {
            for (name, value) in map {
                // Parse each aggregation type
                if let Some(terms) = value.get("terms") {
                    let agg: TermsAggregation = serde_json::from_value(terms.clone())?;
                    result.insert(name, AggregationRequest::Terms(agg));
                } else if let Some(stats) = value.get("stats") {
                    let agg: StatsAggregation = serde_json::from_value(stats.clone())?;
                    result.insert(name, AggregationRequest::Stats(agg));
                } else if let Some(value_count) = value.get("value_count") {
                    let agg: ValueCountAggregation = serde_json::from_value(value_count.clone())?;
                    result.insert(name, AggregationRequest::ValueCount(agg));
                } else if let Some(date_histogram) = value.get("date_histogram") {
                    let agg: DateHistogramAggregation = serde_json::from_value(date_histogram.clone())?;
                    result.insert(name, AggregationRequest::DateHistogram(agg));
                } else if let Some(range) = value.get("range") {
                    let agg: RangeAggregation = serde_json::from_value(range.clone())?;
                    result.insert(name, AggregationRequest::Range(agg));
                } else if let Some(avg) = value.get("avg") {
                    let agg: AvgAggregation = serde_json::from_value(avg.clone())?;
                    result.insert(name, AggregationRequest::Avg(agg));
                } else if let Some(sum) = value.get("sum") {
                    let agg: SumAggregation = serde_json::from_value(sum.clone())?;
                    result.insert(name, AggregationRequest::Sum(agg));
                } else if let Some(min) = value.get("min") {
                    let agg: MinAggregation = serde_json::from_value(min.clone())?;
                    result.insert(name, AggregationRequest::Min(agg));
                } else if let Some(max) = value.get("max") {
                    let agg: MaxAggregation = serde_json::from_value(max.clone())?;
                    result.insert(name, AggregationRequest::Max(agg));
                } else if let Some(cardinality) = value.get("cardinality") {
                    let agg: CardinalityAggregation = serde_json::from_value(cardinality.clone())?;
                    result.insert(name, AggregationRequest::Cardinality(agg));
                }
            }
        }
        
        Ok(result)
    }
    
    fn execute_single_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        agg_req: &AggregationRequest,
    ) -> Result<serde_json::Value> {
        match agg_req {
            AggregationRequest::Terms(terms) => self.execute_terms_aggregation(searcher, query, terms),
            AggregationRequest::Stats(stats) => self.execute_stats_aggregation(searcher, query, stats),
            AggregationRequest::ValueCount(count) => self.execute_value_count(searcher, query, count),
            AggregationRequest::DateHistogram(hist) => self.execute_date_histogram(searcher, query, hist),
            AggregationRequest::Range(range) => self.execute_range_aggregation(searcher, query, range),
            AggregationRequest::Avg(avg) => self.execute_avg_aggregation(searcher, query, avg),
            AggregationRequest::Sum(sum) => self.execute_sum_aggregation(searcher, query, sum),
            AggregationRequest::Min(min) => self.execute_min_aggregation(searcher, query, min),
            AggregationRequest::Max(max) => self.execute_max_aggregation(searcher, query, max),
            AggregationRequest::Cardinality(card) => self.execute_cardinality_aggregation(searcher, query, card),
        }
    }
    
    fn execute_terms_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        terms_agg: &TermsAggregation,
    ) -> Result<serde_json::Value> {
        let field = self.schema.get_field(&terms_agg.field)
            .map_err(|_| anyhow::anyhow!("Field not found: {}", terms_agg.field))?;
        
        // Collect all matching documents
        let weight = query.weight(EnableScoring::disabled_from_searcher(searcher))?;
        let mut term_counts: HashMap<String, u64> = HashMap::new();
        
        for (segment_ord, segment_reader) in searcher.segment_readers().iter().enumerate() {
            let mut scorer = weight.scorer(segment_reader, 1.0)?;
            
            // Try to get fast field reader for better performance
            let field_name = self.schema.get_field_name(field);
            if let Ok(u64_col) = segment_reader.fast_fields().u64(field_name) {
                let mut doc = scorer.doc();
                while doc != tantivy::TERMINATED {
                    let value = u64_col.values.get_val(doc);
                    let key = value.to_string();
                    *term_counts.entry(key).or_insert(0) += 1;
                    doc = scorer.advance();
                }
            } else if let Ok(str_col) = segment_reader.fast_fields().str(field_name) {
                if let Some(column) = str_col {
                    let mut doc = scorer.doc();
                    while doc != tantivy::TERMINATED {
                        let mut values = vec![];
                        column.term_ords(doc).for_each(|ord| {
                            let mut bytes = vec![];
                            if column.ord_to_bytes(ord, &mut bytes).is_ok() {
                                if let Ok(s) = std::str::from_utf8(&bytes) {
                                    values.push(s.to_string());
                                }
                            }
                        });
                        
                        for value in values {
                            *term_counts.entry(value).or_insert(0) += 1;
                        }
                        doc = scorer.advance();
                    }
                }
            } else {
                // Fallback to stored fields
                let mut doc = scorer.doc();
                while doc != tantivy::TERMINATED {
                    let stored_doc: tantivy::TantivyDocument = searcher.doc(DocAddress::new(segment_ord as u32, doc))?;
                    
                    if let Some(field_value) = stored_doc.get_first(field) {
                        // In tantivy 0.24, values are wrapped in CompactDocValue
                        // We need to convert to string representation
                        let key = format!("{:?}", field_value);
                        *term_counts.entry(key).or_insert(0) += 1;
                    }
                    doc = scorer.advance();
                }
            }
        }
        
        // Sort by count and take top N
        let mut sorted_terms: Vec<_> = term_counts.into_iter().collect();
        sorted_terms.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        
        let min_doc_count = terms_agg.min_doc_count.unwrap_or(1);
        let buckets: Vec<serde_json::Value> = sorted_terms
            .into_iter()
            .filter(|(_, count)| *count >= min_doc_count as u64)
            .take(terms_agg.size as usize)
            .map(|(key, count)| {
                serde_json::json!({
                    "key": key,
                    "doc_count": count
                })
            })
            .collect();
        
        Ok(serde_json::json!({
            "doc_count_error_upper_bound": 0,
            "sum_other_doc_count": 0,
            "buckets": buckets
        }))
    }
    
    fn execute_stats_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        stats_agg: &StatsAggregation,
    ) -> Result<serde_json::Value> {
        let field = self.schema.get_field(&stats_agg.field)
            .map_err(|_| anyhow::anyhow!("Field not found: {}", stats_agg.field))?;
        
        let mut count = 0u64;
        let mut sum = 0.0f64;
        let mut min = f64::MAX;
        let mut max = f64::MIN;
        
        let weight = query.weight(EnableScoring::disabled_from_searcher(searcher))?;
        
        for segment_reader in searcher.segment_readers() {
            let mut scorer = weight.scorer(segment_reader, 1.0)?;
            
            let field_name = self.schema.get_field_name(field);
            if let Ok(f64_col) = segment_reader.fast_fields().f64(field_name) {
                let mut doc = scorer.doc();
                while doc != tantivy::TERMINATED {
                    let value = f64_col.values.get_val(doc);
                    count += 1;
                    sum += value;
                    min = min.min(value);
                    max = max.max(value);
                    doc = scorer.advance();
                }
            } else if let Ok(u64_col) = segment_reader.fast_fields().u64(field_name) {
                let mut doc = scorer.doc();
                while doc != tantivy::TERMINATED {
                    let value = u64_col.values.get_val(doc);
                    let fvalue = value as f64;
                    count += 1;
                    sum += fvalue;
                    min = min.min(fvalue);
                    max = max.max(fvalue);
                    doc = scorer.advance();
                }
            }
        }
        
        let avg = if count > 0 { sum / count as f64 } else { 0.0 };
        
        Ok(serde_json::json!({
            "count": count,
            "min": if count > 0 { min } else { 0.0 },
            "max": if count > 0 { max } else { 0.0 },
            "avg": avg,
            "sum": sum
        }))
    }
    
    fn execute_value_count(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        count_agg: &ValueCountAggregation,
    ) -> Result<serde_json::Value> {
        let field = self.schema.get_field(&count_agg.field)
            .map_err(|_| anyhow::anyhow!("Field not found: {}", count_agg.field))?;
        
        let mut count = 0u64;
        let weight = query.weight(EnableScoring::disabled_from_searcher(searcher))?;
        
        for segment_reader in searcher.segment_readers() {
            let mut scorer = weight.scorer(segment_reader, 1.0)?;
            let mut doc = scorer.doc();
            
            while doc != tantivy::TERMINATED {
                // For value_count, we just need to know if the field exists
                count += 1;
                doc = scorer.advance();
            }
        }
        
        Ok(serde_json::json!({
            "value": count
        }))
    }
    
    fn execute_date_histogram(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        hist_agg: &DateHistogramAggregation,
    ) -> Result<serde_json::Value> {
        let field = self.schema.get_field(&hist_agg.field)
            .map_err(|_| anyhow::anyhow!("Field not found: {}", hist_agg.field))?;
        
        // Parse interval (simplified - only supports basic intervals)
        let interval_ms = match hist_agg.interval.as_str() {
            "1m" | "minute" => 60_000i64,
            "1h" | "hour" => 3_600_000i64,
            "1d" | "day" => 86_400_000i64,
            "1w" | "week" => 604_800_000i64,
            _ => return Err(anyhow::anyhow!("Unsupported interval: {}", hist_agg.interval)),
        };
        
        let mut buckets: BTreeMap<i64, u64> = BTreeMap::new();
        let weight = query.weight(EnableScoring::disabled_from_searcher(searcher))?;
        
        for segment_reader in searcher.segment_readers() {
            let mut scorer = weight.scorer(segment_reader, 1.0)?;
            
            let field_name = self.schema.get_field_name(field);
            if let Ok(date_col) = segment_reader.fast_fields().date(field_name) {
                let mut doc = scorer.doc();
                while doc != tantivy::TERMINATED {
                    let date_value = date_col.values.get_val(doc);
                    let timestamp = date_value.into_timestamp_millis();
                    let bucket_key = (timestamp / interval_ms) * interval_ms;
                    *buckets.entry(bucket_key).or_insert(0) += 1;
                    doc = scorer.advance();
                }
            }
        }
        
        let bucket_list: Vec<serde_json::Value> = buckets
            .into_iter()
            .map(|(key, count)| {
                serde_json::json!({
                    "key": key,
                    "key_as_string": format_timestamp(key),
                    "doc_count": count
                })
            })
            .collect();
        
        Ok(serde_json::json!({
            "buckets": bucket_list
        }))
    }
    
    fn execute_range_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        range_agg: &RangeAggregation,
    ) -> Result<serde_json::Value> {
        let field = self.schema.get_field(&range_agg.field)
            .map_err(|_| anyhow::anyhow!("Field not found: {}", range_agg.field))?;
        
        let mut range_counts: Vec<(Option<f64>, Option<f64>, Option<String>, u64)> = 
            range_agg.ranges.iter().map(|r| (r.from, r.to, r.key.clone(), 0)).collect();
        
        let weight = query.weight(EnableScoring::disabled_from_searcher(searcher))?;
        
        for segment_reader in searcher.segment_readers() {
            let mut scorer = weight.scorer(segment_reader, 1.0)?;
            
            let field_name = self.schema.get_field_name(field);
            if let Ok(f64_col) = segment_reader.fast_fields().f64(field_name) {
                let mut doc = scorer.doc();
                while doc != tantivy::TERMINATED {
                    let value = f64_col.values.get_val(doc);
                    for (from, to, _, count) in &mut range_counts {
                        let in_range = match (from, to) {
                            (Some(f), Some(t)) => value >= *f && value < *t,
                            (Some(f), None) => value >= *f,
                            (None, Some(t)) => value < *t,
                            (None, None) => true,
                        };
                        if in_range {
                            *count += 1;
                        }
                    }
                    doc = scorer.advance();
                }
            }
        }
        
        let buckets: Vec<serde_json::Value> = range_counts
            .into_iter()
            .map(|(from, to, key, count)| {
                let mut bucket = serde_json::json!({
                    "doc_count": count
                });
                
                if let Some(k) = key {
                    bucket["key"] = serde_json::Value::String(k);
                }
                if let Some(f) = from {
                    bucket["from"] = serde_json::Value::Number(serde_json::Number::from_f64(f).unwrap());
                }
                if let Some(t) = to {
                    bucket["to"] = serde_json::Value::Number(serde_json::Number::from_f64(t).unwrap());
                }
                
                bucket
            })
            .collect();
        
        Ok(serde_json::json!({
            "buckets": buckets
        }))
    }
    
    fn execute_avg_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        avg_agg: &AvgAggregation,
    ) -> Result<serde_json::Value> {
        let stats = self.execute_stats_aggregation(searcher, query, &StatsAggregation {
            field: avg_agg.field.clone(),
        })?;
        
        Ok(serde_json::json!({
            "value": stats["avg"]
        }))
    }
    
    fn execute_sum_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        sum_agg: &SumAggregation,
    ) -> Result<serde_json::Value> {
        let stats = self.execute_stats_aggregation(searcher, query, &StatsAggregation {
            field: sum_agg.field.clone(),
        })?;
        
        Ok(serde_json::json!({
            "value": stats["sum"]
        }))
    }
    
    fn execute_min_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        min_agg: &MinAggregation,
    ) -> Result<serde_json::Value> {
        let stats = self.execute_stats_aggregation(searcher, query, &StatsAggregation {
            field: min_agg.field.clone(),
        })?;
        
        Ok(serde_json::json!({
            "value": stats["min"]
        }))
    }
    
    fn execute_max_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        max_agg: &MaxAggregation,
    ) -> Result<serde_json::Value> {
        let stats = self.execute_stats_aggregation(searcher, query, &StatsAggregation {
            field: max_agg.field.clone(),
        })?;
        
        Ok(serde_json::json!({
            "value": stats["max"]
        }))
    }
    
    fn execute_cardinality_aggregation(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        card_agg: &CardinalityAggregation,
    ) -> Result<serde_json::Value> {
        let field = self.schema.get_field(&card_agg.field)
            .map_err(|_| anyhow::anyhow!("Field not found: {}", card_agg.field))?;
        
        let mut unique_values = std::collections::HashSet::new();
        let weight = query.weight(EnableScoring::disabled_from_searcher(searcher))?;
        
        for segment_reader in searcher.segment_readers() {
            let mut scorer = weight.scorer(segment_reader, 1.0)?;
            
            let field_name = self.schema.get_field_name(field);
            if let Ok(str_col) = segment_reader.fast_fields().str(field_name) {
                if let Some(column) = str_col {
                    let mut doc = scorer.doc();
                    while doc != tantivy::TERMINATED {
                        column.term_ords(doc).for_each(|ord| {
                            let mut bytes = vec![];
                            if column.ord_to_bytes(ord, &mut bytes).is_ok() {
                                unique_values.insert(bytes);
                            }
                        });
                        doc = scorer.advance();
                    }
                }
            } else if let Ok(u64_col) = segment_reader.fast_fields().u64(field_name) {
                let mut doc = scorer.doc();
                while doc != tantivy::TERMINATED {
                    let value = u64_col.values.get_val(doc);
                    unique_values.insert(value.to_ne_bytes().to_vec());
                    doc = scorer.advance();
                }
            }
        }
        
        Ok(serde_json::json!({
            "value": unique_values.len()
        }))
    }
}

fn format_timestamp(millis: i64) -> String {
    use chrono::{DateTime, Utc};
    DateTime::<Utc>::from_timestamp_millis(millis)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S.%3fZ").to_string())
        .unwrap_or_else(|| millis.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_aggregation_parsing() {
        let aggs_json = serde_json::json!({
            "status_terms": {
                "terms": {
                    "field": "status",
                    "size": 10
                }
            },
            "price_stats": {
                "stats": {
                    "field": "price"
                }
            }
        });
        
        // Create a dummy schema for testing
        let mut schema_builder = tantivy::schema::Schema::builder();
        schema_builder.add_text_field("status", tantivy::schema::STRING | tantivy::schema::STORED);
        schema_builder.add_f64_field("price", tantivy::schema::STORED | tantivy::schema::FAST);
        let schema = schema_builder.build();
        
        // Create a dummy index reader
        let index = tantivy::Index::create_in_ram(schema.clone());
        let reader = index.reader().unwrap();
        
        let executor = AggregationExecutor {
            reader,
            schema,
        };
        
        let parsed = executor.parse_aggregations(aggs_json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains_key("status_terms"));
        assert!(parsed.contains_key("price_stats"));
    }
}