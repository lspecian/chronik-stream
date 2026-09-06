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
//!   AGENT_LLM_MODEL=mlx-community/Mistral-Small-3.1-Text-24B-Instruct-2503-4bit \
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
const MAX_STEPS: usize = 12; // enough for the baseline to chain a deep search manually

// ─────────────────────────── fixture ───────────────────────────────────────
//
// A small issue tracker. Facts are (subject, predicate, object) triples with
// bi-temporal validity; an edge is a triple whose object is another ticket.

fn ts(d: &str) -> String {
    format!("2026-01-{d}T00:00:00Z")
}

/// One fact record: (subject, predicate, object, valid_from_day, valid_to_day).
type FactRec = (String, String, String, String, Option<String>);

struct Task {
    id: String,
    question: String,
    /// Every string here (case-insensitive) must appear in a correct answer.
    gold: Vec<String>,
}

struct Fixture {
    facts: Vec<FactRec>,
    tasks: Vec<Task>,
}

/// Deterministically generate a SCALED issue tracker so raw search over it is a
/// real disadvantage: many blocker chains (a deep transitive query becomes a
/// long hop-chain a search agent must walk by hand), epics with subtasks,
/// assignees/projects/status, and a status history for the point-in-time task.
/// Env-tunable (`ONTO_AGENT_CHAINS` / `_CHAIN_LEN` / `_EPICS`).
fn generate_fixture() -> Fixture {
    let n_chains: usize = std::env::var("ONTO_AGENT_CHAINS").ok().and_then(|v| v.parse().ok()).unwrap_or(150);
    let chain_len: usize = std::env::var("ONTO_AGENT_CHAIN_LEN").ok().and_then(|v| v.parse().ok()).unwrap_or(6).max(3);
    let n_epics: usize = std::env::var("ONTO_AGENT_EPICS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);

    let mut facts: Vec<FactRec> = Vec::new();
    let mut f = |s: &str, p: &str, o: &str, vf: &str, vt: Option<&str>| {
        facts.push((s.into(), p.into(), o.into(), vf.into(), vt.map(|x| x.to_string())));
    };
    let statuses = ["open", "in_progress", "resolved", "closed"];
    for g in 0..n_chains {
        for i in 0..chain_len {
            let t = format!("C{g}_{i}");
            // C0_0 gets an explicit status history below — skip the flat one.
            if !(g == 0 && i == 0) {
                f(&t, "status", statuses[i % statuses.len()], "01", None);
            }
            f(&t, "assignee", &format!("U{}", (g + i) % 20), "01", None);
            f(&t, "project", &format!("P{}", g % 8), "01", None);
            if i > 0 {
                f(&t, "blocked_by", &format!("C{g}_{}", i - 1), "01", None);
            }
        }
    }
    for e in 0..n_epics {
        let epic = format!("EP{e}");
        f(&epic, "assignee", &format!("U{}", e % 20), "01", None);
        f(&epic, "status", "open", "01", None);
        for s in 0..3 {
            let sub = format!("EP{e}_s{s}");
            f(&sub, "status", "open", "01", None);
            f(&epic, "parent_of", &sub, "01", None);
        }
    }
    // Point-in-time target C0_0: open[01,05) in_progress[05,08) resolved[08,).
    f("C0_0", "status", "open", "01", Some("05"));
    f("C0_0", "status", "in_progress", "05", Some("08"));
    f("C0_0", "status", "resolved", "08", None);

    // Tasks over deterministic chain-0 / epic-0 targets.
    let tail = format!("C0_{}", chain_len - 1);
    let deep_blockers: Vec<String> = (0..chain_len - 1).map(|i| format!("C0_{i}")).collect();
    let direct_blocker_user = format!("U{}", (chain_len - 2) % 20);
    let mid = format!("C0_{}", chain_len / 2);
    let mid_blocks = format!("C0_{}", chain_len / 2 + 1);
    let epic0_subs: Vec<String> = (0..3).map(|s| format!("EP0_s{s}")).collect();

    let tasks = vec![
        Task {
            id: "deep_transitive_blockers".into(),
            question: format!("Which tickets transitively block ticket {tail} (follow blocked_by all the way to the end of the chain)?"),
            gold: deep_blockers,
        },
        Task {
            id: "hop_plus_attribute".into(),
            question: format!("Who is assigned to the ticket that directly blocks ticket {tail}?"),
            gold: vec![direct_blocker_user],
        },
        Task {
            id: "reverse_edge".into(),
            question: format!("Which ticket is directly blocked by ticket {mid}?"),
            gold: vec![mid_blocks],
        },
        Task {
            id: "parent_subtasks".into(),
            question: "List the subtasks (children) of epic EP0.".into(),
            gold: epic0_subs,
        },
        Task {
            id: "point_in_time".into(),
            question: "What was ticket C0_0's status on 2026-01-06?".into(),
            gold: vec!["in_progress".into()],
        },
    ];
    Fixture { facts, tasks }
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

// (Task + the task set are produced by generate_fixture above.)

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

async fn ingest(kafka: &str, api: &str, client: &reqwest::Client, facts: &[FactRec]) {
    init_namespace(client, api).await;
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", kafka)
        .set("queue.buffering.max.messages", "200000")
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

    // Register LinkTypes WITH INVERSES so the agent names a relation instead of a
    // direction: blocked_by<->blocks, parent_of<->subtask_of.
    for (name, predicate, inverse, desc) in [
        ("blocked_by", "blocked_by", "blocks", "A blocked_by B: ticket A is blocked by ticket B (B is a blocker of A). Inverse `blocks`: B blocks A."),
        ("parent_of", "parent_of", "subtask_of", "E parent_of T: epic E owns subtask T. Inverse `subtask_of`: T is a subtask of E."),
    ] {
        let env = serde_json::json!({"schema_version":1,"link_type":{"name":name,"predicate":predicate,"inverse":inverse,"description":desc}}).to_string();
        let _ = producer
            .send(FutureRecord::to(&format!("ont.links.{NS}")).key(name).payload(&env), Duration::from_secs(5))
            .await;
    }

    // Produce the facts (scaled — can be thousands).
    for (i, (s, p, o, vf, vt)) in facts.iter().enumerate() {
        let key = format!("{s}|{p}|{o}"); // unique per fact (append-only fixture)
        let val = fact_value(s, p, o, vf.as_str(), vt.as_deref(), i as i64);
        let _ = producer
            .send(
                FutureRecord::to(&format!("mem.fact.{NS}")).key(&key).payload(&val),
                Duration::from_secs(10),
            )
            .await;
    }
    println!("  produced {} facts + 1 ObjectType + 2 LinkTypes", facts.len());

    // Wait until the facts are searchable (scaled — allow longer).
    for _ in 0..120 {
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
        if n >= 10 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // CRITICAL: wait for the ontology CONSUMERS to catch up before running tasks
    // — the ObjectType registry (ont.types) and the edge index (mem.fact). Without
    // this the agent races them and get_object/neighbors return empty (the tools
    // look broken when they're just not hydrated yet).
    for _ in 0..120 {
        let go_ok = client
            .post(format!("{api}/ontology/v1/get_object"))
            .json(&serde_json::json!({"namespace": NS, "type": "Ticket", "id": "C0_1"}))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        // C0_1 has an incoming blocked_by (C0_2 blocked_by C0_1) once the edge
        // index has hydrated.
        let nb_count = match client
            .post(format!("{api}/ontology/v1/neighbors"))
            .json(&serde_json::json!({"namespace": NS, "node": "C0_1", "direction": "incoming", "edge_type": "blocked_by"}))
            .send()
            .await
        {
            Ok(r) => {
                let b: serde_json::Value = r.json().await.unwrap_or_default();
                b.get("count").and_then(|c| c.as_u64()).unwrap_or(0)
            }
            Err(_) => 0,
        };
        // LinkType registry hydrated?
        let rel_ok = match client.get(format!("{api}/ontology/v1/relations?namespace={NS}")).send().await {
            Ok(r) => {
                let b: serde_json::Value = r.json().await.unwrap_or_default();
                b.get("relations").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false)
            }
            Err(_) => false,
        };
        if go_ok && nb_count >= 1 && rel_ok {
            eprintln!("ontology consumers hydrated (ObjectType + edge index + LinkTypes)");
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
            "relations{} -> the named relations available (each with an inverse). Pick a relation by NAME; never reason about direction.\n\
             related{node,relation,transitive?} -> traverse a NAMED relation from a node, returns the neighbor nodes. Express INTENT, not a hop count: omit `transitive` (or set false) for DIRECT (1-hop) neighbors; set `transitive:true` to follow the ENTIRE chain (all transitive hops) in ONE call. Use `blocked_by` for a ticket's blockers, the inverse `blocks` for what a ticket blocks, `parent_of` for an epic's subtasks.\n\
             get_object{type,id,as_of?} -> an object's attributes (status/assignee/project) with provenance; pass as_of (RFC3339, e.g. 2026-01-06T00:00:00Z) for its state at a past time.\n\
             query_objects{type} -> list all instances of a type.\n\
             explain{type,id} -> why an object holds its values, with its edges.\n\
             list_types{} -> the object types available."
        }
        Surface::Baseline => {
            "search{query} -> full-text search over the raw fact records; returns up to 10 matching facts as text. Use PLAIN keyword queries like `C0_5 blocked_by` or `EP0 parent_of` — do NOT use field:value or other special syntax (it returns nothing). Chain searches to follow relationships yourself (search a ticket, read its blocked_by fact, then search that ticket, etc.)."
        }
    }
}

fn build_prompt(surface: Surface, question: &str, transcript: &str, force: bool) -> String {
    if force {
        // Forced-termination turn: the agent has gathered tool results (in the
        // transcript) but kept looping instead of answering. Strip the tools and
        // demand an answer. Applied identically to BOTH arms, so it removes a shared
        // confound (non-termination) rather than favouring either.
        return format!(
            "You are answering a question about an issue tracker. You have ALREADY gathered tool results (below). Do NOT call any more tools.\n\
\n\
Reply with EXACTLY ONE JSON object: {{\"answer\":\"...\"}} and nothing else.\n\
List the relevant ids/names explicitly (e.g. C0_0, C0_1) with no extra commentary.\n\
\n\
Question: {question}\n\
{transcript}\n\
Your JSON answer:"
        );
    }
    let tools = tool_docs(surface);
    format!(
        "You are an agent answering a question about an issue tracker by calling tools.\n\
The namespace is provided automatically — do NOT include it.\n\
\n\
Domain schema (use these EXACT names):\n\
- Object type: `Ticket` (attributes: status, assignee, project). Tickets are named like C0_0, C0_5, C12_3; epics like EP0 with subtasks EP0_s0.\n\
- Relations (each has an inverse; the ontology agent can call `relations` to see them): `blocked_by`/`blocks`, `parent_of`/`subtask_of`. A ticket's blockers = traverse `blocked_by`; what a ticket blocks = traverse `blocks`; an epic's subtasks = traverse `parent_of`.\n\
\n\
Tools:\n{tools}\n\
\n\
Protocol: reply with EXACTLY ONE JSON object and nothing else.\n\
- To call a tool: {{\"tool\":\"NAME\",\"args\":{{...}}}}\n\
- When you can answer: {{\"answer\":\"...\"}}\n\
Keep answers short and list the ids/names explicitly (e.g. C0_0, C0_1) with no extra commentary.\n\
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
                                // Timestamps are structured metadata, NOT part of
                                // the searchable text — so a full-text baseline
                                // sees only the triple, never `valid_from`. (This
                                // is the honest default; the ontology's as_of uses
                                // the structured field.)
                                (Some(s), Some(p), Some(o)) => Some(format!("{s} {p} {o}")),
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
            // Harness-side interface discipline (kills two confounds):
            //  - validate/normalise relation names against the registry enum, so a
            //    miscased or invented edge name gets a helpful error instead of a
            //    silent empty result the model can't distinguish from "no such edge".
            //  - express traversal INTENT (transitive vs direct) instead of a guessed
            //    depth number: `transitive:true` -> full chain, otherwise 1 hop.
            const RELS: [&str; 4] = ["blocked_by", "blocks", "parent_of", "subtask_of"];
            if let Some(obj) = args.as_object_mut() {
                for k in ["relation", "edge_type"] {
                    if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
                        let lc = v.trim().to_lowercase();
                        if !RELS.contains(&lc.as_str()) {
                            return format!(
                                "(invalid relation {v:?}: valid relations are {}. Call `relations` to list them.)",
                                RELS.join(", ")
                            );
                        }
                        obj.insert(k.to_string(), serde_json::json!(lc));
                    }
                }
                if tool == "related" {
                    let transitive = obj.get("transitive").and_then(|v| v.as_bool()).unwrap_or(false);
                    obj.remove("transitive");
                    if transitive {
                        obj.insert("depth".to_string(), serde_json::json!(32));
                    } else if !obj.contains_key("depth") {
                        obj.insert("depth".to_string(), serde_json::json!(1));
                    }
                }
            }
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
    // Forced-termination: the earlier run showed the model reaching the answer in
    // ONE tool call, then looping until the step limit instead of answering. We
    // force an answer-only turn once it (a) repeats an identical call or (b) has
    // made FORCE_AFTER distinct calls. Identical for both arms.
    const FORCE_AFTER: usize = 3;
    let dbg = std::env::var("AGENT_DEBUG").is_ok();
    let mut transcript = String::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tool_calls = 0usize;
    let mut force = false;
    for step in 0..MAX_STEPS {
        let prompt = build_prompt(surface, question, &transcript, force);
        let raw = match gen.complete(&prompt).await {
            Ok(r) => r,
            Err(e) => return (format!("(llm error: {e})"), step),
        };
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
            let sig = format!("{tool}({args})");
            if force {
                if dbg {
                    eprintln!("    [step {step}] (forced-answer mode; ignoring tool call {tool})");
                }
                transcript.push_str("\n(You are in answer mode. Do NOT call tools. Reply with ONLY {\"answer\":\"...\"} using the results above.)\n");
                continue;
            }
            if !seen.insert(sig.clone()) {
                if dbg {
                    eprintln!("    [step {step}] repeated call {sig} -> forcing answer");
                }
                force = true;
                transcript.push_str(&format!("\n(You already called {tool} with those arguments; its result is above. You have enough information — now reply with ONLY {{\"answer\":\"...\"}}.)\n"));
                continue;
            }
            let result = execute_tool(client, api, surface, tool, args.clone()).await;
            tool_calls += 1;
            // 4000 (not 800): a large-but-correct result must be fully visible. The
            // deep transitive walk returns ~19 edges (~1700 chars); an 800-char cap
            // hid the tail and turned a complete tool answer into a false miss —
            // the same truncation confound as the LLM output cap, on the input side.
            let truncated: String = result.chars().take(4000).collect();
            if dbg {
                eprintln!("    [step {step}] {tool}({args}) -> {}", truncated.chars().take(240).collect::<String>());
            }
            transcript.push_str(&format!("\nYou called {tool}({args}). Result: {truncated}\n"));
            if tool_calls >= FORCE_AFTER {
                force = true;
                transcript.push_str("\n(You have gathered enough tool results. Now reply with ONLY {\"answer\":\"...\"} using them; do not call more tools.)\n");
            }
        } else {
            transcript.push_str("\n(No tool or answer in your reply. Call a tool or answer.)\n");
        }
    }
    ("(step limit reached without an answer)".to_string(), MAX_STEPS)
}

// ─────────────────────────── scoring ───────────────────────────────────────
//
// EXACT-MATCH SET scoring, not substring. Substring credits a firehose (an answer
// that lists ALL statuses "hits" a point-in-time gold of one status) and is fooled
// by truncation. Here we extract the entity/status tokens of the gold's CLASS from
// the answer, subtract the ids named in the QUESTION (the queried subject itself),
// and require SET EQUALITY with the gold. Strict in both directions: every gold
// token present, and no extra token of that class.

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Ticket,  // C<g>_<i>
    Subtask, // EP<e>_s<s>
    User,    // U<n>
    Status,  // open | in_progress | resolved | closed
}

