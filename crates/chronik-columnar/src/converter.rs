//! Record conversion from Kafka records to Arrow RecordBatch.
//!
//! Converts CanonicalRecord instances to Arrow RecordBatch format for columnar storage.

use anyhow::{anyhow, Result};
use arrow_array::{
    Array, ArrayRef, BinaryArray, FixedSizeListArray, Float32Array, Int32Array, Int64Array,
    Int8Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Schema};
use std::sync::Arc;

use crate::json_schema::{InferredJsonSchema, JsonFieldType};
use crate::schema::kafka_message_schema;

/// A minimal representation of a Kafka record for conversion.
/// This mirrors the essential fields from CanonicalRecord.
#[derive(Debug, Clone)]
pub struct KafkaRecord {
    /// Topic name.
    pub topic: String,
    /// Partition number.
    pub partition: i32,
    /// Message offset.
    pub offset: i64,
    /// Timestamp in milliseconds.
    pub timestamp_ms: i64,
    /// Timestamp type (0=CreateTime, 1=LogAppendTime).
    pub timestamp_type: i8,
    /// Message key (optional).
    pub key: Option<Vec<u8>>,
    /// Message value/payload.
    pub value: Vec<u8>,
    /// Message headers as key-value pairs.
    pub headers: Vec<(String, Option<Vec<u8>>)>,
    /// Embedding vector (optional, for vector-enabled topics).
    /// The embedding is generated asynchronously by the WalIndexer.
    pub embedding: Option<Vec<f32>>,
}

/// Converter for transforming Kafka records to Arrow format.
pub struct RecordBatchConverter {
    schema: Schema,
}

impl Default for RecordBatchConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordBatchConverter {
    /// Create a new converter with the standard Kafka schema.
    pub fn new() -> Self {
        Self {
            schema: kafka_message_schema(),
        }
    }

    /// Create a converter with a custom schema.
    pub fn with_schema(schema: Schema) -> Self {
        Self { schema }
    }

    /// Get the schema used by this converter.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Convert a batch of Kafka records to an Arrow RecordBatch.
    pub fn convert(&self, records: &[KafkaRecord]) -> Result<RecordBatch> {
        if records.is_empty() {
            return Err(anyhow!("Cannot convert empty record batch"));
        }

        let num_rows = records.len();

        // Build column arrays
        let topics: StringArray = records.iter().map(|r| Some(r.topic.as_str())).collect();

        let partitions: Int32Array = records.iter().map(|r| Some(r.partition)).collect();

        let offsets: Int64Array = records.iter().map(|r| Some(r.offset)).collect();

        // Build timestamp array with timezone using builder
        let timestamps = {
            use arrow_array::builder::TimestampMillisecondBuilder;
            let mut builder = TimestampMillisecondBuilder::new()
                .with_timezone("UTC");
            for record in records {
                builder.append_value(record.timestamp_ms);
            }
            builder.finish()
        };

        let timestamp_types: Int8Array = records.iter().map(|r| Some(r.timestamp_type)).collect();

        let keys: BinaryArray = records.iter().map(|r| r.key.as_deref()).collect();

        let values: BinaryArray = records
            .iter()
            .map(|r| Some(r.value.as_slice()))
            .collect();

        // For headers, we create a simplified representation for now
        // Full Map type support requires more complex construction
        let headers = self.build_headers_array(records)?;

        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(topics),
            Arc::new(partitions),
            Arc::new(offsets),
            Arc::new(timestamps),
            Arc::new(timestamp_types),
            Arc::new(keys),
            Arc::new(values),
            headers,
        ];

        // Handle embedding column if schema has it
        if let Some(embedding_dims) = self.embedding_dimensions() {
            let embedding_array = self.build_embedding_array(records, embedding_dims)?;
            columns.push(embedding_array);
        }

        // Verify we have the right number of columns
        if columns.len() != self.schema.fields().len() {
            return Err(anyhow!(
                "Column count mismatch: got {}, expected {}",
                columns.len(),
                self.schema.fields().len()
            ));
        }

        // Verify all columns have the same length
        for (i, col) in columns.iter().enumerate() {
            if col.len() != num_rows {
                return Err(anyhow!(
                    "Column {} has {} rows, expected {}",
                    i,
                    col.len(),
                    num_rows
                ));
            }
        }

