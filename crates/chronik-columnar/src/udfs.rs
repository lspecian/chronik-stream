//! User-defined functions (UDFs) for SQL queries.
//!
//! Provides JSON extraction, binary decoding, and other utility functions.

use anyhow::Result;
use datafusion::arrow::array::{Array, ArrayRef, BinaryArray, StringArray};
use datafusion::arrow::datatypes::DataType;
use datafusion::logical_expr::{ColumnarValue, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature, Volatility};
use datafusion::prelude::SessionContext;
use std::any::Any;
use std::sync::Arc;

/// Register all custom UDFs with a SessionContext.
pub fn register_udfs(ctx: &SessionContext) -> Result<()> {
    ctx.register_udf(ScalarUDF::from(JsonExtractUdf::new()));
    ctx.register_udf(ScalarUDF::from(JsonExtractStringUdf::new()));
    ctx.register_udf(ScalarUDF::from(JsonExtractIntUdf::new()));
    ctx.register_udf(ScalarUDF::from(JsonExtractFloatUdf::new()));
    ctx.register_udf(ScalarUDF::from(DecodeUtf8Udf::new()));
    ctx.register_udf(ScalarUDF::from(DecodeBase64Udf::new()));
    ctx.register_udf(ScalarUDF::from(VectorDistanceUdf::new()));
    Ok(())
}

/// Generic JSON extract UDF - returns raw JSON value as string.
/// Usage: json_extract(binary_column, '$.field.path') -> string (JSON)
#[derive(Debug)]
struct JsonExtractUdf {
    signature: Signature,
}

