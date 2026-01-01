//! Parquet segment reader for columnar storage.
//!
//! Reads Parquet files and returns Arrow RecordBatches with predicate pushdown support.

use anyhow::{anyhow, Result};
use arrow_array::RecordBatch;
use arrow_schema::Schema;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::ParquetMetaData;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

/// Reader for Parquet segment files.
pub struct ParquetSegmentReader {
    batch_size: usize,
}

impl Default for ParquetSegmentReader {
    fn default() -> Self {
        Self::new(8192)
    }
}

impl ParquetSegmentReader {
    /// Create a new reader with the specified batch size.
    pub fn new(batch_size: usize) -> Self {
        Self { batch_size }
    }

    /// Read all records from a Parquet file into memory.
    pub fn read_all(&self, path: &Path) -> Result<Vec<RecordBatch>> {
        let file = File::open(path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let reader = builder.with_batch_size(self.batch_size).build()?;

        let batches: Result<Vec<_>, _> = reader.collect();
        Ok(batches?)
    }

    /// Get metadata from a Parquet file without reading data.
    pub fn read_metadata(&self, path: &Path) -> Result<Arc<ParquetMetaData>> {
        let file = File::open(path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        Ok(builder.metadata().clone())
    }

    /// Get the schema from a Parquet file.
    pub fn read_schema(&self, path: &Path) -> Result<Schema> {
        let file = File::open(path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        Ok(builder.schema().as_ref().clone())
    }
}

/// Statistics about a Parquet file.
#[derive(Debug, Clone)]
pub struct ParquetFileStats {
    /// Total number of rows.
    pub num_rows: i64,
    /// Number of row groups.
    pub num_row_groups: usize,
    /// Total compressed size in bytes.
    pub compressed_size: i64,
    /// Column names.
    pub columns: Vec<String>,
}

impl ParquetFileStats {
    /// Create stats from metadata.
    pub fn from_metadata(meta: &Arc<ParquetMetaData>) -> Self {
        let num_rows = meta.row_groups().iter().map(|rg| rg.num_rows()).sum();
        let compressed_size = meta
            .row_groups()
            .iter()
            .map(|rg| rg.compressed_size())
            .sum();
        let columns = meta
            .file_metadata()
            .schema_descr()
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();

        Self {
            num_rows,
            num_row_groups: meta.num_row_groups(),
            compressed_size,
            columns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converter::{KafkaRecord, RecordBatchConverter};
    use crate::schema::kafka_message_schema;
    use crate::writer::ParquetSegmentWriter;
    use crate::config::ColumnarConfig;
    use tempfile::tempdir;

    fn make_test_records(count: usize) -> Vec<KafkaRecord> {
        (0..count)
            .map(|i| KafkaRecord {
                topic: "test-topic".to_string(),
                partition: 0,
                offset: i as i64,
                timestamp_ms: 1704067200000 + i as i64,
                timestamp_type: 0,
                key: Some(format!("key-{}", i).into_bytes()),
                value: format!("value-{}", i).into_bytes(),
                headers: vec![],
                embedding: None,
            })
            .collect()
    }

    fn write_test_file(path: &Path, num_records: usize) {
        let config = ColumnarConfig::default();
        let schema = kafka_message_schema();
        let writer = ParquetSegmentWriter::new(schema, config);

        let converter = RecordBatchConverter::new();
        let records = make_test_records(num_records);
        let batch = converter.convert(&records).unwrap();

        writer.write_to_file(&[batch], path).unwrap();
    }

    #[test]
    fn test_read_parquet_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 100);

        let reader = ParquetSegmentReader::default();
        let batches = reader.read_all(&path).unwrap();

        let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 100);
    }

    #[test]
    fn test_read_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 50);

        let reader = ParquetSegmentReader::default();
        let metadata = reader.read_metadata(&path).unwrap();

        let stats = ParquetFileStats::from_metadata(&metadata);
        assert_eq!(stats.num_rows, 50);
        assert!(stats.columns.contains(&"_topic".to_string()));
        assert!(stats.columns.contains(&"_offset".to_string()));
    }
}
