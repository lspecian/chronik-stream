//! Ontology headline metric (roadmap §8): **agent task success via the ontology
//! MCP tools vs. a baseline agent using raw `/_search`**, on multi-hop tasks.
//!
//! This is the product proof — does the semantic surface make the agent *better*,
//! not just prettier. Both arms use the SAME LLM (a local Modena model) and the
//! SAME backing facts; the only difference is the tools:
//!   - **ontology arm**: the 6 fixed MCP tools (`get_object`, `traverse`,
//!     `neighbors`, `explain`, `query_objects`, `list_types`) — one call answers
//!     a multi-hop / reverse / point-in-time question.
//!   - **baseline arm**: a single `search` tool over raw `/_search`, so the agent
//!     must find, read, and chain facts itself.
//! We score final-answer correctness per task and report both rates.
//!
//! ## Running (needs a live broker with ontology enabled + a local LLM)
//! ```bash
//! CHRONIK_INTEGRATION=1 \
//!   CHRONIK_API=http://localhost:6092 CHRONIK_KAFKA=localhost:9092 \
//!   AGENT_LLM_ENDPOINT=http://192.168.1.184:8080 \
//!   AGENT_LLM_MODEL=mlx-community/Qwen3-30B-A3B-4bit-DWQ \
//!   cargo test -p chronik-memory --test eval_ontology_agent -- --ignored --nocapture
//! ```
//! The broker must run with `CHRONIK_ONTOLOGY_ENABLED=true` (so the ObjectType
//! registry + edge index are live) and an embedding provider (v2.12 rejects the
//! `vector.enabled` fact topic otherwise).

use std::sync::Arc;
use std::time::Duration;

use chronik_memory::embeddings::TextGenerator;
use chronik_memory::OpenAIExtractor;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};

const NS: &str = "agenteval"; // colon-free -> tenant == namespace
const MAX_STEPS: usize = 8;

// ─────────────────────────── fixture ───────────────────────────────────────
//
// A small issue tracker. Facts are (subject, predicate, object) triples with
// bi-temporal validity; an edge is a triple whose object is another ticket.

fn ts(d: &str) -> String {
    format!("2026-01-{d}T00:00:00Z")
}

/// (subject, predicate, object, valid_from_day, valid_to_day)
fn fixture_facts() -> Vec<(&'static str, &'static str, &'static str, &'static str, Option<&'static str>)> {
    vec![
        // blocker chain: T3 -blocked_by-> T2 -blocked_by-> T1
        ("T2", "blocked_by", "T1", "01", None),
        ("T3", "blocked_by", "T2", "01", None),
        ("T5", "blocked_by", "T4", "01", None),
        // assignees
        ("T1", "assignee", "alice", "01", None),
        ("T2", "assignee", "bob", "01", None),
        ("T3", "assignee", "carol", "01", None),
        ("T4", "assignee", "dave", "01", None),
        ("T5", "assignee", "erin", "01", None),
        // projects
        ("T1", "project", "web", "01", None),
        ("T2", "project", "web", "01", None),
        ("T3", "project", "web", "01", None),
        ("T4", "project", "api", "01", None),
        ("T5", "project", "api", "01", None),
        // epic E1 owns T1, T2
        ("E1", "parent_of", "T1", "01", None),
        ("E1", "parent_of", "T2", "01", None),
        ("E1", "assignee", "alice", "01", None),
        // T1 status history (for as_of): open [01,05), in_progress [05,08), resolved [08,)
        ("T1", "status", "open", "01", Some("05")),
        ("T1", "status", "in_progress", "05", Some("08")),
        ("T1", "status", "resolved", "08", None),
    ]
}

