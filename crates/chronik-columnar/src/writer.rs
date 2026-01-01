//! Parquet segment writer for columnar storage.
//!
//! Writes Arrow RecordBatches to Parquet files with configurable compression,
//! row groups, bloom filters, and statistics.

use anyhow::{anyhow, Result};
use arrow_array::RecordBatch;
use arrow_schema::Schema;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, Encoding};
use parquet::file::properties::{WriterProperties, WriterPropertiesBuilder};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{ColumnarConfig, CompressionCodec, StatisticsLevel};

/// Writer for creating Parquet segment files from Arrow RecordBatches.
pub struct ParquetSegmentWriter {
    config: ColumnarConfig,
    schema: Arc<Schema>,
}

impl ParquetSegmentWriter {
    /// Create a new Parquet segment writer.
    pub fn new(schema: Schema, config: ColumnarConfig) -> Self {
        Self {
            config,
            schema: Arc::new(schema),
        }
    }

    /// Get the schema used by this writer.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Write a batch of records to a Parquet file.
    pub fn write_to_file(&self, batches: &[RecordBatch], path: &Path) -> Result<ParquetWriteStats> {
        let file = File::create(path)?;
        self.write_to_writer(batches, file)
    }

    /// Write batches to any writer implementing Write.
    pub fn write_to_writer<W: Write + Send>(
        &self,
        batches: &[RecordBatch],
        writer: W,
    ) -> Result<ParquetWriteStats> {
        if batches.is_empty() {
            return Err(anyhow!("Cannot write empty batch list"));
        }

        let props = self.build_writer_properties()?;
        let mut arrow_writer = ArrowWriter::try_new(writer, self.schema.clone(), Some(props))?;

        let mut total_rows = 0;
        for batch in batches {
            total_rows += batch.num_rows();
            arrow_writer.write(batch)?;
        }

        let metadata = arrow_writer.close()?;

        Ok(ParquetWriteStats {
            num_rows: total_rows,
            num_row_groups: metadata.row_groups.len(),
            file_size_bytes: metadata
                .row_groups
                .iter()
                .map(|rg| rg.total_byte_size as u64)
                .sum(),
        })
    }

    /// Write batches to a byte buffer.
    pub fn write_to_bytes(&self, batches: &[RecordBatch]) -> Result<(Vec<u8>, ParquetWriteStats)> {
        let mut buffer = Vec::new();
        let stats = self.write_to_writer(batches, &mut buffer)?;
        Ok((buffer, stats))
    }

    /// Build Parquet writer properties from config.
    fn build_writer_properties(&self) -> Result<WriterProperties> {
        let mut builder = WriterProperties::builder()
            .set_compression(self.map_compression())
            .set_max_row_group_size(self.config.row_group_size)
            .set_data_page_size_limit(self.config.page_size)
            .set_dictionary_enabled(self.config.dictionary_enabled)
            .set_encoding(Encoding::PLAIN);

        // Set statistics level
        builder = self.set_statistics_level(builder);

        // Configure bloom filters for specified columns
        if self.config.bloom_filter_enabled {
            builder = self.configure_bloom_filters(builder);
        }

        Ok(builder.build())
    }

    /// Map our compression codec to Parquet compression.
    fn map_compression(&self) -> Compression {
        match self.config.compression {
            CompressionCodec::None => Compression::UNCOMPRESSED,
            CompressionCodec::Snappy => Compression::SNAPPY,
            CompressionCodec::Gzip => Compression::GZIP(Default::default()),
            CompressionCodec::Lz4 => Compression::LZ4,
            CompressionCodec::Zstd => {
                Compression::ZSTD(parquet::basic::ZstdLevel::try_new(
                    self.config.compression_level as i32,
                ).unwrap_or_default())
            }
        }
    }

    /// Set statistics collection level.
    fn set_statistics_level(&self, builder: WriterPropertiesBuilder) -> WriterPropertiesBuilder {
        match self.config.statistics {
            StatisticsLevel::None => builder.set_statistics_enabled(
                parquet::file::properties::EnabledStatistics::None,
            ),
            StatisticsLevel::Chunk => builder.set_statistics_enabled(
                parquet::file::properties::EnabledStatistics::Chunk,
            ),
            StatisticsLevel::Full => builder.set_statistics_enabled(
                parquet::file::properties::EnabledStatistics::Page,
            ),
        }
    }

