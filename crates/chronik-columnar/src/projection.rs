//! Typed columns projected out of a topic's message values (issue #73).
//!
//! A columnar topic always stores the Kafka envelope (`_topic`, `_partition`,
//! `_offset`, `_timestamp`, `_key`, `_value`, ...). A **projection** adds typed
//! columns read out of each JSON `_value`, written into the same Parquet
//! segments, so SQL filters and aggregates on them read one narrow column
//! (with min/max pruning) instead of decoding every `_value`.
//!
//! ## Configuration (topic configs)
//!
//! | key | value |
//! |---|---|
//! | `columnar.projection` | `auto` (default): infer top-level scalar fields from the data. `otlp-logs`: the OpenTelemetry log-record mapping below. `none`: only declared columns. |
//! | `columnar.columns` | Declared columns, `;`-separated: `name:type:/json/pointer`. Override a same-named auto or preset column. |
//! | `columnar.projection.attributes` | For `otlp-logs`: attribute keys, `;`-separated, each projected as `attr_<key>` (string). |
//!
//! A column is `name[:type][:/path[|/path...]]`:
//! - `name`: letters, digits and `_`, not starting with a digit or `_` (the `_`
//!   prefix is the envelope's).
//! - `type`: `string`, `int64`, `float64`, `boolean`, `timestamp`, `json`.
//!   Omitted, it is `string`.
//! - `path`: a JSON Pointer (RFC 6901) into the value — `/resource/service.name`
//!   reads key `service.name` of object `resource` (`~1` is `/`, `~0` is `~`).
//!   Alternatives separated by `|` are tried in order; the first present and
//!   non-null wins. Omitted, it is `/<name>`.
//!
//! `;` separates columns because `kafka-configs.sh --add-config` already uses
//! `,` between configs.
//!
//! ## Conversions
//!
//! One function, [`ColumnSpec::extract`], converts for both the Parquet writer
//! and the SQL hot side (`json_project_*` UDFs), so a value reads the same
//! whichever tier holds its record.
//!
//! - `string`: strings as-is; numbers and booleans as text; objects and arrays
//!   as JSON text.
//! - `int64` / `float64`: numbers, and numeric strings (OTLP JSON encodes
//!   64-bit integers as strings). A float with no fraction is a valid int64.
//! - `boolean`: booleans, and the strings `true` / `false`.
//! - `timestamp` (UTC, nanoseconds): RFC 3339 strings; numbers or numeric
//!   strings as epoch time, the unit read from the magnitude (seconds below
//!   1e11, milliseconds below 1e14, microseconds below 1e17, else nanoseconds).
//! - `json`: any value as JSON text.
//!
//! Anything that does not convert is NULL. `_value` itself is never changed.

use arrow_schema::{DataType, Field, TimeUnit};
use serde_json::Value;
use std::collections::HashMap;

use crate::json_schema::{InferredJsonSchema, JsonFieldType};

/// Topic config: the projection mode.
pub const PROJECTION_KEY: &str = "columnar.projection";
/// Topic config: declared columns.
pub const COLUMNS_KEY: &str = "columnar.columns";
/// Topic config: OTLP attribute keys to project.
pub const OTLP_ATTRIBUTES_KEY: &str = "columnar.projection.attributes";
/// Topic config: the auto-inferred field list (written by the indexer).
pub const INFERRED_KEY: &str = "columnar.json_schema";

/// The timezone every projected timestamp carries.
pub const TIMESTAMP_TZ: &str = "UTC";

/// A projected column's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnType {
    String,
    Int64,
    Float64,
    Boolean,
    Timestamp,
    Json,
}

