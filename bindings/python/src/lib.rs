//! Python bindings for `chronik-memory`.
//!
//! This is a thin PyO3 wrapper around the Rust SDK. Goals:
//! - Mirror the Rust API's shape (builder → `Memory` → method calls).
//! - All I/O methods are *Python awaitables* — `await mem.recall(...)`
//!   works inside `asyncio` because we route the underlying Rust futures
//!   through `pyo3_async_runtimes::tokio`.
//! - Records returned to Python are plain `dict`s (JSON-shaped), not
//!   typed classes. The optional `chronik_memory.types` Pydantic layer
//!   ships separately and is the recommended way to get typed access on
//!   the Python side.
//!
//! Out of scope for this initial wheel:
//! - `ingest_with_extraction(...)` (needs an `Extractor` configured at
//!   build time — exposing extractors through PyO3 deserves its own pass).
//! - `Worker` (the standalone worker is a Rust binary; Python users run
//!   it as a sidecar — see `docs/PROVIDER_COMPARISON.md` (planned)).
//! - `remember`/`forget` typed bodies — pending the typed-class layer.
//!
//! These land in follow-up commits, tracked under AMS-3.1 in the SDK
//! roadmap. The current surface is enough to demonstrate that the
//! binding works end-to-end and to run the Rust ⇄ Python eval-parity
//! comparison.

// We can't `use chronik_memory::...` here because the `#[pymodule]` macro
// at the bottom of this file declares a `fn chronik_memory(...)` whose
// name shadows the crate. Reach for the crate via the absolute path.
use ::chronik_memory::extractor::Turn;
use ::chronik_memory::{Memory, MemoryError, MemoryType};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

/// Python-facing wrapper around `chronik_memory::Memory`.
///
/// Constructed via `await PyMemory.build(kafka, api, namespace, ...)`
/// rather than `__init__`, because the underlying builder requires a
/// running Tokio runtime to create the rdkafka producer + admin client.
#[pyclass(name = "Memory", module = "chronik_memory")]
#[derive(Clone)]
struct PyMemory {
    inner: Memory,
}

#[pymethods]
impl PyMemory {
    /// Build a `Memory` against a running Chronik cluster.
    ///
    /// ```python
    /// import asyncio
    /// from chronik_memory import Memory
    ///
    /// mem = asyncio.run(Memory.build(
    ///     kafka="localhost:9092",
    ///     api="http://localhost:6092",
    ///     namespace="acme:agent:bot:user:luis",
    /// ))
    /// ```
    #[staticmethod]
    #[pyo3(signature = (kafka, api, namespace, request_timeout_secs=None))]
    fn build<'py>(
        py: Python<'py>,
        kafka: String,
        api: String,
        namespace: String,
        request_timeout_secs: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut builder = Memory::builder()
                .chronik_kafka(kafka)
                .chronik_api(api)
                .namespace(namespace);
            if let Some(secs) = request_timeout_secs {
                builder = builder.request_timeout(std::time::Duration::from_secs(secs));
            }
            let inner = builder.build().await.map_err(memory_err)?;
            Ok(PyMemory { inner })
        })
    }

    /// Idempotent — creates the raw + typed topics for this namespace if
    /// they don't already exist. Safe to call on every cold start.
    fn init_namespace<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            inner.init_namespace().await.map_err(memory_err)?;
            Ok(())
        })
    }

    /// Initialize the *full* topic set (raw + 4 typed topics + task-current
    /// view). Use when you know you'll be writing instructions / tasks.
    fn init_namespace_full<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            inner.init_namespace_full().await.map_err(memory_err)?;
            Ok(())
        })
    }

    /// Ingest one raw conversation turn.
    ///
    /// `external_id` (when set) is used as the Kafka record key on the
    /// raw topic — this is the recommended way to get cross-process
    /// idempotency for upstream messaging IDs (WhatsApp, Slack, etc.).
    #[pyo3(signature = (role, content, *, external_id=None, channel=None))]
    fn ingest_turn<'py>(
        &self,
        py: Python<'py>,
        role: String,
        content: String,
        external_id: Option<String>,
        channel: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let turn = Turn {
                role,
                content,
                ts: None,
                channel,
                external_id,
            };
            let ack = inner.ingest_turn(turn).await.map_err(memory_err)?;
            Python::with_gil(|py| {
                let dict = PyDict::new_bound(py);
                dict.set_item("topic", ack.topic)?;
                dict.set_item("partition", ack.partition)?;
                dict.set_item("offset", ack.offset)?;
                dict.set_item("deduped", ack.deduped)?;
                Ok(dict.unbind())
            })
        })
    }

    /// Hybrid recall against the namespace's typed memory topics.
    ///
    /// Returns a `list[dict]`, one entry per result. Each dict has
    /// `score: float`, `memory: dict` (the envelope), `channels:
    /// dict[str, float]` (per-channel RRF contribution).
    ///
    /// `types` filters to a subset of memory types: `["fact"]`,
    /// `["fact", "event"]`, etc. Defaults to fact + event.
    /// `channels` is a list of channel names — currently `["bm25"]` is
    /// the only universally-available option; vector / hyde / sql remain
    /// Rust-only until the typed-class layer lands.
    #[pyo3(signature = (query, *, k=10, types=None))]
    fn recall<'py>(
        &self,
        py: Python<'py>,
        query: String,
        k: usize,
        types: Option<Vec<String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let parsed_types = match types {
            Some(ts) => parse_types(&ts)?,
            None => vec![MemoryType::Fact, MemoryType::Event],
        };
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let results = inner
                .recall(&query)
                .types(&parsed_types)
                .k(k)
                .send()
                .await
                .map_err(memory_err)?;
            // Convert each result via JSON round-trip — keeps Python side
            // free of any chrono / serde-json typing concerns.
            let serializable: Vec<serde_json::Value> = results
                .into_iter()
                .map(|r| {
                    let channels: serde_json::Map<String, serde_json::Value> = r
                        .channels
                        .into_iter()
                        .map(|(c, v)| (format!("{c:?}").to_lowercase(), serde_json::json!(v)))
                        .collect();
                    serde_json::json!({
                        "score": r.score,
                        "memory": serde_json::to_value(&r.memory).unwrap_or(serde_json::Value::Null),
                        "channels": channels,
                    })
                })
                .collect();
            Python::with_gil(|py| json_array_to_pylist(py, &serializable))
        })
    }

    /// Read-only accessor for the namespace this `Memory` was built with.
    #[getter]
    fn namespace(&self) -> &str {
        self.inner.namespace()
    }

    /// Read-only accessor for the unified-API URL this `Memory` is
    /// pointed at.
    #[getter]
    fn chronik_api(&self) -> &str {
        self.inner.chronik_api()
    }

    fn __repr__(&self) -> String {
        format!(
            "Memory(namespace={:?}, chronik_api={:?})",
            self.inner.namespace(),
            self.inner.chronik_api()
        )
    }
}