fn object_type_json() -> String {
    // Ticket ObjectType: status/assignee/project attributes from predicates.
    r#"{"schema_version":1,"object_type":{"type_name":"Ticket","attributes":[
        {"name":"status","type":"string","from_predicate":"status"},
        {"name":"assignee","type":"string","from_predicate":"assignee"},
        {"name":"project","type":"string","from_predicate":"project"}],
        "identity":{"id_field":"subject","normalize":true},
        "backing":{"topic_prefix":"mem.fact","append_only":false}}}"#
        .replace(['\n', ' '], "")
}

struct Task {
    id: &'static str,
    question: &'static str,
    /// Every string here (case-insensitive) must appear in a correct answer.
    gold: &'static [&'static str],
}

fn tasks() -> Vec<Task> {
    vec![
        Task { id: "multi_hop_blockers", question: "Which tickets transitively block ticket T3 (follow blocked_by)?", gold: &["T1", "T2"] },
        Task { id: "hop_plus_attribute", question: "Who is assigned to the ticket that directly blocks ticket T5?", gold: &["dave"] },
        Task { id: "reverse_edge", question: "Which tickets are directly blocked by ticket T1?", gold: &["T2"] },
        Task { id: "parent_subtasks", question: "List the subtasks (children) of epic E1.", gold: &["T1", "T2"] },
        Task { id: "point_in_time", question: "What was ticket T1's status on 2026-01-06?", gold: &["in_progress"] },
    ]
}

// ─────────────────────────── ingest ────────────────────────────────────────

async fn init_namespace(client: &reqwest::Client, api: &str) {
    let _ = client
        .post(format!("{api}/memory/v1/admin/init-namespace"))
        .json(&serde_json::json!({"tenant": NS, "agent": "a1"}))
        .send()
        .await;
}

fn fact_value(subject: &str, predicate: &str, object: &str, vf: &str, vt: Option<&str>, off: i64) -> String {
    let mut v = serde_json::json!({
        "namespace": NS,
        "key": format!("{subject}|{predicate}"),
        "valid_from": ts(vf),
        "confidence": 1.0,
        "source": {"topic": format!("mem.raw.{NS}"), "offsets": [off], "extractor": "fixture@1"},
        "type": "fact",
        "body": {"subject": subject, "predicate": predicate, "object": object,
                 "text": format!("{subject} {predicate} {object}")},
    });
    if let Some(vt) = vt {
        v["valid_to"] = serde_json::json!(ts(vt));
    }
    v.to_string()
}

