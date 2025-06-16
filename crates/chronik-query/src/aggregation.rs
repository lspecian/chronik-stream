//! Real-time aggregation engine.

use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::interval;

/// Aggregation window type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowType {
    /// Fixed time window (e.g., 1 minute, 5 minutes)
    Fixed(Duration),
    /// Sliding window with size and step
    Sliding { size: Duration, step: Duration },
    /// Session window with timeout
    Session(Duration),
}

/// Aggregation function
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AggregationFunction {
    Count,
    Sum,
    Average,
    Min,
    Max,
    First,
    Last,
}

/// Aggregation query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregationQuery {
    /// Topic to aggregate
    pub topic: String,
    /// Partition (None means all partitions)
    pub partition: Option<i32>,
    /// Group by fields
    pub group_by: Vec<String>,
    /// Window type
    pub window: WindowType,
    /// Aggregation function
    pub function: AggregationFunction,
    /// Field to aggregate (for Sum, Average, Min, Max)
    pub field: Option<String>,
    /// Filter expression (simple key=value for now)
    pub filter: Option<HashMap<String, String>>,
}

/// Aggregation result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregationResult {
    /// Window start time (epoch millis)
    pub window_start: i64,
    /// Window end time (epoch millis)
    pub window_end: i64,
    /// Group key (if grouping)
    pub group_key: Option<String>,
    /// Aggregated value
    pub value: f64,
    /// Number of records in the window
    pub count: usize,
}

/// Message for aggregation
#[derive(Debug, Clone)]
struct AggregationMessage {
    timestamp: i64,
    key: Option<String>,
    value: String,
    parsed_value: Option<f64>,
}

/// Window state
#[derive(Debug)]
struct WindowState {
    start_time: i64,
    end_time: i64,
    messages: Vec<AggregationMessage>,
    groups: HashMap<String, Vec<AggregationMessage>>,
}

impl WindowState {
    fn new(start_time: i64, end_time: i64) -> Self {
        Self {
            start_time,
            end_time,
            messages: Vec::new(),
            groups: HashMap::new(),
        }
    }
    
    fn add_message(&mut self, message: AggregationMessage, group_key: Option<String>) {
        if let Some(key) = group_key {
            self.groups.entry(key).or_insert_with(Vec::new).push(message);
        } else {
            self.messages.push(message);
        }
    }
    
    fn compute_result(&self, function: AggregationFunction, field: &Option<String>) -> Vec<AggregationResult> {
        let mut results = Vec::new();
        
        if self.groups.is_empty() {
            // No grouping
            if let Some(value) = compute_aggregation(&self.messages, function, field) {
                results.push(AggregationResult {
                    window_start: self.start_time,
                    window_end: self.end_time,
                    group_key: None,
                    value,
                    count: self.messages.len(),
                });
            }
        } else {
            // With grouping
            for (group_key, messages) in &self.groups {
                if let Some(value) = compute_aggregation(messages, function, field) {
                    results.push(AggregationResult {
                        window_start: self.start_time,
                        window_end: self.end_time,
                        group_key: Some(group_key.clone()),
                        value,
                        count: messages.len(),
                    });
                }
            }
        }
        
        results
    }
}

/// Compute aggregation for a set of messages
fn compute_aggregation(
    messages: &[AggregationMessage],
    function: AggregationFunction,
    field: &Option<String>,
) -> Option<f64> {
    if messages.is_empty() {
        return None;
    }
    
    match function {
        AggregationFunction::Count => Some(messages.len() as f64),
        AggregationFunction::Sum => {
            if field.is_some() {
                Some(messages.iter().filter_map(|m| m.parsed_value).sum())
            } else {
                None
            }
        }
        AggregationFunction::Average => {
            if field.is_some() {
                let values: Vec<f64> = messages.iter().filter_map(|m| m.parsed_value).collect();
                if values.is_empty() {
                    None
                } else {
                    Some(values.iter().sum::<f64>() / values.len() as f64)
                }
            } else {
                None
            }
        }
        AggregationFunction::Min => {
            if field.is_some() {
                messages.iter().filter_map(|m| m.parsed_value).min_by(|a, b| a.partial_cmp(b).unwrap())
            } else {
                None
            }
        }
        AggregationFunction::Max => {
            if field.is_some() {
                messages.iter().filter_map(|m| m.parsed_value).max_by(|a, b| a.partial_cmp(b).unwrap())
            } else {
                None
            }
        }
        AggregationFunction::First => {
            messages.first().and_then(|m| m.parsed_value.or(Some(1.0)))
        }
        AggregationFunction::Last => {
            messages.last().and_then(|m| m.parsed_value.or(Some(1.0)))
        }
    }
}

/// Aggregation engine
pub struct AggregationEngine {
    /// Active aggregations
    aggregations: Arc<RwLock<HashMap<String, ActiveAggregation>>>,
}

/// Active aggregation state
struct ActiveAggregation {
    query: AggregationQuery,
    windows: VecDeque<WindowState>,
    last_window_time: i64,
}