impl JsonExtractUdf {
    fn new() -> Self {
        Self {
            signature: Signature::new(
                TypeSignature::Exact(vec![DataType::Binary, DataType::Utf8]),
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for JsonExtractUdf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "json_extract"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke(&self, args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
        let args = ColumnarValue::values_to_arrays(args)?;

        let binary_array = args[0]
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected binary array".to_string()))?;
        let path_array = args[1]
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected string array".to_string()))?;

        let mut builder = datafusion::arrow::array::StringBuilder::new();

        for i in 0..binary_array.len() {
            if binary_array.is_null(i) || path_array.is_null(i) {
                builder.append_null();
                continue;
            }

            let json_bytes = binary_array.value(i);
            let path = path_array.value(i);

            match extract_json_value(json_bytes, path) {
                Some(value) => builder.append_value(&value),
                None => builder.append_null(),
            }
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// JSON extract string UDF implementation.
#[derive(Debug)]
struct JsonExtractStringUdf {
    signature: Signature,
}

impl JsonExtractStringUdf {
    fn new() -> Self {
        Self {
            signature: Signature::new(
                TypeSignature::Exact(vec![DataType::Binary, DataType::Utf8]),
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for JsonExtractStringUdf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "json_extract_string"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke(&self, args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
        let args = ColumnarValue::values_to_arrays(args)?;

        let binary_array = args[0]
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected binary array".to_string()))?;
        let path_array = args[1]
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected string array".to_string()))?;

        let mut builder = datafusion::arrow::array::StringBuilder::new();

        for i in 0..binary_array.len() {
            if binary_array.is_null(i) || path_array.is_null(i) {
                builder.append_null();
                continue;
            }

            let json_bytes = binary_array.value(i);
            let path = path_array.value(i);

            match extract_json_string(json_bytes, path) {
                Some(value) => builder.append_value(&value),
                None => builder.append_null(),
            }
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// JSON extract int UDF implementation.
#[derive(Debug)]
struct JsonExtractIntUdf {
    signature: Signature,
}

impl JsonExtractIntUdf {
    fn new() -> Self {
        Self {
            signature: Signature::new(
                TypeSignature::Exact(vec![DataType::Binary, DataType::Utf8]),
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for JsonExtractIntUdf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "json_extract_int"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Int64)
    }

    fn invoke(&self, args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
        let args = ColumnarValue::values_to_arrays(args)?;

        let binary_array = args[0]
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected binary array".to_string()))?;
        let path_array = args[1]
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected string array".to_string()))?;

        let mut builder = datafusion::arrow::array::Int64Builder::new();

        for i in 0..binary_array.len() {
            if binary_array.is_null(i) || path_array.is_null(i) {
                builder.append_null();
                continue;
            }

            let json_bytes = binary_array.value(i);
            let path = path_array.value(i);

            match extract_json_int(json_bytes, path) {
                Some(value) => builder.append_value(value),
                None => builder.append_null(),
            }
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// JSON extract float UDF implementation.
#[derive(Debug)]
struct JsonExtractFloatUdf {
    signature: Signature,
}

impl JsonExtractFloatUdf {
    fn new() -> Self {
        Self {
            signature: Signature::new(
                TypeSignature::Exact(vec![DataType::Binary, DataType::Utf8]),
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for JsonExtractFloatUdf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "json_extract_float"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Float64)
    }

    fn invoke(&self, args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
        let args = ColumnarValue::values_to_arrays(args)?;

        let binary_array = args[0]
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected binary array".to_string()))?;
        let path_array = args[1]
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected string array".to_string()))?;

        let mut builder = datafusion::arrow::array::Float64Builder::new();

        for i in 0..binary_array.len() {
            if binary_array.is_null(i) || path_array.is_null(i) {
                builder.append_null();
                continue;
            }

            let json_bytes = binary_array.value(i);
            let path = path_array.value(i);

            match extract_json_float(json_bytes, path) {
                Some(value) => builder.append_value(value),
                None => builder.append_null(),
            }
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// Decode UTF-8 UDF implementation.
#[derive(Debug)]
struct DecodeUtf8Udf {
    signature: Signature,
}

impl DecodeUtf8Udf {
    fn new() -> Self {
        Self {
            signature: Signature::new(
                TypeSignature::Exact(vec![DataType::Binary]),
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for DecodeUtf8Udf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "decode_utf8"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke(&self, args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
        let args = ColumnarValue::values_to_arrays(args)?;

        let binary_array = args[0]
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected binary array".to_string()))?;

        let mut builder = datafusion::arrow::array::StringBuilder::new();

        for i in 0..binary_array.len() {
            if binary_array.is_null(i) {
                builder.append_null();
                continue;
            }

            let bytes = binary_array.value(i);
            match std::str::from_utf8(bytes) {
                Ok(s) => builder.append_value(s),
                Err(_) => builder.append_null(),
            }
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// Decode base64 UDF implementation.
#[derive(Debug)]
struct DecodeBase64Udf {
    signature: Signature,
}

impl DecodeBase64Udf {
    fn new() -> Self {
        Self {
            signature: Signature::new(
                TypeSignature::Exact(vec![DataType::Utf8]),
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for DecodeBase64Udf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "decode_base64"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Binary)
    }

    fn invoke(&self, args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
        use base64::Engine;

        let args = ColumnarValue::values_to_arrays(args)?;

        let string_array = args[0]
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal("Expected string array".to_string()))?;

        let mut builder = datafusion::arrow::array::BinaryBuilder::new();

        for i in 0..string_array.len() {
            if string_array.is_null(i) {
                builder.append_null();
                continue;
            }

            let s = string_array.value(i);
            match base64::engine::general_purpose::STANDARD.decode(s) {
                Ok(bytes) => builder.append_value(&bytes),
                Err(_) => builder.append_null(),
            }
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

/// Vector distance UDF - calculates distance between two embedding vectors.
/// Usage: vector_distance(vec1, vec2, 'cosine') -> float
/// Supported metrics: 'cosine', 'euclidean', 'dot'
#[derive(Debug)]
struct VectorDistanceUdf {
    signature: Signature,
}

impl VectorDistanceUdf {
    fn new() -> Self {
        Self {
            signature: Signature::new(
                // Two FixedSizeList<Float32> arrays and a metric string
                TypeSignature::Any(3),
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for VectorDistanceUdf {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "vector_distance"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Float64)
    }

    fn invoke(&self, args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
        use datafusion::arrow::array::{Float32Array, Float64Builder, FixedSizeListArray};

        let args = ColumnarValue::values_to_arrays(args)?;

        // Get the first vector array
        let vec1_array = args[0]
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal(
                "Expected FixedSizeList<Float32> for first argument".to_string()
            ))?;

        // Get the second vector array
        let vec2_array = args[1]
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal(
                "Expected FixedSizeList<Float32> for second argument".to_string()
            ))?;

        // Get the metric string
        let metric_array = args[2]
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| datafusion::error::DataFusionError::Internal(
                "Expected string for metric argument".to_string()
            ))?;

        let mut builder = Float64Builder::new();

        for i in 0..vec1_array.len() {
            if vec1_array.is_null(i) || vec2_array.is_null(i) || metric_array.is_null(i) {
                builder.append_null();
                continue;
            }

            // Extract float arrays from FixedSizeList
            let vec1_values = vec1_array.value(i);
            let vec2_values = vec2_array.value(i);

            let vec1 = vec1_values
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| datafusion::error::DataFusionError::Internal(
                    "Expected Float32Array in FixedSizeList".to_string()
                ))?;

            let vec2 = vec2_values
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| datafusion::error::DataFusionError::Internal(
                    "Expected Float32Array in FixedSizeList".to_string()
                ))?;

            let metric = metric_array.value(i);

            // Convert to Vec<f32> for distance calculation
            let v1: Vec<f32> = (0..vec1.len()).map(|j| vec1.value(j)).collect();
            let v2: Vec<f32> = (0..vec2.len()).map(|j| vec2.value(j)).collect();

            let distance = match metric {
                "cosine" => cosine_distance(&v1, &v2),
                "euclidean" => euclidean_distance(&v1, &v2),
                "dot" => dot_product(&v1, &v2),
                _ => {
                    builder.append_null();
                    continue;
                }
            };

            builder.append_value(distance);
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish()) as ArrayRef))
    }
}

// Helper functions for vector distance calculations

/// Calculate cosine distance between two vectors (1 - cosine_similarity).
fn cosine_distance(v1: &[f32], v2: &[f32]) -> f64 {
    if v1.len() != v2.len() || v1.is_empty() {
        return f64::NAN;
    }

    let dot: f64 = v1.iter().zip(v2.iter()).map(|(a, b)| (*a as f64) * (*b as f64)).sum();
    let norm1: f64 = v1.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let norm2: f64 = v2.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();

    if norm1 == 0.0 || norm2 == 0.0 {
        return f64::NAN;
    }

    1.0 - (dot / (norm1 * norm2))
}

/// Calculate Euclidean distance between two vectors.
fn euclidean_distance(v1: &[f32], v2: &[f32]) -> f64 {
    if v1.len() != v2.len() {
        return f64::NAN;
    }

    v1.iter()
        .zip(v2.iter())
        .map(|(a, b)| ((*a as f64) - (*b as f64)).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// Calculate dot product between two vectors.
fn dot_product(v1: &[f32], v2: &[f32]) -> f64 {
    if v1.len() != v2.len() {
        return f64::NAN;
    }

    v1.iter()
        .zip(v2.iter())
        .map(|(a, b)| (*a as f64) * (*b as f64))
        .sum()
}

// Helper functions for JSON extraction

/// Extract any JSON value and return it as a JSON string.
fn extract_json_value(json_bytes: &[u8], path: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(json_bytes).ok()?;
    let value = json_path_get(&json, path)?;
    // Return the raw JSON representation
    Some(value.to_string())
}

fn extract_json_string(json_bytes: &[u8], path: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(json_bytes).ok()?;
    let value = json_path_get(&json, path)?;
    value.as_str().map(|s| s.to_string())
}

fn extract_json_int(json_bytes: &[u8], path: &str) -> Option<i64> {
    let json: serde_json::Value = serde_json::from_slice(json_bytes).ok()?;
    let value = json_path_get(&json, path)?;
    value.as_i64()
}

fn extract_json_float(json_bytes: &[u8], path: &str) -> Option<f64> {
    let json: serde_json::Value = serde_json::from_slice(json_bytes).ok()?;
    let value = json_path_get(&json, path)?;
    value.as_f64()
}

/// Simple JSON path extraction (supports $.field.nested syntax).
fn json_path_get<'a>(json: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let path = path.strip_prefix("$.")?;
    let parts: Vec<&str> = path.split('.').collect();

    let mut current = json;
    for part in parts {
        // Handle array index: field[0]
        if let Some(idx_start) = part.find('[') {
            let field = &part[..idx_start];
            let idx_str = &part[idx_start + 1..part.len() - 1];
            let idx: usize = idx_str.parse().ok()?;

            current = current.get(field)?;
            current = current.get(idx)?;
        } else {
            current = current.get(part)?;
        }
    }

    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_path_simple() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"name": "test", "value": 42}"#).unwrap();

        assert_eq!(
            json_path_get(&json, "$.name"),
            Some(&serde_json::Value::String("test".to_string()))
        );
        assert_eq!(
            json_path_get(&json, "$.value"),
            Some(&serde_json::json!(42))
        );
    }

    #[test]
    fn test_json_path_nested() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"user": {"name": "alice", "age": 30}}"#).unwrap();

        assert_eq!(
            json_path_get(&json, "$.user.name"),
            Some(&serde_json::Value::String("alice".to_string()))
        );
        assert_eq!(
            json_path_get(&json, "$.user.age"),
            Some(&serde_json::json!(30))
        );
    }

    #[test]
    fn test_json_path_array() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"items": [{"id": 1}, {"id": 2}]}"#).unwrap();

        assert_eq!(
            json_path_get(&json, "$.items[0].id"),
            Some(&serde_json::json!(1))
        );
        assert_eq!(
            json_path_get(&json, "$.items[1].id"),
            Some(&serde_json::json!(2))
        );
    }

    #[test]
    fn test_json_path_missing() {
        let json: serde_json::Value = serde_json::from_str(r#"{"name": "test"}"#).unwrap();

        assert_eq!(json_path_get(&json, "$.missing"), None);
        assert_eq!(json_path_get(&json, "$.name.nested"), None);
    }

    #[test]
    fn test_extract_json_string() {
        let json = r#"{"message": "hello world"}"#.as_bytes();
        assert_eq!(
            extract_json_string(json, "$.message"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn test_extract_json_int() {
        let json = r#"{"count": 42}"#.as_bytes();
        assert_eq!(extract_json_int(json, "$.count"), Some(42));
    }

    #[test]
    fn test_extract_json_float() {
        let json = r#"{"price": 19.99}"#.as_bytes();
        assert_eq!(extract_json_float(json, "$.price"), Some(19.99));
    }

    #[test]
    fn test_extract_json_value_string() {
        let json = r#"{"message": "hello world"}"#.as_bytes();
        // json_extract returns the raw JSON representation
        assert_eq!(
            extract_json_value(json, "$.message"),
            Some("\"hello world\"".to_string())
        );
    }

    #[test]
    fn test_extract_json_value_object() {
        let json = r#"{"user": {"name": "alice", "age": 30}}"#.as_bytes();
        let result = extract_json_value(json, "$.user");
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["name"], "alice");
        assert_eq!(parsed["age"], 30);
    }

    #[test]
    fn test_extract_json_value_array() {
        let json = r#"{"items": [1, 2, 3]}"#.as_bytes();
        assert_eq!(
            extract_json_value(json, "$.items"),
            Some("[1,2,3]".to_string())
        );
    }

    #[test]
    fn test_extract_json_value_number() {
        let json = r#"{"count": 42}"#.as_bytes();
        assert_eq!(
            extract_json_value(json, "$.count"),
            Some("42".to_string())
        );
    }

    #[test]
    fn test_cosine_distance_identical() {
        let v = vec![1.0, 0.0, 0.0];
        let distance = cosine_distance(&v, &v);
        assert!((distance - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_distance_orthogonal() {
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![0.0, 1.0, 0.0];
        let distance = cosine_distance(&v1, &v2);
        assert!((distance - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_distance_opposite() {
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![-1.0, 0.0, 0.0];
        let distance = cosine_distance(&v1, &v2);
        assert!((distance - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_euclidean_distance() {
        let v1 = vec![0.0, 0.0, 0.0];
        let v2 = vec![3.0, 4.0, 0.0];
        let distance = euclidean_distance(&v1, &v2);
        assert!((distance - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_dot_product() {
        let v1 = vec![1.0, 2.0, 3.0];
        let v2 = vec![4.0, 5.0, 6.0];
        let result = dot_product(&v1, &v2);
        // 1*4 + 2*5 + 3*6 = 4 + 10 + 18 = 32
        assert!((result - 32.0).abs() < 1e-10);
    }

    #[test]
    fn test_vector_distance_mismatched_lengths() {
        let v1 = vec![1.0, 2.0];
        let v2 = vec![1.0, 2.0, 3.0];
        let distance = cosine_distance(&v1, &v2);
        assert!(distance.is_nan());
    }

    #[test]
    fn test_vector_distance_empty() {
        let v1: Vec<f32> = vec![];
        let v2: Vec<f32> = vec![];
        let distance = cosine_distance(&v1, &v2);
        assert!(distance.is_nan());
    }
}
