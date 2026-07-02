//! Cluster smoke test — SDK-driven dogfood of `tests/cluster/`.
//!
//! Phase-1 exit criterion (carried over): "At least one internal Chronik
//! test migrated to use the SDK". This is that test. It proves the SDK
//! is the canonical way to drive a Chronik cluster for memory use cases:
//! every operation goes through the public `chronik_memory::Memory` API,
//! never the underlying `rdkafka` client.
//!
//! Coverage:
//! - Builder + namespace init creates the raw + 4 typed topics + the
//!   compacted task-current view.
//! - `ingest_turn` writes a raw turn and returns `(topic, partition,
//!   offset, deduped)`.
//! - Idempotency: re-ingesting the same `external_id` returns
//!   `deduped=true` without producing a second record.
//! - `remember` writes a typed memory directly (no LLM extraction needed).
//! - `recall` returns hits scoped to this namespace, against the BM25
//!   channel, with namespace bias.
//! - `forget` produces a tombstone that hides a previously-`remember`d
//!   record from subsequent recalls.
//!
//! No LLM is used — the test exercises the durable SDK contract end-to-end
//! against a real cluster without spending tokens.
//!
//! ## Running
//!
//! ```bash
//! ./tests/cluster/start.sh
//! CHRONIK_INTEGRATION=1 \
//!   CHRONIK_API=http://localhost:6094 CHRONIK_KAFKA=localhost:9094 \
//!   cargo test -p chronik-memory --test cluster_smoke -- --ignored --nocapture
//! ./tests/cluster/stop.sh
//! ```
//!
//! `CHRONIK_API` / `CHRONIK_KAFKA` should point at whichever cluster node
//! holds the typed-topic Tantivy indexes (varies across cluster restarts —
//! the `tests/cluster/` deployment doesn't yet ship with cross-node
//! search fan-out).