async fn ingest(kafka: &str, api: &str, client: &reqwest::Client) {
    init_namespace(client, api).await;
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", kafka)
        .create()
        .expect("producer");

    // Register the Ticket ObjectType.
    let ot = object_type_json();
    let _ = producer
        .send(
            FutureRecord::to(&format!("ont.types.{NS}")).key("Ticket").payload(&ot),
            Duration::from_secs(5),
        )
        .await;

    // Produce the facts.
    for (i, (s, p, o, vf, vt)) in fixture_facts().into_iter().enumerate() {
        let key = format!("{s}|{p}|{o}"); // unique per fact (append-only fixture)
        let val = fact_value(s, p, o, vf, vt, i as i64);
        let _ = producer
            .send(
                FutureRecord::to(&format!("mem.fact.{NS}")).key(&key).payload(&val),
                Duration::from_secs(5),
            )
            .await;
    }

    // Wait until the facts are searchable.
    for _ in 0..30 {
        let n = match client
            .post(format!("{api}/_search"))
            .json(&serde_json::json!({"index": format!("mem.fact.{NS}"), "size": 30, "query": {"match": {"_all": "blocked_by"}}}))
            .send()
            .await
        {
            Ok(r) => {
                let body: serde_json::Value = r.json().await.unwrap_or(serde_json::json!({}));
                body.pointer("/hits/total/value").and_then(|v| v.as_u64()).unwrap_or(0)
            }
            Err(_) => 0,
        };
        if n >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // CRITICAL: wait for the ontology CONSUMERS to catch up before running tasks
    // — the ObjectType registry (ont.types) and the edge index (mem.fact). Without
    // this the agent races them and get_object/neighbors return empty (the tools
    // look broken when they're just not hydrated yet).
    for _ in 0..40 {
        let go_ok = client
            .post(format!("{api}/ontology/v1/get_object"))
            .json(&serde_json::json!({"namespace": NS, "type": "Ticket", "id": "T1"}))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        let nb_count = match client
            .post(format!("{api}/ontology/v1/neighbors"))
            .json(&serde_json::json!({"namespace": NS, "node": "T1", "direction": "incoming", "edge_type": "blocked_by"}))
            .send()
            .await
        {
            Ok(r) => {
                let b: serde_json::Value = r.json().await.unwrap_or_default();
                b.get("count").and_then(|c| c.as_u64()).unwrap_or(0)
            }
            Err(_) => 0,
        };
        if go_ok && nb_count >= 1 {
            eprintln!("ontology consumers hydrated (registry + edge index)");
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ─────────────────────────── agent loop ────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Surface {
    Ontology,
    Baseline,
}

fn tool_docs(surface: Surface) -> &'static str {
    match surface {
        Surface::Ontology => {
            "get_object{type,id,as_of?} -> an object's attributes with provenance; pass as_of (RFC3339, e.g. 2026-01-06T00:00:00Z) for its state at a past time.\n\
             traverse{from,edge_type,depth} -> follow OUTGOING links from a node, up to `depth` hops (edge_type='*' for all).\n\
             neighbors{node,direction,edge_type,depth} -> links in either direction; direction='incoming' lists what points AT node; depth for multi-hop.\n\
             explain{type,id} -> why an object holds its values, with its edges.\n\
             query_objects{type} -> list all instances of a type.\n\
             list_types{} -> the object types available.\n\
             Direction guide: a ticket's OWN blockers are its OUTGOING `blocked_by`. Tickets blocked BY a ticket are its INCOMING `blocked_by` (neighbors direction=incoming). An epic's subtasks are its OUTGOING `parent_of`."
        }
        Surface::Baseline => {
            "search{query} -> full-text search over the raw fact records; returns up to 10 matching facts as text. Chain searches to follow relationships yourself."
        }
    }
}

fn build_prompt(surface: Surface, question: &str, transcript: &str) -> String {
    let tools = tool_docs(surface);
    format!(
        "You are an agent answering a question about an issue tracker by calling tools.\n\
The namespace is provided automatically — do NOT include it.\n\
\n\
Domain schema (use these EXACT names):\n\
- Object type: `Ticket` (attributes: status, assignee, project). Tickets look like T1, T5; the epic node is E1.\n\
- Edge types: `blocked_by` (\"A blocked_by B\" means ticket A is blocked by ticket B), `parent_of` (\"E parent_of T\" means epic E owns subtask T).\n\
\n\
Tools:\n{tools}\n\
\n\
Protocol: reply with EXACTLY ONE JSON object and nothing else.\n\
- To call a tool: {{\"tool\":\"NAME\",\"args\":{{...}}}}\n\
- When you can answer: {{\"answer\":\"...\"}}\n\
Ticket ids look like T1, T5; the epic is E1. Keep answers short and list ids/names explicitly.\n\
\n\
Question: {question}\n\
{transcript}\n\
Your JSON:"
    )
}

/// Extract the first balanced JSON object from an LLM reply.
fn extract_json(s: &str) -> Option<serde_json::Value> {
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s[start..].char_indices() {
        match c {
            '"' if !esc => in_str = !in_str,
            '\\' if in_str => {
                esc = !esc;
                continue;
            }
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&s[start..start + i + 1]).ok();
                }
            }
            _ => {}
        }
        esc = false;
    }
    None
}