impl ColumnType {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "string" | "str" | "text" | "utf8" => Self::String,
            "int64" | "int" | "long" | "bigint" => Self::Int64,
            "float64" | "float" | "double" => Self::Float64,
            "boolean" | "bool" => Self::Boolean,
            "timestamp" | "ts" => Self::Timestamp,
            "json" => Self::Json,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Int64 => "int64",
            Self::Float64 => "float64",
            Self::Boolean => "boolean",
            Self::Timestamp => "timestamp",
            Self::Json => "json",
        }
    }

    /// The same type in the Arrow version DataFusion uses (the crate links two:
    /// the writer's and DataFusion's).
    pub fn sql_arrow_type(self) -> datafusion::arrow::datatypes::DataType {
        use datafusion::arrow::datatypes::{DataType as D, TimeUnit as U};
        match self {
            Self::String | Self::Json => D::Utf8,
            Self::Int64 => D::Int64,
            Self::Float64 => D::Float64,
            Self::Boolean => D::Boolean,
            Self::Timestamp => D::Timestamp(U::Nanosecond, Some(TIMESTAMP_TZ.into())),
        }
    }

    /// The Arrow type of the column, in Parquet and in SQL.
    pub fn arrow_type(self) -> DataType {
        match self {
            Self::String | Self::Json => DataType::Utf8,
            Self::Int64 => DataType::Int64,
            Self::Float64 => DataType::Float64,
            Self::Boolean => DataType::Boolean,
            Self::Timestamp => DataType::Timestamp(TimeUnit::Nanosecond, Some(TIMESTAMP_TZ.into())),
        }
    }
}

/// A value projected out of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Projected {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Nanoseconds since the Unix epoch, UTC.
    TimestampNanos(i64),
}

/// One projected column.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSpec {
    pub name: String,
    pub ty: ColumnType,
    /// Alternative JSON Pointers, already split into unescaped keys.
    pub paths: Vec<Vec<String>>,
}

impl ColumnSpec {
    pub fn new(name: &str, ty: ColumnType, paths: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            ty,
            paths: paths.iter().map(|p| parse_pointer(p)).collect(),
        }
    }

    pub fn arrow_field(&self) -> Field {
        Field::new(&self.name, self.ty.arrow_type(), true)
    }

    /// The paths as written in config (`/a/b|/c`), for the SQL hot side.
    pub fn paths_literal(&self) -> String {
        self.paths
            .iter()
            .map(|keys| keys.iter().map(|k| format!("/{}", k.replace('~', "~0").replace('/', "~1"))).collect::<String>())
            .collect::<Vec<_>>()
            .join("|")
    }

    /// The value of this column for a parsed message.
    pub fn extract(&self, message: &Value) -> Option<Projected> {
        let value = self.paths.iter().find_map(|keys| lookup(message, keys).filter(|v| !v.is_null()))?;
        convert(value, self.ty)
    }

    /// Parse one `name[:type][:/path[|/path...]]` entry.
    ///
    /// The path is told apart by its leading `/`, so it may itself contain
    /// `:`. There is no `=` anywhere: `kafka-configs.sh --add-config` refuses
    /// a value containing one.
    pub fn parse(entry: &str) -> Result<Self, String> {
        let entry = entry.trim();
        let (name, rest) = match entry.split_once(':') {
            Some((n, r)) => (n.trim(), Some(r.trim())),
            None => (entry, None),
        };
        validate_name(name)?;
        let (ty, paths) = match rest {
            None | Some("") => (ColumnType::String, None),
            Some(r) if r.starts_with('/') => (ColumnType::String, Some(r)),
            Some(r) => {
                let (t, p) = match r.split_once(':') {
                    Some((t, p)) => (t.trim(), Some(p.trim())),
                    None => (r, None),
                };
                let ty = ColumnType::parse(t).ok_or_else(|| format!("column '{name}': unknown type '{t}'"))?;
                if let Some(p) = p.filter(|p| !p.is_empty() && !p.starts_with('/')) {
                    return Err(format!("column '{name}': path '{p}' must be a JSON pointer starting with '/'"));
                }
                (ty, p.filter(|p| !p.is_empty()))
            }
        };
        let paths: Vec<Vec<String>> = match paths {
            Some(p) => p
                .split('|')
                .map(|p| {
                    let p = p.trim();
                    if p.starts_with('/') {
                        Ok(parse_pointer(p))
                    } else {
                        Err(format!("column '{name}': path '{p}' must be a JSON pointer starting with '/'"))
                    }
                })
                .collect::<Result<_, _>>()?,
            None => vec![vec![name.to_string()]],
        };
        Ok(Self { name: name.to_string(), ty, paths })
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok_first = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    if !ok_first || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "column name '{name}' must start with a letter and hold only letters, digits and '_'"
        ));
    }
    Ok(())
}