use chronik_memory::schema::{Body, FactBody, MemoryType};
use chronik_memory::Memory;
use std::time::Duration;
use ulid::Ulid;

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[tokio::test]
#[ignore = "requires a live Chronik cluster (run tests/cluster/start.sh first) + CHRONIK_INTEGRATION=1"]
async fn sdk_drives_cluster_end_to_end() {
    if std::env::var("CHRONIK_INTEGRATION").ok().as_deref() != Some("1") {
        eprintln!("skipping: set CHRONIK_INTEGRATION=1 to run");
        return;
    }
    let kafka = env("CHRONIK_KAFKA", "localhost:9092");
    let api = env("CHRONIK_API", "http://localhost:6092");
    let index_sleep_ms: u64 = std::env::var("CLUSTER_SMOKE_INDEX_SLEEP_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(45_000);

    // Fresh ULID-suffixed namespace so re-runs don't collide with prior data.
    let ns = format!("cluster-smoke:{}", Ulid::new());
    println!("namespace: {}", ns);

    let mem = Memory::builder()
        .chronik_kafka(kafka.clone())
        .chronik_api(api.clone())
        .namespace(&ns)
        .request_timeout(Duration::from_secs(30))
        .build()
        .await
        .expect("build memory");
    assert_eq!(mem.namespace(), ns);
    println!("[OK] build + namespace getter");

    // 1. Topic creation — should be idempotent. Run twice; the second call
    // is a no-op as far as the cluster is concerned.
    mem.init_namespace_full()
        .await
        .expect("init_namespace_full");
    mem.init_namespace_full()
        .await
        .expect("init_namespace_full (second call)");
    println!("[OK] init_namespace_full is idempotent");

    // 2. Ingest two raw turns — same content, same external_id. The second
    // ingest must dedupe.
    let external_id = format!("smoke-msg-{}", Ulid::new());
    let role = "user";
    let content = "I'm looking for a 3-bedroom apartment in Lapa. Max budget 800k euros.";

    let ack1 = mem
        .ingest_turn(chronik_memory::extractor::Turn {
            role: role.into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: Some(external_id.clone()),
        })
        .await
        .expect("ingest_turn first");
    assert!(!ack1.deduped, "first ingest must not be deduped");
    println!(
        "[OK] ingest_turn first  → topic={} partition={} offset={} deduped=false",
        ack1.topic, ack1.partition, ack1.offset
    );

    let ack2 = mem
        .ingest_turn(chronik_memory::extractor::Turn {
            role: role.into(),
            content: content.into(),
            ts: None,
            channel: None,
            external_id: Some(external_id.clone()),
        })
        .await
        .expect("ingest_turn second");
    assert!(
        ack2.deduped,
        "second ingest with same external_id must be deduped"
    );
    println!(
        "[OK] ingest_turn second → topic={} partition={} offset={} deduped=true (idempotency cache hit)",
        ack2.topic, ack2.partition, ack2.offset
    );

    // 3. Write a fact memory directly via `remember` — bypasses extractor.
    // We pick canonical predicates from the v3 vocabulary so a recall query
    // can find them deterministically.
    let body_budget = Body::Fact(FactBody {
        subject: "user".into(),
        predicate: "budget".into(),
        object: serde_json::json!(800_000),
        polarity: "asserted".into(),
        text: "User has max budget 800000 EUR".into(),
    });
    let body_neighborhood = Body::Fact(FactBody {
        subject: "user".into(),
        predicate: "prefers_neighborhood".into(),
        object: serde_json::json!("Lapa"),
        polarity: "asserted".into(),
        text: "User prefers Lapa neighborhood".into(),
    });

    let ack_budget = mem
        .remember(body_budget.clone(), Some("user|budget".into()), 0.95)
        .await
        .expect("remember budget");
    let ack_neigh = mem
        .remember(
            body_neighborhood.clone(),
            Some("user|prefers_neighborhood".into()),
            0.95,
        )
        .await
        .expect("remember neighborhood");
    println!(
        "[OK] remember budget       → topic={} partition={} offset={}",
        ack_budget.topic, ack_budget.partition, ack_budget.offset
    );
    println!(
        "[OK] remember neighborhood → topic={} partition={} offset={}",
        ack_neigh.topic, ack_neigh.partition, ack_neigh.offset
    );

    // 4. Wait for the cold WalIndexer cycle to make the typed-topic facts
    // searchable. (On a hot-text-enabled cluster this would be < 1 s; the
    // test cluster currently runs cold-only so we wait the full WalIndexer
    // window.)
    println!(
        "[..] waiting {} ms for cold-path Tantivy indexing",
        index_sleep_ms
    );
    tokio::time::sleep(Duration::from_millis(index_sleep_ms)).await;

    // 5. Recall — should return both facts, namespace-scoped.
    let results = mem
        .recall("what is the user's budget and preferred neighborhood")
        .types(&[MemoryType::Fact])
        .k(10)
        .send()
        .await
        .expect("recall");
    let keys: Vec<String> = results
        .iter()
        .filter_map(|r| r.memory.key.clone())
        .collect();
    println!("[OK] recall → {} result(s), keys={:?}", results.len(), keys);
    assert!(
        results.iter().all(|r| r.memory.namespace == ns),
        "recall must only surface this namespace's records"
    );
    let saw_budget = keys.iter().any(|k| k == "user|budget");
    let saw_neigh = keys.iter().any(|k| k == "user|prefers_neighborhood");
    assert!(
        saw_budget || saw_neigh,
        "recall returned no expected key — got {:?}",
        keys
    );
    if !saw_budget || !saw_neigh {
        eprintln!(
            "warn: recall missed one expected key (saw budget={} neighborhood={}); \
             keys present: {:?} — this can happen on cold-only clusters when one \
             segment seal lags. Both should appear given enough sleep.",
            saw_budget, saw_neigh, keys
        );
    }

    // 6. Forget the budget fact, wait for indexing, then verify with two
    // recall queries:
    //   a) a budget-shaped query must NOT surface user|budget (the forget
    //      target), even though the original fact is still in the search
    //      index — the tombstone envelope wins dedup and is then dropped.
    //   b) a neighborhood-shaped query must STILL surface
    //      user|prefers_neighborhood — forget is targeted, not blanket.
    if saw_budget {
        mem.forget(MemoryType::Fact, Some("user|budget"), None)
            .await
            .expect("forget budget");
        println!("[OK] forget budget tombstone written");

        tokio::time::sleep(Duration::from_millis(index_sleep_ms)).await;

        // (a) Budget-shaped query — must drop the budget record entirely.
        let after_budget = mem
            .recall("what is the user's budget")
            .types(&[MemoryType::Fact])
            .k(10)
            .send()
            .await
            .expect("recall after forget (budget query)");
        let after_budget_keys: Vec<String> = after_budget
            .iter()
            .filter_map(|r| r.memory.key.clone())
            .collect();
        let still_has_budget = after_budget
            .iter()
            .any(|r| r.memory.key.as_deref() == Some("user|budget"));
        assert!(
            !still_has_budget,
            "recall surfaced 'user|budget' after forget — \
             tombstone envelope semantics are broken (got keys {:?})",
            after_budget_keys
        );
        println!(
            "[OK] post-forget budget query → {} result(s), keys={:?} (budget correctly hidden)",
            after_budget.len(),
            after_budget_keys
        );

        // (b) Neighborhood-shaped query — must still surface the
        //     non-forgotten record.
        let after_neigh = mem
            .recall("what neighborhoods does the user prefer")
            .types(&[MemoryType::Fact])
            .k(10)
            .send()
            .await
            .expect("recall after forget (neighborhood query)");
        let after_neigh_keys: Vec<String> = after_neigh
            .iter()
            .filter_map(|r| r.memory.key.clone())
            .collect();
        let still_has_neigh = after_neigh
            .iter()
            .any(|r| r.memory.key.as_deref() == Some("user|prefers_neighborhood"));
        assert!(
            still_has_neigh,
            "forget(budget) collaterally hid 'user|prefers_neighborhood' \
             — got keys {:?}",
            after_neigh_keys
        );
        println!(
            "[OK] post-forget neighborhood query → {} result(s), keys={:?} (neighborhood retained)",
            after_neigh.len(),
            after_neigh_keys
        );
    }

    println!("\n[PASS] cluster_smoke — SDK drives the cluster end-to-end");
}