fn parse_types(ts: &[String]) -> PyResult<Vec<MemoryType>> {
    let mut out = Vec::with_capacity(ts.len());
    for t in ts {
        let parsed = match t.as_str() {
            "fact" => MemoryType::Fact,
            "event" => MemoryType::Event,
            "instruction" => MemoryType::Instruction,
            "task" => MemoryType::Task,
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown memory type {other:?}; expected one of fact|event|instruction|task"
                )));
            }
        };
        out.push(parsed);
    }
    if out.is_empty() {
        return Err(PyValueError::new_err(
            "types=[] is not allowed; pass at least one of fact|event|instruction|task",
        ));
    }
    Ok(out)
}

fn memory_err(e: MemoryError) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Convert a `Vec<serde_json::Value>` into a Python `list[dict]` without
/// pulling in `pythonize`. Only handles the JSON shapes we actually
/// produce on the recall path (object / array / string / number / bool /
/// null) — recursive.
fn json_array_to_pylist<'py>(
    py: Python<'py>,
    arr: &[serde_json::Value],
) -> PyResult<Py<PyAny>> {
    let list = PyList::empty_bound(py);
    for v in arr {
        list.append(json_to_py(py, v)?)?;
    }
    Ok(list.unbind().into_any())
}

fn json_to_py<'py>(py: Python<'py>, v: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use serde_json::Value;
    Ok(match v {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_py(py),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_py(py)
            } else if let Some(u) = n.as_u64() {
                u.into_py(py)
            } else if let Some(f) = n.as_f64() {
                f.into_py(py)
            } else {
                py.None()
            }
        }
        Value::String(s) => s.into_py(py),
        Value::Array(arr) => {
            let list = PyList::empty_bound(py);
            for item in arr {
                list.append(json_to_py(py, item)?)?;
            }
            list.unbind().into_any()
        }
        Value::Object(map) => {
            let dict = PyDict::new_bound(py);
            for (k, val) in map {
                dict.set_item(k, json_to_py(py, val)?)?;
            }
            dict.unbind().into_any()
        }
    })
}

#[pymodule]
fn chronik_memory(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMemory>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