async fn execute_tool(
    client: &reqwest::Client,
    api: &str,
    surface: Surface,
    tool: &str,
    mut args: serde_json::Value,
) -> String {
    if let Some(obj) = args.as_object_mut() {
        obj.insert("namespace".to_string(), serde_json::json!(NS));
    }
    match surface {
        Surface::Baseline => {
            // Only `search` is honored.
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let resp = client
                .post(format!("{api}/_search"))
                .json(&serde_json::json!({"index": format!("mem.fact.{NS}"), "size": 10, "query": {"match": {"_all": query}}}))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let body: serde_json::Value = r.json().await.unwrap_or(serde_json::json!({}));
                    let hits = body.get("hits").and_then(|h| h.get("hits")).and_then(|h| h.as_array()).cloned().unwrap_or_default();
                    let texts: Vec<String> = hits
                        .iter()
                        .filter_map(|h| {
                            let src = h.get("_source")?;
                            // Tolerant envelope: direct shape, or wrapped as a JSON
                            // string under value/_value/_json_content.
                            let env = if src.get("body").is_some() || src.get("type").is_some() {
                                src.clone()
                            } else {
                                ["value", "_value", "_json_content"]
                                    .iter()
                                    .find_map(|f| {
                                        src.get(*f)
                                            .and_then(|v| v.as_str())
                                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                                    })
                                    .unwrap_or_else(|| src.clone())
                            };
                            let b = env.get("body").unwrap_or(&env);
                            let subj = b.get("subject").and_then(|v| v.as_str());
                            let pred = b.get("predicate").and_then(|v| v.as_str());
                            let obj = b.get("object").and_then(|v| v.as_str());
                            match (subj, pred, obj) {
                                (Some(s), Some(p), Some(o)) => Some(format!("{s} {p} {o} (valid_from {})", env.get("valid_from").and_then(|v| v.as_str()).unwrap_or("?"))),
                                _ => b.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()),
                            }
                        })
                        .collect();
                    if texts.is_empty() { "(no results)".into() } else { texts.join("; ") }
                }
                Err(e) => format!("(search error: {e})"),
            }
        }
        Surface::Ontology => {
            let resp = client
                .post(format!("{api}/ontology/v1/mcp"))
                .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":args}}))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let body: serde_json::Value = r.json().await.unwrap_or(serde_json::json!({}));
                    // Prefer structuredContent; fall back to content text or error.
                    if let Some(sc) = body.pointer("/result/structuredContent") {
                        serde_json::to_string(sc).unwrap_or_else(|_| "{}".into())
                    } else if let Some(t) = body.pointer("/result/content/0/text").and_then(|v| v.as_str()) {
                        t.to_string()
                    } else {
                        serde_json::to_string(&body).unwrap_or_else(|_| "{}".into())
                    }
                }
                Err(e) => format!("(mcp error: {e})"),
            }
        }
    }
}

async fn run_agent(
    client: &reqwest::Client,
    api: &str,
    gen: &Arc<dyn TextGenerator>,
    question: &str,
    surface: Surface,
) -> (String, usize) {
    let mut transcript = String::new();
    for step in 0..MAX_STEPS {
        let prompt = build_prompt(surface, question, &transcript);
        let raw = match gen.complete(&prompt).await {
            Ok(r) => r,
            Err(e) => return (format!("(llm error: {e})"), step),
        };
        let dbg = std::env::var("AGENT_DEBUG").is_ok();
        let Some(v) = extract_json(&raw) else {
            if dbg {
                eprintln!("    [step {step}] unparseable LLM reply: {:?}", raw.chars().take(160).collect::<String>());
            }
            transcript.push_str("\n(Your last reply was not valid JSON. Reply with ONE JSON object only.)\n");
            continue;
        };
        if let Some(ans) = v.get("answer").and_then(|a| a.as_str()) {
            return (ans.to_string(), step + 1);
        }
        if let (Some(tool), args) = (v.get("tool").and_then(|t| t.as_str()), v.get("args").cloned().unwrap_or(serde_json::json!({}))) {
            let result = execute_tool(client, api, surface, tool, args.clone()).await;
            let truncated: String = result.chars().take(800).collect();
            if dbg {
                eprintln!("    [step {step}] {tool}({args}) -> {}", truncated.chars().take(240).collect::<String>());
            }
            transcript.push_str(&format!("\nYou called {tool}({args}). Result: {truncated}\n"));
        } else {
            transcript.push_str("\n(No tool or answer in your reply. Call a tool or answer.)\n");
        }
    }
    ("(step limit reached without an answer)".to_string(), MAX_STEPS)
}