/// Split a JSON Pointer into unescaped keys. A path not starting with `/` is a
/// single top-level key.
fn parse_pointer(p: &str) -> Vec<String> {
    match p.strip_prefix('/') {
        Some(rest) => rest.split('/').map(|k| k.replace("~1", "/").replace("~0", "~")).collect(),
        None => vec![p.to_string()],
    }
}

fn lookup<'a>(mut value: &'a Value, keys: &[String]) -> Option<&'a Value> {
    for key in keys {
        value = match value {
            Value::Object(map) => map.get(key)?,
            Value::Array(items) => items.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(value)
}

fn convert(value: &Value, ty: ColumnType) -> Option<Projected> {
    match ty {
        ColumnType::String => Some(Projected::Str(match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })),
        ColumnType::Json => Some(Projected::Str(value.to_string())),
        ColumnType::Int64 => match value {
            Value::Number(n) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.2e18).map(|f| f as i64))
                .map(Projected::Int),
            Value::String(s) => s.trim().parse::<i64>().ok().map(Projected::Int),
            _ => None,
        },
        ColumnType::Float64 => match value {
            Value::Number(n) => n.as_f64().map(Projected::Float),
            Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()).map(Projected::Float),
            _ => None,
        },
        ColumnType::Boolean => match value {
            Value::Bool(b) => Some(Projected::Bool(*b)),
            Value::String(s) if s.eq_ignore_ascii_case("true") => Some(Projected::Bool(true)),
            Value::String(s) if s.eq_ignore_ascii_case("false") => Some(Projected::Bool(false)),
            _ => None,
        },
        ColumnType::Timestamp => match value {
            Value::Number(n) => match n.as_i64() {
                Some(i) => int_epoch_to_nanos(i),
                None => n.as_f64().and_then(epoch_to_nanos),
            }
            .map(Projected::TimestampNanos),
            Value::String(s) => parse_timestamp(s).map(Projected::TimestampNanos),
            _ => None,
        },
    }
}

/// Epoch time in an unknown unit, read from its magnitude.
fn epoch_to_nanos(v: f64) -> Option<i64> {
    if !v.is_finite() {
        return None;
    }
    let a = v.abs();
    let nanos = if a < 1e11 {
        v * 1e9
    } else if a < 1e14 {
        v * 1e6
    } else if a < 1e17 {
        v * 1e3
    } else {
        v
    };
    (nanos.abs() < 9.2e18).then_some(nanos as i64)
}

/// Integer epoch time (OTLP `timeUnixNano`, often a string), exactly, without
/// the precision a round-trip through f64 would lose.
fn int_epoch_to_nanos(n: i64) -> Option<i64> {
    let a = n.unsigned_abs();
    if a < 100_000_000_000 {
        n.checked_mul(1_000_000_000)
    } else if a < 100_000_000_000_000 {
        n.checked_mul(1_000_000)
    } else if a < 100_000_000_000_000_000 {
        n.checked_mul(1_000)
    } else {
        Some(n)
    }
}

fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return int_epoch_to_nanos(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return epoch_to_nanos(f);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp_nanos_opt();
    }
    // Common non-RFC forms, read as UTC.
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return dt.and_utc().timestamp_nanos_opt();
        }
    }
    None
}