    /// Configure bloom filters for specified columns.
    fn configure_bloom_filters(&self, mut builder: WriterPropertiesBuilder) -> WriterPropertiesBuilder {
        for col_name in &self.config.bloom_filter_columns {
            // Parquet bloom filter configuration
            builder = builder.set_column_bloom_filter_enabled(
                parquet::schema::types::ColumnPath::from(col_name.as_str()),
                true,
            );
        }
        builder
    }
}

/// Statistics from a Parquet write operation.
#[derive(Debug, Clone)]
pub struct ParquetWriteStats {
    /// Total number of rows written.
    pub num_rows: usize,
    /// Number of row groups created.
    pub num_row_groups: usize,
    /// Total file size in bytes.
    pub file_size_bytes: u64,
}

/// Generate the Parquet file path for a topic/partition/segment.
pub fn parquet_path(
    base_dir: &Path,
    topic: &str,
    partition: i32,
    segment_id: u64,
    config: &ColumnarConfig,
) -> PathBuf {
    use crate::config::PartitioningStrategy;

    match config.partitioning {
        PartitioningStrategy::None => {
            base_dir
                .join(topic)
                .join(format!("partition={}", partition))
                .join(format!("{:020}.parquet", segment_id))
        }
        PartitioningStrategy::Hourly => {
            // For hourly partitioning, segment_id encodes the hour
            let hour = segment_id / 3600;
            base_dir
                .join(topic)
                .join(format!("partition={}", partition))
                .join(format!("hour={:010}", hour))
                .join(format!("{:020}.parquet", segment_id))
        }
        PartitioningStrategy::Daily => {
            // For daily partitioning, segment_id encodes the day
            let day = segment_id / 86400;
            base_dir
                .join(topic)
                .join(format!("partition={}", partition))
                .join(format!("day={:010}", day))
                .join(format!("{:020}.parquet", segment_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converter::{KafkaRecord, RecordBatchConverter};
    use crate::schema::kafka_message_schema;
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

    #[test]
    fn test_write_parquet_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");

        let config = ColumnarConfig::default();
        let schema = kafka_message_schema();
        let writer = ParquetSegmentWriter::new(schema, config);

        let converter = RecordBatchConverter::new();
        let records = make_test_records(100);
        let batch = converter.convert(&records).unwrap();

        let stats = writer.write_to_file(&[batch], &path).unwrap();
        assert_eq!(stats.num_rows, 100);
        assert!(path.exists());
    }

    #[test]
    fn test_write_to_bytes() {
        let config = ColumnarConfig::default();
        let schema = kafka_message_schema();
        let writer = ParquetSegmentWriter::new(schema, config);

        let converter = RecordBatchConverter::new();
        let records = make_test_records(50);
        let batch = converter.convert(&records).unwrap();

        let (bytes, stats) = writer.write_to_bytes(&[batch]).unwrap();
        assert_eq!(stats.num_rows, 50);
        assert!(!bytes.is_empty());
        // Parquet magic bytes: PAR1
        assert_eq!(&bytes[..4], b"PAR1");
    }

    #[test]
    fn test_compression_mapping() {
        let mut config = ColumnarConfig::default();
        config.compression = CompressionCodec::Snappy;

        let schema = kafka_message_schema();
        let writer = ParquetSegmentWriter::new(schema, config);

        let compression = writer.map_compression();
        assert!(matches!(compression, Compression::SNAPPY));
    }

    #[test]
    fn test_parquet_path_no_partitioning() {
        let config = ColumnarConfig::default();
        let path = parquet_path(
            Path::new("/data"),
            "my-topic",
            0,
            12345,
            &config,
        );
        assert_eq!(
            path.to_string_lossy(),
            "/data/my-topic/partition=0/00000000000000012345.parquet"
        );
    }

    #[test]
    fn test_parquet_path_daily_partitioning() {
        let mut config = ColumnarConfig::default();
        config.partitioning = crate::config::PartitioningStrategy::Daily;

        let path = parquet_path(
            Path::new("/data"),
            "my-topic",
            2,
            172800, // 2 days in seconds
            &config,
        );
        assert!(path.to_string_lossy().contains("day="));
    }
}
