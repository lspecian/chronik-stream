//! Regression tests for issue #19: SQL tables must stay current after registration.
//!
//! Before the fix, a topic's Parquet segments were registered once as a fixed
//! file list, so every segment written afterwards was invisible to SQL:
//! `COUNT(*)` froze at whatever the topic held when the first query ran, and a
//! VIEW built on top of the table froze with it.

use std::sync::Arc;

use chronik_columnar::datafusion::arrow::array::{Int64Array, RecordBatch};
use chronik_columnar::datafusion::arrow::datatypes::{DataType, Field, Schema};
use chronik_columnar::{
    ColumnarQueryEngine, LiveParquetTableProvider, ParquetPathSource, DEFAULT_PARQUET_REFRESH_MS,
};
use parking_lot::Mutex;

/// A path source the test can grow between queries, standing in for the
/// WalIndexer appending Parquet segments.
#[derive(Debug, Default)]
struct MutablePaths(Mutex<Vec<String>>);

impl MutablePaths {
    fn push(&self, path: String) {
        self.0.lock().push(path);
    }
}

#[async_trait::async_trait]
impl ParquetPathSource for MutablePaths {
    async fn parquet_paths(&self) -> Vec<String> {
        self.0.lock().clone()
    }
}

fn test_schema() -> Schema {
    Schema::new(vec![Field::new("v", DataType::Int64, false)])
}

/// Write `values` as a Parquet file inside `dir` and return its path.
///
/// Uses DataFusion's own writer so the file matches the arrow version the
/// query engine reads with.
async fn write_parquet(dir: &std::path::Path, name: &str, values: Vec<i64>) -> String {
    use chronik_columnar::datafusion::dataframe::DataFrameWriteOptions;
    use chronik_columnar::datafusion::prelude::SessionContext;

    let schema = Arc::new(test_schema());
    let batch =
        RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(values))]).unwrap();

    let path = dir.join(name);
    let path_str = path.to_string_lossy().to_string();

    let ctx = SessionContext::new();
    ctx.read_batch(batch)
        .unwrap()
        .write_parquet(&path_str, DataFrameWriteOptions::new().with_single_file_output(true), None)
        .await
        .unwrap();

    path_str
}

async fn count(engine: &ColumnarQueryEngine, sql: &str) -> i64 {
    let batches = engine.execute_sql(sql).await.unwrap();
    let col = batches[0].column(0);
    col.as_any().downcast_ref::<Int64Array>().unwrap().value(0)
}

#[tokio::test]
async fn live_parquet_table_sees_segments_written_after_registration() {
    let dir = tempfile::tempdir().unwrap();
    let engine = ColumnarQueryEngine::new();

    let first = write_parquet(dir.path(), "a.parquet", vec![1, 2, 3]).await;
    let source = Arc::new(MutablePaths::default());
    source.push(first.clone());

    // Refresh interval 0: every scan re-resolves, so the test does not sleep.
    engine
        .register_live_parquet_table("t", source.clone(), &first, 0)
        .await
        .unwrap();

    assert_eq!(count(&engine, "SELECT COUNT(*) FROM t").await, 3);

    // The indexer writes another segment.
    source.push(write_parquet(dir.path(), "b.parquet", vec![4, 5]).await);

    assert_eq!(
        count(&engine, "SELECT COUNT(*) FROM t").await,
        5,
        "table registered once must include segments written later"
    );

    // And a third, to show it keeps tracking rather than refreshing once.
    source.push(write_parquet(dir.path(), "c.parquet", vec![6, 7, 8, 9]).await);
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM t").await, 9);
}

#[tokio::test]
async fn view_over_live_table_stays_current() {
    let dir = tempfile::tempdir().unwrap();
    let engine = ColumnarQueryEngine::new();

    let first = write_parquet(dir.path(), "a.parquet", vec![1, 2, 3]).await;
    let source = Arc::new(MutablePaths::default());
    source.push(first.clone());

    engine
        .register_live_parquet_table("t_cold", source.clone(), &first, 0)
        .await
        .unwrap();
    engine
        .register_view("t", "SELECT v FROM t_cold")
        .await
        .unwrap();

    assert_eq!(count(&engine, "SELECT COUNT(*) FROM t").await, 3);

    source.push(write_parquet(dir.path(), "b.parquet", vec![4, 5]).await);

    // A DataFusion view resolves its base provider at CREATE time, which is why
    // the provider (not the registration) has to be the live part.
    assert_eq!(
        count(&engine, "SELECT COUNT(*) FROM t").await,
        5,
        "view over a live table must see new segments too"
    );
}

#[tokio::test]
async fn live_parquet_table_tolerates_an_empty_segment_set() {
    let dir = tempfile::tempdir().unwrap();
    let sample = write_parquet(dir.path(), "sample.parquet", vec![1]).await;

    // Registered with a schema sample but no live paths: a topic whose segments
    // have all been deleted must answer "0 rows", not fail the query.
    let provider = Arc::new(LiveParquetTableProvider::new(
        Arc::new(MutablePaths::default()),
        Arc::new(test_schema()),
        DEFAULT_PARQUET_REFRESH_MS,
    ));

    let engine = ColumnarQueryEngine::new();
    engine.register_table_provider("t", provider).unwrap();

    assert_eq!(count(&engine, "SELECT COUNT(*) FROM t").await, 0);
    drop(sample);
}

#[tokio::test]
async fn registering_a_table_name_twice_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let engine = ColumnarQueryEngine::new();

    let a = write_parquet(dir.path(), "a.parquet", vec![1, 2, 3]).await;
    let source_a = Arc::new(MutablePaths::default());
    source_a.push(a.clone());
    engine
        .register_live_parquet_table("t", source_a, &a, 0)
        .await
        .unwrap();
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM t").await, 3);

    let b = write_parquet(dir.path(), "b.parquet", vec![7, 8]).await;
    let source_b = Arc::new(MutablePaths::default());
    source_b.push(b.clone());
    engine
        .register_live_parquet_table("t", source_b, &b, 0)
        .await
        .expect("re-registering an existing name must replace it, not error");
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM t").await, 2);
}