/// The projection mode of a topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionMode {
    Auto,
    None,
    OtlpLogs,
}

impl ProjectionMode {
    pub fn of(config: &HashMap<String, String>) -> Result<Self, String> {
        match config.get(PROJECTION_KEY).map(|s| s.trim().to_ascii_lowercase()) {
            None => Ok(Self::Auto),
            Some(s) if s.is_empty() || s == "auto" => Ok(Self::Auto),
            Some(s) if s == "none" => Ok(Self::None),
            Some(s) if s == "otlp-logs" || s == "otlp_logs" => Ok(Self::OtlpLogs),
            Some(other) => Err(format!(
                "{PROJECTION_KEY}={other}: expected auto, none or otlp-logs"
            )),
        }
    }
}

/// The OpenTelemetry log-record mapping (`columnar.projection=otlp-logs`).
///
/// One log record per message, in the flattened shape chronik-signal writes
/// (`severity`, `resource: {"service.name": ...}`, `attributes: {...}`), with
/// the OTLP/JSON `LogRecord` field names as alternatives (`severityText`,
/// `timeUnixNano`, `body: {"stringValue": ...}`).
pub fn otlp_logs_columns(attributes: &[String]) -> Vec<ColumnSpec> {
    use ColumnType::*;
    let mut cols = vec![
        ColumnSpec::new("timestamp", Timestamp, &["/timestamp", "/timeUnixNano", "/time_unix_nano"]),
        ColumnSpec::new(
            "observed_timestamp",
            Timestamp,
            &["/observed_timestamp", "/observedTimeUnixNano", "/observed_time_unix_nano"],
        ),
        ColumnSpec::new("severity", String, &["/severity", "/severity_text", "/severityText"]),
        ColumnSpec::new("severity_text", String, &["/severity_text", "/severityText"]),
        ColumnSpec::new("severity_number", Int64, &["/severity_number", "/severityNumber"]),
        ColumnSpec::new("body", String, &["/body/stringValue", "/body"]),
        ColumnSpec::new("service_name", String, &["/resource/service.name", "/resource/attributes/service.name"]),
        ColumnSpec::new("trace_id", String, &["/trace_id", "/traceId"]),
        ColumnSpec::new("span_id", String, &["/span_id", "/spanId"]),
        ColumnSpec::new("scope_name", String, &["/scope/name"]),
        ColumnSpec::new("attributes", Json, &["/attributes"]),
        ColumnSpec::new("resource", Json, &["/resource"]),
    ];
    for key in attributes {
        let name: std::string::String = format!(
            "attr_{}",
            key.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect::<std::string::String>()
        );
        let pointer = format!("/attributes/{}", key.replace('~', "~0").replace('/', "~1"));
        cols.push(ColumnSpec { name, ty: String, paths: vec![parse_pointer(&pointer)] });
    }
    cols
}

/// Parse a `;`-separated column list.
pub fn parse_columns(list: &str) -> Result<Vec<ColumnSpec>, String> {
    list.split(';').filter(|e| !e.trim().is_empty()).map(ColumnSpec::parse).collect()
}

/// Check a topic's projection configs; the error names the bad entry.
pub fn validate_config(config: &HashMap<String, String>) -> Result<(), String> {
    ProjectionMode::of(config)?;
    if let Some(list) = config.get(COLUMNS_KEY) {
        let cols = parse_columns(list)?;
        let mut seen = std::collections::HashSet::new();
        for c in &cols {
            if !seen.insert(c.name.as_str()) {
                return Err(format!("{COLUMNS_KEY}: column '{}' is declared twice", c.name));
            }
        }
    }
    Ok(())
}

/// The auto-inferred field list stored in the topic config, as columns.
pub fn inferred_columns(config: &HashMap<String, String>) -> Vec<ColumnSpec> {
    config
        .get(INFERRED_KEY)
        .and_then(|s| InferredJsonSchema::from_config_string(s))
        .map(|schema| schema.fields.iter().filter_map(column_from_inferred).collect())
        .unwrap_or_default()
}