fn scores_hit(answer: &str, gold: &[&str]) -> bool {
    let a = answer.to_lowercase();
    gold.iter().all(|g| a.contains(&g.to_lowercase()))
}

#[tokio::test]
#[ignore = "requires CHRONIK_INTEGRATION=1 + a live broker (ontology enabled) + a local LLM"]
async fn ontology_agent_beats_baseline() {
    if std::env::var("CHRONIK_INTEGRATION").ok().as_deref() != Some("1") {
        eprintln!("skipping: set CHRONIK_INTEGRATION=1");
        return;
    }
    let api = std::env::var("CHRONIK_API").unwrap_or_else(|_| "http://localhost:6092".into());
    let kafka = std::env::var("CHRONIK_KAFKA").unwrap_or_else(|_| "localhost:9092".into());
    let endpoint = std::env::var("AGENT_LLM_ENDPOINT").unwrap_or_else(|_| "http://192.168.1.184:8080".into());
    let model = std::env::var("AGENT_LLM_MODEL").unwrap_or_else(|_| "mlx-community/Qwen3-30B-A3B-4bit-DWQ".into());

    let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).build().unwrap();
    if client.get(format!("{api}/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
        eprintln!("broker healthy at {api}");
    } else {
        eprintln!("skipping: broker not healthy at {api}");
        return;
    }

    println!("== ingesting fixture (issue tracker) into namespace {NS} ==");
    ingest(&kafka, &api, &client).await;

    let gen: Arc<dyn TextGenerator> = Arc::new(OpenAIExtractor::for_local_server(&endpoint, &model).with_max_tokens(512));
    println!("== running agent tasks (LLM: {model}) ==\n");

    let tasks = tasks();
    let mut onto_hits = 0usize;
    let mut base_hits = 0usize;
    for t in &tasks {
        let (onto_ans, onto_steps) = run_agent(&client, &api, &gen, t.question, Surface::Ontology).await;
        let onto_ok = scores_hit(&onto_ans, t.gold);
        onto_hits += onto_ok as usize;

        let (base_ans, base_steps) = run_agent(&client, &api, &gen, t.question, Surface::Baseline).await;
        let base_ok = scores_hit(&base_ans, t.gold);
        base_hits += base_ok as usize;

        println!("[{}] gold={:?}", t.id, t.gold);
        println!("  ontology ({onto_steps} steps): {} — {:?}", if onto_ok { "HIT " } else { "miss" }, onto_ans.chars().take(120).collect::<String>());
        println!("  baseline ({base_steps} steps): {} — {:?}\n", if base_ok { "HIT " } else { "miss" }, base_ans.chars().take(120).collect::<String>());
    }

    let n = tasks.len();
    println!("== RESULT ==");
    println!("  ontology  agent success: {onto_hits}/{n} = {:.2}", onto_hits as f64 / n as f64);
    println!("  baseline  agent success: {base_hits}/{n} = {:.2}", base_hits as f64 / n as f64);
    println!("  (roadmap O-2 exit: ontology must beat baseline on the multi-hop task fixture)");

    // Pipeline sanity only (not a quality gate): at least one arm produced an
    // answer. The headline comparison is the printed numbers.
    assert!(onto_hits + base_hits > 0, "both arms failed to answer any task — pipeline broken");
}