impl AggregationEngine {
    /// Create a new aggregation engine
    pub fn new() -> Self {
        Self {
            aggregations: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Start the aggregation engine
    pub async fn start(self: Arc<Self>) -> Result<()> {
        // Start window processing task
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(e) = engine.run_window_processing().await {
                tracing::error!("Window processing failed: {}", e);
            }
        });
        
        Ok(())
    }
    
    /// Register an aggregation query
    pub async fn register_aggregation(&self, id: String, query: AggregationQuery) -> Result<()> {
        let mut aggregations = self.aggregations.write().await;
        
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        let window_duration = match query.window {
            WindowType::Fixed(d) => d,
            WindowType::Sliding { size, .. } => size,
            WindowType::Session(timeout) => timeout,
        };
        
        let window_millis = window_duration.as_millis() as i64;
        let window_start = (now / window_millis) * window_millis;
        
        let active = ActiveAggregation {
            query,
            windows: VecDeque::new(),
            last_window_time: window_start,
        };
        
        aggregations.insert(id, active);
        Ok(())
    }
    
    /// Remove an aggregation
    pub async fn remove_aggregation(&self, id: &str) -> Result<()> {
        let mut aggregations = self.aggregations.write().await;
        aggregations.remove(id);
        Ok(())
    }
    
    /// Process a message for aggregations
    pub async fn process_message(
        &self,
        topic: &str,
        partition: i32,
        timestamp: i64,
        key: Option<&[u8]>,
        value: &[u8],
    ) -> Result<()> {
        let key_str = key.and_then(|k| std::str::from_utf8(k).ok()).map(|s| s.to_string());
        let value_str = std::str::from_utf8(value).unwrap_or("").to_string();
        
        let mut aggregations = self.aggregations.write().await;
        
        for (_, active) in aggregations.iter_mut() {
            // Check if message matches aggregation criteria
            if active.query.topic != topic {
                continue;
            }
            
            if let Some(p) = active.query.partition {
                if p != partition {
                    continue;
                }
            }
            
            // Parse value if needed
            let parsed_value = if active.query.field.is_some() {
                // Try to parse as JSON and extract field
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&value_str) {
                    active.query.field.as_ref()
                        .and_then(|f| json.get(f))
                        .and_then(|v| v.as_f64())
                } else {
                    // Try direct parsing
                    value_str.parse::<f64>().ok()
                }
            } else {
                None
            };
            
            let message = AggregationMessage {
                timestamp,
                key: key_str.clone(),
                value: value_str.clone(),
                parsed_value,
            };
            
            // Determine window
            let window_duration = match active.query.window {
                WindowType::Fixed(d) => d.as_millis() as i64,
                WindowType::Sliding { size, .. } => size.as_millis() as i64,
                WindowType::Session(_) => {
                    // Session windows not fully implemented
                    continue;
                }
            };
            
            let window_start = (timestamp / window_duration) * window_duration;
            let window_end = window_start + window_duration;
            
            // Find or create window
            let window_exists = active.windows.iter().any(|w| w.start_time == window_start);
            
            if !window_exists {
                let new_window = WindowState::new(window_start, window_end);
                active.windows.push_back(new_window);
            }
            
            if let Some(window) = active.windows.iter_mut().find(|w| w.start_time == window_start) {
                // Determine group key
                let group_key = if !active.query.group_by.is_empty() {
                    // Simple grouping by concatenating field values
                    Some(active.query.group_by.iter()
                        .map(|field| {
                            if field == "key" {
                                key_str.as_ref().unwrap_or(&"null".to_string()).clone()
                            } else {
                                // Try to extract from JSON
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&value_str) {
                                    json.get(field)
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("null")
                                        .to_string()
                                } else {
                                    "null".to_string()
                                }
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(":"))
                } else {
                    None
                };
                
                window.add_message(message, group_key);
            }
        }
        
        Ok(())
    }
    
    /// Get aggregation results
    pub async fn get_results(&self, id: &str) -> Result<Vec<AggregationResult>> {
        let aggregations = self.aggregations.read().await;
        let active = aggregations.get(id)
            .ok_or_else(|| Error::InvalidInput(format!("Unknown aggregation: {}", id)))?;
        
        let mut results = Vec::new();
        for window in &active.windows {
            results.extend(window.compute_result(active.query.function, &active.query.field));
        }
        
        Ok(results)
    }
    
    /// Run window processing loop
    async fn run_window_processing(&self) -> Result<()> {
        let mut interval = interval(Duration::from_secs(1));
        
        loop {
            interval.tick().await;
            
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
            let mut aggregations = self.aggregations.write().await;
            
            for (_, active) in aggregations.iter_mut() {
                // Clean old windows based on window type
                match active.query.window {
                    WindowType::Fixed(duration) => {
                        let retention = duration.as_millis() as i64 * 10; // Keep 10 windows
                        active.windows.retain(|w| now - w.end_time < retention);
                    }
                    WindowType::Sliding { size, step: _ } => {
                        let retention = size.as_millis() as i64 * 10;
                        active.windows.retain(|w| now - w.end_time < retention);
                    }
                    WindowType::Session(_) => {
                        // Session window cleanup not implemented
                    }
                }
            }
        }
    }
}