/// One inferred field as a column: a top-level key, read by its name.
pub fn column_from_inferred(f: &crate::json_schema::InferredField) -> Option<ColumnSpec> {
    // Inferred names are raw JSON keys; one that is not a valid column name
    // (or collides with the `_` envelope) has never been projectable under
    // that name and is skipped, as the view always skipped `_` names.
    validate_name(&f.name).ok()?;
    let ty = match f.field_type {
        JsonFieldType::String | JsonFieldType::Object | JsonFieldType::Array => ColumnType::String,
        JsonFieldType::Int64 => ColumnType::Int64,
        JsonFieldType::Float64 => ColumnType::Float64,
        JsonFieldType::Boolean => ColumnType::Boolean,
    };
    Some(ColumnSpec { name: f.name.clone(), ty, paths: vec![vec![f.name.clone()]] })
}

/// The columns a topic projects, in order: the mode's columns (auto: the
/// stored inferred list; otlp-logs: the preset), with declared columns
/// replacing same-named ones and the rest appended. An invalid declaration is
/// skipped with the reason returned alongside, so one typo does not stop the
/// topic's other columns (validation at config time reports it to the user).
pub fn effective_columns(config: &HashMap<String, String>) -> (Vec<ColumnSpec>, Option<String>) {
    let mut problem = None;
    let mode = ProjectionMode::of(config).unwrap_or_else(|e| {
        problem = Some(e);
        ProjectionMode::Auto
    });
    let mut cols = match mode {
        ProjectionMode::Auto => inferred_columns(config),
        ProjectionMode::None => Vec::new(),
        ProjectionMode::OtlpLogs => {
            let attrs: Vec<String> = config
                .get(OTLP_ATTRIBUTES_KEY)
                .map(|s| s.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
                .unwrap_or_default();
            otlp_logs_columns(&attrs)
        }
    };
    if let Some(list) = config.get(COLUMNS_KEY) {
        for entry in list.split(';').filter(|e| !e.trim().is_empty()) {
            match ColumnSpec::parse(entry) {
                Ok(spec) => match cols.iter_mut().find(|c| c.name == spec.name) {
                    Some(existing) => *existing = spec,
                    None => cols.push(spec),
                },
                Err(e) => problem = Some(format!("{COLUMNS_KEY}: {e}")),
            }
        }
    }
    (cols, problem)
}

/// Order-independent fingerprint of a column set (names, types, paths), so a
/// change to any of them is noticed by the SQL layer.
pub fn fingerprint(cols: &[ColumnSpec]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut total: u64 = 0;
    for c in cols {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        c.name.hash(&mut h);
        c.ty.hash(&mut h);
        c.paths.hash(&mut h);
        total = total.wrapping_add(h.finish());
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// The message chronik-signal writes (signal-stream `default-logs`).
    fn signal_log() -> Value {
        json!({
            "attributes": {"request_id": "req-1", "http.status_code": 503},
            "body": "smoke vu=3 iter=0 i=8",
            "observed_timestamp": "2026-07-07T03:09:27.978+00:00",
            "resource": {"service.name": "k6-smoke"},
            "scope": {"name": "k6", "version": ""},
            "severity": "INFO",
            "severity_number": 9,
            "severity_text": "INFO",
            "span_id": null,
            "timestamp": "2026-07-07T03:09:27.978+00:00",
            "trace_id": null
        })
    }

    fn get(cols: &[ColumnSpec], name: &str, msg: &Value) -> Option<Projected> {
        cols.iter().find(|c| c.name == name).unwrap().extract(msg)
    }

    #[test]
    fn the_otlp_preset_reads_a_signal_log_record() {
        let cols = otlp_logs_columns(&["request_id".into(), "http.status_code".into()]);
        let m = signal_log();
        assert_eq!(get(&cols, "severity", &m), Some(Projected::Str("INFO".into())));
        assert_eq!(get(&cols, "severity_number", &m), Some(Projected::Int(9)));
        assert_eq!(get(&cols, "service_name", &m), Some(Projected::Str("k6-smoke".into())));
        assert_eq!(get(&cols, "body", &m), Some(Projected::Str("smoke vu=3 iter=0 i=8".into())));
        assert_eq!(get(&cols, "trace_id", &m), None, "null is NULL");
        assert_eq!(get(&cols, "scope_name", &m), Some(Projected::Str("k6".into())));
        assert_eq!(get(&cols, "attr_request_id", &m), Some(Projected::Str("req-1".into())));
        assert_eq!(get(&cols, "attr_http_status_code", &m), Some(Projected::Str("503".into())));
        assert_eq!(
            get(&cols, "timestamp", &m),
            Some(Projected::TimestampNanos(1_783_393_767_978_000_000))
        );
    }

    #[test]
    fn the_otlp_preset_reads_an_otlp_json_log_record() {
        let cols = otlp_logs_columns(&[]);
        let m = json!({
            "timeUnixNano": "1783393767978123456",
            "severityText": "ERROR",
            "severityNumber": 17,
            "body": {"stringValue": "boom"},
            "traceId": "5b8efff798038103d269b633813fc60c"
        });
        assert_eq!(get(&cols, "timestamp", &m), Some(Projected::TimestampNanos(1_783_393_767_978_123_456)));
        assert_eq!(get(&cols, "severity", &m), Some(Projected::Str("ERROR".into())));
        assert_eq!(get(&cols, "severity_number", &m), Some(Projected::Int(17)));
        assert_eq!(get(&cols, "body", &m), Some(Projected::Str("boom".into())));
        assert_eq!(get(&cols, "trace_id", &m), Some(Projected::Str("5b8efff798038103d269b633813fc60c".into())));
    }

    #[test]
    fn declared_columns_parse_with_types_pointers_and_alternatives() {
        let cols = parse_columns(
            "svc:string:/resource/service.name; lat:float64:/latency_ms|/duration ;ok:boolean;level",
        )
        .unwrap();
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[0].paths, vec![vec!["resource".to_string(), "service.name".to_string()]]);
        assert_eq!(cols[1].ty, ColumnType::Float64);
        assert_eq!(cols[1].paths.len(), 2);
        assert_eq!(cols[2].paths, vec![vec!["ok".to_string()]]);
        assert_eq!(cols[3].ty, ColumnType::String);
        let m = json!({"duration": "12.5", "ok": "true", "level": 3});
        assert_eq!(cols[1].extract(&m), Some(Projected::Float(12.5)));
        assert_eq!(cols[2].extract(&m), Some(Projected::Bool(true)));
        assert_eq!(cols[3].extract(&m), Some(Projected::Str("3".into())));
    }

    #[test]
    fn a_pointer_escapes_slashes_and_tildes_and_indexes_arrays() {
        let c = ColumnSpec::parse("x:int64:/a~1b/c~0d/1").unwrap();
        assert_eq!(c.extract(&json!({"a/b": {"c~d": [5, 7]}})), Some(Projected::Int(7)));
        assert_eq!(c.paths_literal(), "/a~1b/c~0d/1");
    }

    #[test]
    fn a_path_may_follow_the_name_directly_and_may_contain_colons() {
        let c = ColumnSpec::parse("svc:/resource/service.name").unwrap();
        assert_eq!((c.ty, c.paths.len()), (ColumnType::String, 1));
        let c = ColumnSpec::parse("st:int64:/http:status|/status").unwrap();
        assert_eq!(c.paths[0], vec!["http:status".to_string()]);
        assert_eq!(c.extract(&json!({"http:status": "503"})), Some(Projected::Int(503)));
    }

    #[test]
    fn bad_declarations_are_refused_with_the_reason() {
        assert!(parse_columns("x:int64:status").unwrap_err().contains("JSON pointer"));
        assert!(parse_columns("x:/a|b").unwrap_err().contains("JSON pointer"));
        assert!(parse_columns("_offset:int64").unwrap_err().contains("_offset"));
        assert!(parse_columns("x:decimal").unwrap_err().contains("decimal"));
        assert!(parse_columns("1x").is_err());
        assert!(validate_config(&cfg(&[(COLUMNS_KEY, "a;a:int64")])).unwrap_err().contains("twice"));
        assert!(validate_config(&cfg(&[(PROJECTION_KEY, "otlp")])).is_err());
    }

    #[test]
    fn conversions_follow_the_documented_rules() {
        let int = ColumnSpec::parse("v:int64").unwrap();
        assert_eq!(int.extract(&json!({"v": 4.0})), Some(Projected::Int(4)));
        assert_eq!(int.extract(&json!({"v": 4.5})), None);
        assert_eq!(int.extract(&json!({"v": "42"})), Some(Projected::Int(42)));
        assert_eq!(int.extract(&json!({"v": true})), None);
        let ts = ColumnSpec::parse("v:timestamp").unwrap();
        assert_eq!(ts.extract(&json!({"v": 1_700_000_000})), Some(Projected::TimestampNanos(1_700_000_000_000_000_000)));
        assert_eq!(ts.extract(&json!({"v": 1_700_000_000_123i64})), Some(Projected::TimestampNanos(1_700_000_000_123_000_000)));
        assert_eq!(ts.extract(&json!({"v": "2026-07-07 03:09:27.5"})), Some(Projected::TimestampNanos(1_783_393_767_500_000_000)));
        assert_eq!(ts.extract(&json!({"v": "yesterday"})), None);
        let js = ColumnSpec::parse("v:json").unwrap();
        assert_eq!(js.extract(&json!({"v": {"a": 1}})), Some(Projected::Str("{\"a\":1}".into())));
        let s = ColumnSpec::parse("v").unwrap();
        assert_eq!(s.extract(&json!({"v": [1, 2]})), Some(Projected::Str("[1,2]".into())));
    }

    #[test]
    fn effective_columns_layer_declared_over_the_mode() {
        let auto = cfg(&[(INFERRED_KEY, "sev:string,n:int64?"), (COLUMNS_KEY, "n:float64;svc:/r/s")]);
        let (cols, problem) = effective_columns(&auto);
        assert!(problem.is_none());
        let names: Vec<_> = cols.iter().map(|c| (c.name.as_str(), c.ty)).collect();
        assert_eq!(names, vec![("sev", ColumnType::String), ("n", ColumnType::Float64), ("svc", ColumnType::String)]);

        let none = cfg(&[(PROJECTION_KEY, "none"), (INFERRED_KEY, "sev:string")]);
        assert!(effective_columns(&none).0.is_empty());

        let otlp = cfg(&[(PROJECTION_KEY, "otlp-logs"), (OTLP_ATTRIBUTES_KEY, "user.id"), (COLUMNS_KEY, "region:/resource/cloud.region")]);
        let (cols, _) = effective_columns(&otlp);
        assert!(cols.iter().any(|c| c.name == "attr_user_id"));
        assert_eq!(cols.last().unwrap().name, "region");

        let typo = cfg(&[(COLUMNS_KEY, "ok;bad:nope")]);
        let (cols, problem) = effective_columns(&typo);
        assert_eq!(cols.len(), 1);
        assert!(problem.unwrap().contains("nope"));
    }

    #[test]
    fn the_fingerprint_moves_with_type_and_path_not_order() {
        let a = parse_columns("x:int64:/a;y").unwrap();
        let b = parse_columns("y;x:int64:/a").unwrap();
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&parse_columns("x:float64:/a;y").unwrap()));
        assert_ne!(fingerprint(&a), fingerprint(&parse_columns("x:int64:/b;y").unwrap()));
    }
}
