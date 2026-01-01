//! Arrow schema definitions for Kafka messages.
//!
//! Provides standard and extended schemas for converting Kafka records to Arrow format.

use arrow_schema::{DataType, Field, Schema, TimeUnit};
use std::sync::Arc;

/// Create the standard Kafka message schema.
///
/// Standard columns:
/// - `_topic`: String (topic name)
/// - `_partition`: Int32 (partition number)
/// - `_offset`: Int64 (message offset)
/// - `_timestamp`: Timestamp(Millisecond) (message timestamp)
/// - `_timestamp_type`: Int8 (0=CreateTime, 1=LogAppendTime)
/// - `_key`: Binary (nullable, message key)
/// - `_value`: Binary (message value/payload)
/// - `_headers`: Map<String, Binary> (message headers)
pub fn kafka_message_schema() -> Schema {
    Schema::new(vec![
        Field::new("_topic", DataType::Utf8, false),
        Field::new("_partition", DataType::Int32, false),
        Field::new("_offset", DataType::Int64, false),
        Field::new(
            "_timestamp",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        Field::new("_timestamp_type", DataType::Int8, false),
        Field::new("_key", DataType::Binary, true),
        Field::new("_value", DataType::Binary, false),
        // Note: Arrow's MapBuilder uses "keys" and "values" (plural) as default field names
        Field::new(
            "_headers",
            DataType::Map(
                Arc::new(Field::new(
                    "entries",
                    DataType::Struct(
                        vec![
                            Field::new("keys", DataType::Utf8, false),
                            Field::new("values", DataType::Binary, true),
                        ]
                        .into(),
                    ),
                    false,
                )),
                false,
            ),
            true,
        ),
    ])
}

/// Builder for creating extended schemas with additional columns.
#[derive(Debug, Clone)]
pub struct KafkaSchemaBuilder {
    fields: Vec<Field>,
}

impl Default for KafkaSchemaBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl KafkaSchemaBuilder {
    /// Create a new schema builder with standard Kafka columns.
    pub fn new() -> Self {
        let base_schema = kafka_message_schema();
        Self {
            fields: base_schema.fields().iter().map(|f| f.as_ref().clone()).collect(),
        }
    }

    /// Add a string column extracted from JSON value.
    pub fn with_json_string_field(mut self, name: &str, json_path: &str, nullable: bool) -> Self {
        let field = Field::new(name, DataType::Utf8, nullable)
            .with_metadata([("json_path".to_string(), json_path.to_string())].into());
        self.fields.push(field);
        self
    }

    /// Add an integer column extracted from JSON value.
    pub fn with_json_int_field(mut self, name: &str, json_path: &str, nullable: bool) -> Self {
        let field = Field::new(name, DataType::Int64, nullable)
            .with_metadata([("json_path".to_string(), json_path.to_string())].into());
        self.fields.push(field);
        self
    }

    /// Add a float column extracted from JSON value.
    pub fn with_json_float_field(mut self, name: &str, json_path: &str, nullable: bool) -> Self {
        let field = Field::new(name, DataType::Float64, nullable)
            .with_metadata([("json_path".to_string(), json_path.to_string())].into());
        self.fields.push(field);
        self
    }

    /// Add a boolean column extracted from JSON value.
    pub fn with_json_bool_field(mut self, name: &str, json_path: &str, nullable: bool) -> Self {
        let field = Field::new(name, DataType::Boolean, nullable)
            .with_metadata([("json_path".to_string(), json_path.to_string())].into());
        self.fields.push(field);
        self
    }

    /// Add a custom field.
    pub fn with_field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    /// Add an embedding column for vector search.
    ///
    /// The embedding is stored as a FixedSizeList<Float32> with the specified dimensions.
    /// Common dimensions:
    /// - OpenAI text-embedding-3-small: 1536
    /// - OpenAI text-embedding-3-large: 3072
    /// - all-MiniLM-L6-v2: 384
    ///
    /// The column is nullable since embeddings are generated asynchronously
    /// and may not be available immediately. The inner float values are also
    /// nullable to support placeholder values when the embedding is null.
    pub fn with_embedding(mut self, dimensions: i32) -> Self {
        let field = Field::new(
            "_embedding",
            DataType::FixedSizeList(
                // Inner field is nullable to support null embeddings with placeholder values
                Arc::new(Field::new("item", DataType::Float32, true)),
                dimensions,
            ),
            true, // Nullable - embeddings generated async
        )
        .with_metadata([
            ("vector_dimensions".to_string(), dimensions.to_string()),
        ].into());
        self.fields.push(field);
        self
    }

    /// Check if this schema has an embedding column.
    pub fn has_embedding(&self) -> bool {
        self.fields.iter().any(|f| f.name() == "_embedding")
    }

    /// Get the embedding dimensions if present.
    pub fn embedding_dimensions(&self) -> Option<i32> {
        self.fields.iter()
            .find(|f| f.name() == "_embedding")
            .and_then(|f| {
                if let DataType::FixedSizeList(_, dims) = f.data_type() {
                    Some(*dims)
                } else {
                    None
                }
            })
    }

    /// Build the final schema.
    pub fn build(self) -> Schema {
        Schema::new(self.fields)
    }
}

/// Create a schema with embedding support for vector-enabled topics.
///
/// This creates the standard Kafka message schema plus an `_embedding` column
/// with the specified dimensions.
pub fn kafka_message_schema_with_embedding(dimensions: i32) -> Schema {
    KafkaSchemaBuilder::new()
        .with_embedding(dimensions)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kafka_message_schema() {
        let schema = kafka_message_schema();
        assert_eq!(schema.fields().len(), 8);
        assert!(schema.field_with_name("_topic").is_ok());
        assert!(schema.field_with_name("_partition").is_ok());
        assert!(schema.field_with_name("_offset").is_ok());
        assert!(schema.field_with_name("_timestamp").is_ok());
        assert!(schema.field_with_name("_key").is_ok());
        assert!(schema.field_with_name("_value").is_ok());
        assert!(schema.field_with_name("_headers").is_ok());
    }

    #[test]
    fn test_schema_builder() {
        let schema = KafkaSchemaBuilder::new()
            .with_json_string_field("user_id", "$.user.id", false)
            .with_json_int_field("amount", "$.transaction.amount", true)
            .build();

        assert_eq!(schema.fields().len(), 10); // 8 base + 2 custom
        assert!(schema.field_with_name("user_id").is_ok());
        assert!(schema.field_with_name("amount").is_ok());
    }

    #[test]
    fn test_key_is_nullable() {
        let schema = kafka_message_schema();
        let key_field = schema.field_with_name("_key").unwrap();
        assert!(key_field.is_nullable());
    }

    #[test]
    fn test_value_is_not_nullable() {
        let schema = kafka_message_schema();
        let value_field = schema.field_with_name("_value").unwrap();
        assert!(!value_field.is_nullable());
    }

    #[test]
    fn test_schema_with_embedding() {
        let schema = kafka_message_schema_with_embedding(1536);
        assert_eq!(schema.fields().len(), 9); // 8 base + embedding

        let embedding_field = schema.field_with_name("_embedding").unwrap();
        assert!(embedding_field.is_nullable());

        // Check it's a FixedSizeList<Float32>
        if let DataType::FixedSizeList(inner, dims) = embedding_field.data_type() {
            assert_eq!(*dims, 1536);
            assert_eq!(inner.data_type(), &DataType::Float32);
            // Inner field is nullable to support placeholder values for null embeddings
            assert!(inner.is_nullable());
        } else {
            panic!("Expected FixedSizeList type");
        }
    }

    #[test]
    fn test_schema_builder_with_embedding() {
        let builder = KafkaSchemaBuilder::new().with_embedding(384);

        assert!(builder.has_embedding());
        assert_eq!(builder.embedding_dimensions(), Some(384));

        let schema = builder.build();
        assert_eq!(schema.fields().len(), 9);
    }

    #[test]
    fn test_schema_builder_without_embedding() {
        let builder = KafkaSchemaBuilder::new();

        assert!(!builder.has_embedding());
        assert_eq!(builder.embedding_dimensions(), None);
    }

    #[test]
    fn test_embedding_metadata() {
        let schema = kafka_message_schema_with_embedding(3072);
        let embedding_field = schema.field_with_name("_embedding").unwrap();

        let metadata = embedding_field.metadata();
        assert_eq!(metadata.get("vector_dimensions"), Some(&"3072".to_string()));
    }
}
