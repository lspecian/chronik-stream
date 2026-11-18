//! Issue #73: projected columns over segments written before and after the
//! projection was configured, written by the production converter and writer.

use std::sync::Arc;

use chronik_columnar::converter::{KafkaRecord, RecordBatchConverter};
use chronik_columnar::datafusion::arrow::array::Int64Array;
use chronik_columnar::projection::{effective_columns, COLUMNS_KEY, INFERRED_KEY, PROJECTION_KEY};
use chronik_columnar::{ColumnarConfig, ColumnarQueryEngine, ParquetSegmentWriter, StaticParquetPaths};

fn record(offset: i64, json: &str) -> KafkaRecord {
    KafkaRecord {
        topic: "logs".into(),
        partition: 0,
        offset,
        timestamp_ms: 1_783_393_767_978,
        timestamp_type: 0,
        key: None,
        value: json.as_bytes().to_vec(),
        headers: vec![],
        embedding: None,
    }
}

/// signal-shaped log records; every third is an ERROR from "api".
fn logs(from: i64, n: i64) -> Vec<KafkaRecord> {
    (from..from + n)
        .map(|o| {
            let (sev, svc) = if o % 3 == 0 { ("ERROR", "api") } else { ("INFO", "web") };
            record(
                o,
                &format!(
                    r#"{{"severity":"{sev}","severity_number":{n},"resource":{{"service.name":"{svc}"}},"timestamp":"2026-07-07T03:09:{s:02}.000+00:00","body":"m{o}"}}"#,
                    n = if sev == "ERROR" { 17 } else { 9 },
                    s = o % 60
                ),
            )
        })
        .collect()
}

fn write(dir: &std::path::Path, name: &str, records: &[KafkaRecord], config: &std::collections::HashMap<String, String>) -> String {
    let (columns, problem) = effective_columns(config);
    assert!(problem.is_none(), "{problem:?}");
    let batch = RecordBatchConverter::new().convert_with_columns(records, &columns).unwrap();
    let writer = ParquetSegmentWriter::new((*batch.schema()).clone(), ColumnarConfig::default());
    let (bytes, _) = writer.write_to_bytes(&[batch]).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().to_string()
}

fn cfg(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn scalar(engine: &ColumnarQueryEngine, sql: &str) -> i64 {
    let batches = engine.execute_sql(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    batches[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap().value(0)
}

async fn plan(engine: &ColumnarQueryEngine, sql: &str) -> String {
    engine.explain(sql).await.unwrap()
}

/// The migration chronik-signal makes: a topic that ran under `auto` (so its
/// old segments carry `severity` and `timestamp` as text, and no
/// `service_name` at all) switches to `otlp-logs`. Old segments must answer
/// for the new columns, through the type change, exactly as new ones do.
#[tokio::test]
async fn switching_to_otlp_logs_keeps_the_history_queryable() {
    let dir = tempfile::tempdir().unwrap();
    let auto = cfg(&[(INFERRED_KEY, "severity:string,severity_number:int64,timestamp:string,body:string,resource:object")]);
    let old = write(dir.path(), "old.parquet", &logs(0, 300), &auto);
    let bare = write(dir.path(), "bare.parquet", &logs(300, 300), &cfg(&[(PROJECTION_KEY, "none")]));
    let otlp = cfg(&[(PROJECTION_KEY, "otlp-logs")]);
    let new = write(dir.path(), "new.parquet", &logs(600, 300), &otlp);

    let (projected, _) = effective_columns(&otlp);
    let engine = ColumnarQueryEngine::new();
    engine
        .register_live_parquet_table(
            "logs_cold",
            Arc::new(StaticParquetPaths(vec![old, bare, new.clone()])),
            &new,
            0,
            &projected,
        )
        .await
        .unwrap();

    // 900 records, every third an ERROR from "api", in all three segments.
    assert_eq!(scalar(&engine, "SELECT count(*) FROM logs_cold WHERE severity = 'ERROR'").await, 300);
    assert_eq!(scalar(&engine, "SELECT count(*) FROM logs_cold WHERE service_name = 'api'").await, 300);
    assert_eq!(scalar(&engine, "SELECT count(DISTINCT service_name) FROM logs_cold").await, 2);
    assert_eq!(scalar(&engine, "SELECT sum(severity_number) FROM logs_cold WHERE severity = 'ERROR'").await, 300 * 17);
    // `timestamp` was text in the old segment and is a timestamp now.
    assert_eq!(
        scalar(&engine, "SELECT count(*) FROM logs_cold WHERE timestamp >= TIMESTAMP '2026-07-07T03:09:30Z'").await,
        450
    );
    let grouped = engine
        .execute_sql("SELECT service_name, count(*) AS n FROM logs_cold GROUP BY service_name ORDER BY service_name")
        .await
        .unwrap();
    let out = chronik_columnar::datafusion::arrow::util::pretty::pretty_format_batches(&grouped).unwrap().to_string();
    assert!(out.contains("| api          | 300 |") && out.contains("| web          | 600 |"), "{out}");

    // The scan reads the projected column, not `_value`.
    let p = plan(&engine, "SELECT count(*) FROM logs_cold WHERE severity = 'ERROR'").await;
    assert!(p.contains("projection=[severity]"), "{p}");
    assert!(!p.contains("_value"), "{p}");
}

/// A declared column over a topic whose segments never had it.
#[tokio::test]
async fn a_declared_column_answers_for_segments_written_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let before = write(dir.path(), "a.parquet", &logs(0, 90), &cfg(&[(PROJECTION_KEY, "none")]));
    let declared = cfg(&[(PROJECTION_KEY, "none"), (COLUMNS_KEY, "svc:/resource/service.name;sevnum:int64:/severity_number")]);
    let after = write(dir.path(), "b.parquet", &logs(90, 90), &declared);
    let (projected, _) = effective_columns(&declared);
    let engine = ColumnarQueryEngine::new();
    engine
        .register_live_parquet_table("t", Arc::new(StaticParquetPaths(vec![before.clone(), after])), &before, 0, &projected)
        .await
        .unwrap();
    assert_eq!(scalar(&engine, "SELECT count(*) FROM t WHERE svc = 'api'").await, 60);
    assert_eq!(scalar(&engine, "SELECT max(sevnum) FROM t").await, 17);
    assert_eq!(scalar(&engine, "SELECT count(*) FROM t WHERE svc IS NULL").await, 0);
}