        RecordBatch::try_new(Arc::new(self.schema.clone()), columns)
            .map_err(|e| anyhow!("Failed to create RecordBatch: {}", e))
    }

    /// Convert records to RecordBatch with additional JSON-extracted columns.
    ///
    /// Parses each record's `value` as JSON and extracts fields according to
    /// the provided `InferredJsonSchema`. Non-JSON values produce null columns.
    pub fn convert_with_json(
        &self,
        records: &[KafkaRecord],
        json_schema: &InferredJsonSchema,
    ) -> Result<RecordBatch> {
        if records.is_empty() {
            return Err(anyhow!("Cannot convert empty record batch"));
        }

        // Build base batch first (standard Kafka columns)
        let base_batch = self.convert(records)?;

        if json_schema.fields.is_empty() {
            return Ok(base_batch);
        }

        // Parse all values as JSON upfront
        let parsed: Vec<Option<serde_json::Map<String, serde_json::Value>>> = records
            .iter()
            .map(|r| {
                serde_json::from_slice::<serde_json::Value>(&r.value)
                    .ok()
                    .and_then(|v| v.as_object().cloned())
            })
            .collect();

        // Build extra columns from JSON fields
        let mut extra_columns: Vec<(String, ArrayRef)> = Vec::new();

        for field in &json_schema.fields {
            let array: ArrayRef = match field.field_type {
                JsonFieldType::String | JsonFieldType::Object | JsonFieldType::Array => {
                    let arr: StringArray = parsed
                        .iter()
                        .map(|obj| {
                            obj.as_ref().and_then(|o| o.get(&field.name)).and_then(|v| {
                                match v {
                                    serde_json::Value::String(s) => Some(s.clone()),
                                    serde_json::Value::Null => None,
                                    other => Some(other.to_string()),
                                }
                            })
                        })
                        .collect();
                    Arc::new(arr)
                }
                JsonFieldType::Int64 => {
                    let arr: Int64Array = parsed
                        .iter()
                        .map(|obj| {
                            obj.as_ref()
                                .and_then(|o| o.get(&field.name))
                                .and_then(|v| v.as_i64())
                        })
                        .collect();
                    Arc::new(arr)
                }
                JsonFieldType::Float64 => {
                    use arrow_array::Float64Array;
                    let arr: Float64Array = parsed
                        .iter()
                        .map(|obj| {
                            obj.as_ref()
                                .and_then(|o| o.get(&field.name))
                                .and_then(|v| v.as_f64())
                        })
                        .collect();
                    Arc::new(arr)
                }
                JsonFieldType::Boolean => {
                    use arrow_array::BooleanArray;
                    let arr: BooleanArray = parsed
                        .iter()
                        .map(|obj| {
                            obj.as_ref()
                                .and_then(|o| o.get(&field.name))
                                .and_then(|v| v.as_bool())
                        })
                        .collect();
                    Arc::new(arr)
                }
            };
            extra_columns.push((field.name.clone(), array));
        }

        // Build extended schema = base schema + JSON fields
        let mut all_fields: Vec<arrow_schema::FieldRef> =
            base_batch.schema().fields().iter().cloned().collect();
        for (name, _) in &extra_columns {
            let field_def = json_schema
                .fields
                .iter()
                .find(|f| f.name == *name)
                .unwrap();
            all_fields.push(Arc::new(arrow_schema::Field::new(
                name,
                field_def.field_type.to_arrow_type(),
                true, // JSON fields are always nullable (value might not be JSON)
            )));
        }

        let extended_schema = Arc::new(Schema::new(all_fields));

        // Combine base columns + extra columns
        let mut all_columns: Vec<ArrayRef> = (0..base_batch.num_columns())
            .map(|i| base_batch.column(i).clone())
            .collect();
        for (_, col) in extra_columns {
            all_columns.push(col);
        }

        RecordBatch::try_new(extended_schema, all_columns)
            .map_err(|e| anyhow!("Failed to create extended RecordBatch: {}", e))
    }

    /// Build the headers array as a Map type.
    fn build_headers_array(&self, records: &[KafkaRecord]) -> Result<ArrayRef> {
        use arrow_array::builder::{
            BinaryBuilder, MapBuilder, StringBuilder,
        };

        let key_builder = StringBuilder::new();
        let value_builder = BinaryBuilder::new();
        let mut map_builder = MapBuilder::new(None, key_builder, value_builder);

        for record in records {
            if record.headers.is_empty() {
                map_builder.append(true)?;
            } else {
                for (key, value) in &record.headers {
                    map_builder.keys().append_value(key);
                    match value {
                        Some(v) => map_builder.values().append_value(v),
                        None => map_builder.values().append_null(),
                    }
                }
                map_builder.append(true)?;
            }
        }

        Ok(Arc::new(map_builder.finish()))
    }

    /// Get the embedding dimensions if the schema has an _embedding column.
    fn embedding_dimensions(&self) -> Option<i32> {
        self.schema
            .field_with_name("_embedding")
            .ok()
            .and_then(|f| {
                if let DataType::FixedSizeList(_, dims) = f.data_type() {
                    Some(*dims)
                } else {
                    None
                }
            })
    }

    /// Build the embedding array as a FixedSizeList<Float32>.
    fn build_embedding_array(&self, records: &[KafkaRecord], dimensions: i32) -> Result<ArrayRef> {
        use arrow_array::builder::{FixedSizeListBuilder, Float32Builder};

        let value_builder = Float32Builder::new();
        let mut list_builder = FixedSizeListBuilder::new(value_builder, dimensions);

        for record in records {
            match &record.embedding {
                Some(embedding) => {
                    // Validate embedding dimensions
                    if embedding.len() != dimensions as usize {
                        return Err(anyhow!(
                            "Embedding dimension mismatch: got {}, expected {}",
                            embedding.len(),
                            dimensions
                        ));
                    }

                    // Append each value in the embedding
                    for value in embedding {
                        list_builder.values().append_value(*value);
                    }
                    list_builder.append(true);
                }
                None => {
                    // Append null embedding (placeholder zeros for the list, then null marker)
                    for _ in 0..dimensions {
                        list_builder.values().append_null();
                    }
                    list_builder.append(false); // Mark as null
                }
            }
        }

        Ok(Arc::new(list_builder.finish()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_record(offset: i64) -> KafkaRecord {
        KafkaRecord {
            topic: "test-topic".to_string(),
            partition: 0,
            offset,
            timestamp_ms: 1704067200000 + offset, // 2024-01-01 + offset ms
            timestamp_type: 0,
            key: Some(format!("key-{}", offset).into_bytes()),
            value: format!("value-{}", offset).into_bytes(),
            headers: vec![("header1".to_string(), Some(b"value1".to_vec()))],
            embedding: None,
        }
    }

    fn make_test_record_with_embedding(offset: i64, dims: usize) -> KafkaRecord {
        let mut record = make_test_record(offset);
        // Create a simple embedding: values based on offset
        record.embedding = Some((0..dims).map(|i| (offset as f32) + (i as f32) * 0.01).collect());
        record
    }

    #[test]
    fn test_convert_single_record() {
        let converter = RecordBatchConverter::new();
        let records = vec![make_test_record(0)];
        let batch = converter.convert(&records).unwrap();

        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 8);
    }

    #[test]
    fn test_convert_multiple_records() {
        let converter = RecordBatchConverter::new();
        let records: Vec<_> = (0..100).map(make_test_record).collect();
        let batch = converter.convert(&records).unwrap();

        assert_eq!(batch.num_rows(), 100);
    }

    #[test]
    fn test_convert_empty_fails() {
        let converter = RecordBatchConverter::new();
        let records: Vec<KafkaRecord> = vec![];
        assert!(converter.convert(&records).is_err());
    }

    #[test]
    fn test_null_key() {
        let converter = RecordBatchConverter::new();
        let mut record = make_test_record(0);
        record.key = None;
        let batch = converter.convert(&[record]).unwrap();

        let key_col = batch
            .column_by_name("_key")
            .unwrap()
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap();
        assert!(key_col.is_null(0));
    }

    #[test]
    fn test_empty_headers() {
        let converter = RecordBatchConverter::new();
        let mut record = make_test_record(0);
        record.headers = vec![];
        let batch = converter.convert(&[record]).unwrap();
        assert_eq!(batch.num_rows(), 1);
    }

    #[test]
    fn test_convert_with_embedding() {
        use crate::schema::kafka_message_schema_with_embedding;

        let schema = kafka_message_schema_with_embedding(384);
        let converter = RecordBatchConverter::with_schema(schema);

        let records: Vec<_> = (0..10)
            .map(|i| make_test_record_with_embedding(i, 384))
            .collect();

        let batch = converter.convert(&records).unwrap();

        assert_eq!(batch.num_rows(), 10);
        assert_eq!(batch.num_columns(), 9); // 8 base + embedding

        // Verify embedding column exists and has correct type
        let embedding_col = batch.column_by_name("_embedding").unwrap();
        let embedding_array = embedding_col
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .expect("Expected FixedSizeListArray");

        assert_eq!(embedding_array.len(), 10);
        assert_eq!(embedding_array.value_length(), 384);
    }

    #[test]
    fn test_convert_with_null_embeddings() {
        use crate::schema::kafka_message_schema_with_embedding;

        let schema = kafka_message_schema_with_embedding(384);
        let converter = RecordBatchConverter::with_schema(schema);

        // Mix of records with and without embeddings
        let mut records: Vec<_> = (0..5).map(|i| make_test_record(i)).collect();
        records.extend((5..10).map(|i| make_test_record_with_embedding(i, 384)));

        let batch = converter.convert(&records).unwrap();

        let embedding_col = batch.column_by_name("_embedding").unwrap();
        let embedding_array = embedding_col
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .expect("Expected FixedSizeListArray");

        // First 5 should be null, last 5 should have values
        for i in 0..5 {
            assert!(embedding_array.is_null(i), "Row {} should be null", i);
        }
        for i in 5..10 {
            assert!(!embedding_array.is_null(i), "Row {} should not be null", i);
        }
    }

    #[test]
    fn test_embedding_dimension_mismatch() {
        use crate::schema::kafka_message_schema_with_embedding;

        let schema = kafka_message_schema_with_embedding(384);
        let converter = RecordBatchConverter::with_schema(schema);

        // Create record with wrong embedding size
        let mut record = make_test_record(0);
        record.embedding = Some(vec![0.0; 100]); // Wrong size!

        let result = converter.convert(&[record]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("dimension mismatch"));
    }

    #[test]
    fn test_convert_with_json_basic() {
        let converter = RecordBatchConverter::new();
        let records = vec![
            KafkaRecord {
                topic: "test".to_string(),
                partition: 0,
                offset: 0,
                timestamp_ms: 1000,
                timestamp_type: 0,
                key: None,
                value: br#"{"name":"Alice","age":30,"score":9.5}"#.to_vec(),
                headers: vec![],
                embedding: None,
            },
            KafkaRecord {
                topic: "test".to_string(),
                partition: 0,
                offset: 1,
                timestamp_ms: 1001,
                timestamp_type: 0,
                key: None,
                value: br#"{"name":"Bob","age":25,"score":8.0}"#.to_vec(),
                headers: vec![],
                embedding: None,
            },
        ];

        let values: Vec<&[u8]> = records.iter().map(|r| r.value.as_slice()).collect();
        let json_schema = InferredJsonSchema::infer(&values, 64).unwrap();

        let batch = converter.convert_with_json(&records, &json_schema).unwrap();

        // 8 base + 3 JSON columns
        assert_eq!(batch.num_rows(), 2);
        assert!(batch.num_columns() > 8);

        // Verify JSON columns exist
        assert!(batch.column_by_name("name").is_some());
        assert!(batch.column_by_name("age").is_some());
        assert!(batch.column_by_name("score").is_some());

        // Verify values
        let name_col = batch.column_by_name("name").unwrap()
            .as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(name_col.value(0), "Alice");
        assert_eq!(name_col.value(1), "Bob");

        let age_col = batch.column_by_name("age").unwrap()
            .as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(age_col.value(0), 30);
        assert_eq!(age_col.value(1), 25);
    }

    #[test]
    fn test_convert_with_json_null_values() {
        let converter = RecordBatchConverter::new();
        let records = vec![
            KafkaRecord {
                topic: "test".to_string(),
                partition: 0,
                offset: 0,
                timestamp_ms: 1000,
                timestamp_type: 0,
                key: None,
                value: br#"{"name":"Alice"}"#.to_vec(), // missing "age"
                headers: vec![],
                embedding: None,
            },
            KafkaRecord {
                topic: "test".to_string(),
                partition: 0,
                offset: 1,
                timestamp_ms: 1001,
                timestamp_type: 0,
                key: None,
                value: b"not json at all".to_vec(), // not JSON
                headers: vec![],
                embedding: None,
            },
        ];

        let json_values = vec![br#"{"name":"Alice","age":30}"#.as_ref()];
        let json_schema = InferredJsonSchema::infer(&json_values, 64).unwrap();

        let batch = converter.convert_with_json(&records, &json_schema).unwrap();
        assert_eq!(batch.num_rows(), 2);

        let name_col = batch.column_by_name("name").unwrap()
            .as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(name_col.value(0), "Alice");
        assert!(name_col.is_null(1)); // not JSON → null

        let age_col = batch.column_by_name("age").unwrap()
            .as_any().downcast_ref::<Int64Array>().unwrap();
        assert!(age_col.is_null(0)); // missing field → null
        assert!(age_col.is_null(1)); // not JSON → null
    }

    #[test]
    fn test_embedding_values_preserved() {
        use crate::schema::kafka_message_schema_with_embedding;

        let schema = kafka_message_schema_with_embedding(4); // Small dims for easy testing
        let converter = RecordBatchConverter::with_schema(schema);

        let mut record = make_test_record(0);
        record.embedding = Some(vec![1.0, 2.0, 3.0, 4.0]);

        let batch = converter.convert(&[record]).unwrap();

        let embedding_col = batch.column_by_name("_embedding").unwrap();
        let embedding_array = embedding_col
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .expect("Expected FixedSizeListArray");

        // Get the inner values
        let values = embedding_array.value(0);
        let float_array = values.as_any().downcast_ref::<Float32Array>().unwrap();

        assert_eq!(float_array.value(0), 1.0);
        assert_eq!(float_array.value(1), 2.0);
        assert_eq!(float_array.value(2), 3.0);
        assert_eq!(float_array.value(3), 4.0);
    }
}
