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

/// The offsets a Parquet segment actually holds, as sorted runs of
/// consecutive offsets (inclusive). Reads only the `_offset` column.
///
/// A segment's name and metadata give its min and max offset, but not which
/// offsets lie between: a flush that skipped offsets already in cold leaves
/// holes. Anything deciding that a segment is redundant must look here.
pub fn offset_runs(path: &Path) -> Result<Vec<(i64, i64)>> {
    use arrow_array::Int64Array;
    use parquet::arrow::ProjectionMask;

    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let index = builder
        .parquet_schema()
        .columns()
        .iter()
        .position(|c| c.path().parts() == ["_offset"])
        .ok_or_else(|| anyhow!("{} has no _offset column", path.display()))?;
    let mask = ProjectionMask::leaves(builder.parquet_schema(), [index]);
    let reader = builder.with_projection(mask).with_batch_size(65_536).build()?;

    let mut offsets: Vec<i64> = Vec::new();
    for batch in reader {
        let batch = batch?;
        let column = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| anyhow!("{}: _offset is not Int64", path.display()))?;
        offsets.extend(column.iter().flatten());
    }
    offsets.sort_unstable();
    offsets.dedup();
    let mut runs: Vec<(i64, i64)> = Vec::new();
    for o in offsets {
        match runs.last_mut() {
            Some(last) if o == last.1 + 1 => last.1 = o,
            _ => runs.push((o, o)),
        }
    }
    Ok(runs)
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
    fn offset_runs_reports_the_offsets_actually_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("holey.parquet");
        // Offsets 0-49 and 150-199: a segment named 0-199 with a hole.
        let records: Vec<KafkaRecord> =
            make_test_records(200).into_iter().filter(|r| r.offset < 50 || r.offset >= 150).collect();
        let batch = RecordBatchConverter::new().convert(&records).unwrap();
        ParquetSegmentWriter::new(kafka_message_schema(), ColumnarConfig::default())
            .write_to_file(&[batch], &path)
            .unwrap();
        assert_eq!(offset_runs(&path).unwrap(), vec![(0, 49), (150, 199)]);

        let full = dir.path().join("full.parquet");
        write_test_file(&full, 100);
        assert_eq!(offset_runs(&full).unwrap(), vec![(0, 99)]);
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