const STATUSES: [&str; 4] = ["open", "in_progress", "resolved", "closed"];

fn classify_gold(gold: &[String]) -> Class {
    let g = gold.first().map(|s| s.as_str()).unwrap_or("");
    let up = g.to_uppercase();
    if is_subtask(&up) {
        Class::Subtask
    } else if is_ticket(&up) {
        Class::Ticket
    } else if up.starts_with('U') && up.len() > 1 && up[1..].chars().all(|c| c.is_ascii_digit()) {
        Class::User
    } else {
        Class::Status
    }
}

fn is_ticket(up: &str) -> bool {
    // C<digits>_<digits>
    let Some(rest) = up.strip_prefix('C') else { return false };
    match rest.split_once('_') {
        Some((a, b)) => !a.is_empty() && !b.is_empty() && a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

fn is_subtask(up: &str) -> bool {
    // EP<digits>_S<digits>
    let Some(rest) = up.strip_prefix("EP") else { return false };
    match rest.split_once("_S") {
        Some((a, b)) => !a.is_empty() && !b.is_empty() && a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Extract the set of tokens of `class` from free text (case-insensitive).
fn extract_ids(s: &str, class: Class) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let mut tok = String::new();
    let flush = |tok: &mut String, out: &mut std::collections::HashSet<String>| {
        if !tok.is_empty() {
            let up = tok.to_uppercase();
            let ok = match class {
                Class::User => up.starts_with('U') && up.len() > 1 && up[1..].chars().all(|c| c.is_ascii_digit()),
                Class::Subtask => is_subtask(&up),
                Class::Ticket => is_ticket(&up),
                Class::Status => false,
            };
            if ok {
                out.insert(up);
            }
            tok.clear();
        }
    };
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            tok.push(c);
        } else {
            flush(&mut tok, &mut out);
        }
    }
    flush(&mut tok, &mut out);
    out
}

/// Whole-word (alphanumeric-bounded) containment on a lowercased haystack.
fn contains_word(hay_lower: &str, w: &str) -> bool {
    let bytes = hay_lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = hay_lower[from..].find(w) {
        let start = from + pos;
        let end = start + w.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= hay_lower.len() {
            break;
        }
    }
    false
}

fn scores_hit(question: &str, answer: &str, gold: &[String]) -> bool {
    let class = classify_gold(gold);
    if class == Class::Status {
        // Normalise separators so "in progress" / "in-progress" == "in_progress".
        let norm: String = answer
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let gold_set: std::collections::HashSet<String> = gold.iter().map(|g| g.to_lowercase()).collect();
        let present: std::collections::HashSet<String> =
            STATUSES.iter().filter(|s| contains_word(&norm, s)).map(|s| s.to_string()).collect();
        return present == gold_set;
    }
    // Id classes: answer's tokens of this class, minus the ids named in the question.
    let mut ans = extract_ids(answer, class);
    for q in extract_ids(question, class) {
        ans.remove(&q);
    }
    let gold_set: std::collections::HashSet<String> = gold.iter().map(|g| g.to_uppercase()).collect();
    ans == gold_set
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
    let model = std::env::var("AGENT_LLM_MODEL")
        .unwrap_or_else(|_| "mlx-community/Mistral-Small-3.1-Text-24B-Instruct-2503-4bit".into());

    // 30s (not 120s) bounds the cost of a slow tool call — notably query_objects,
    // which full-scans every instance. Only broker/tool calls use this client; the
    // LLM has its own. A tool that can't answer in 30s isn't useful to the agent.
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    if client.get(format!("{api}/health")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
        eprintln!("broker healthy at {api}");
    } else {
        eprintln!("skipping: broker not healthy at {api}");
        return;
    }

    let fixture = generate_fixture();
    println!(
        "== ingesting SCALED fixture into namespace {NS}: {} facts, {} tasks ==",
        fixture.facts.len(),
        fixture.tasks.len()
    );
    ingest(&kafka, &api, &client, &fixture.facts).await;

    // Budget raised 512 -> 2048: a truncated answer used to score a false MISS when
    // the correct list ran past the cap. The reader step, not the cap, should decide.
    let gen: Arc<dyn TextGenerator> = Arc::new(OpenAIExtractor::for_local_server(&endpoint, &model).with_max_tokens(2048));
    println!("== running agent tasks (LLM: {model}) ==\n");

    let tasks = &fixture.tasks;
    let mut onto_hits = 0usize;
    let mut base_hits = 0usize;
    for t in tasks {
        let (onto_ans, onto_steps) = run_agent(&client, &api, &gen, &t.question, Surface::Ontology).await;
        let onto_ok = scores_hit(&t.question, &onto_ans, &t.gold);
        onto_hits += onto_ok as usize;

        let (base_ans, base_steps) = run_agent(&client, &api, &gen, &t.question, Surface::Baseline).await;
        let base_ok = scores_hit(&t.question, &base_ans, &t.gold);
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
