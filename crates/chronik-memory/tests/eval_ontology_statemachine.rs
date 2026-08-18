//! Ontology efficacy harness — an issue-tracker **state machine**, evaluated the
//! way the field actually evaluates structured-state + validated-action agents
//! (τ-bench / AppWorld: a backing state + actions, verified against ground
//! truth; temporal-KGQA: point-in-time; multi-hop KGQA: traversal). This is the
//! RIGHT test for the ontology layer — NOT scattered-detail recall, which is
//! what LongMemEval measures and where the object model correctly scored ~0
//! (see memory `ontology-o0-shipped`).
//!
//! ## What it tests
//! A deterministic append-only **event log** for an issue tracker defines the
//! ground truth: every ticket's state at every point in time, and the link
//! graph. An independent **reference model** (imperative fold over events)
//! computes the correct answers; the **ontology projection**
//! (`assemble_entity` / `filter_as_of` / `assemble_edges`) computes them
//! declaratively from the same events encoded as bitemporal facts. Tier A scores
//! the ontology EXACT-MATCH against the reference model:
//!   - **state**    — current status / assignee / project per ticket
//!   - **as_of**    — status at historical timestamps (point-in-time; flat
//!                    recall physically cannot do this)
//!   - **traverse** — 1-hop and 2-hop `blocked_by` chains + `parent_of` subtasks
//!
//! Tier A is deterministic and needs no broker or LLM — it runs under plain
//! `cargo test`. Tier B (agentic validated actions, τ-bench-style) is added on
//! top and gated behind `--ignored` + a live stack.
//!
//! ```bash
//! cargo test -p chronik-memory --test eval_ontology_statemachine tier_a -- --nocapture
//! ```

use chronik_ontology::{assemble_edges, assemble_entity, filter_as_of};
use chrono::{DateTime, TimeZone, Utc};
use std::collections::{BTreeMap, BTreeSet};

// ───────────────────────────── domain ──────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Open,
    InProgress,
    Resolved,
    Closed,
}
impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in_progress",
            Status::Resolved => "resolved",
            Status::Closed => "closed",
        }
    }
    /// A blocker is "done" (no longer blocking) once resolved or closed.
    #[allow(dead_code)] // used by Tier B's action validator
    fn is_done(self) -> bool {
        matches!(self, Status::Resolved | Status::Closed)
    }
}

type Tick = String;
type User = String;

/// Append-only domain events. `t` is a logical timestamp (day index).
#[derive(Clone, Debug)]
enum Event {
    Created { t: i64, ticket: Tick, project: String },
    Assigned { t: i64, ticket: Tick, user: User },
    StatusSet { t: i64, ticket: Tick, status: Status },
    BlockedBy { t: i64, ticket: Tick, blocker: Tick },
    Unblocked { t: i64, ticket: Tick, blocker: Tick },
    ParentOf { t: i64, parent: Tick, child: Tick },
}
impl Event {
    fn t(&self) -> i64 {
        match self {
            Event::Created { t, .. }
            | Event::Assigned { t, .. }
            | Event::StatusSet { t, .. }
            | Event::BlockedBy { t, .. }
            | Event::Unblocked { t, .. }
            | Event::ParentOf { t, .. } => *t,
        }
    }
}

// ─────────────────────────── generator ─────────────────────────────────────
//
// A STRUCTURED, deterministic generator (no RNG — fully reproducible). Tickets
// are grouped into blocker chains of 3 (T{g}a <- T{g}b <- T{g}c: c blocked_by b
// blocked_by a) and epics own two subtasks. Statuses advance on a per-ticket
// schedule staggered by id so `as_of` at different times yields different
// answers, and blockers clear when their blocker resolves.

struct World {
    events: Vec<Event>,
    tickets: Vec<Tick>,
    epics: Vec<(Tick, Vec<Tick>)>,     // epic -> subtasks
    chains: Vec<Vec<Tick>>,            // [a, b, c] with c<-b<-a
    now: i64,                          // a timestamp after all events
}

fn generate(n_chains: usize, n_epics: usize) -> World {
    let mut ev = Vec::new();
    let mut tickets = Vec::new();
    let mut chains = Vec::new();
    let mut id = 0usize;
    let mut mk = |ev: &mut Vec<Event>, tickets: &mut Vec<Tick>, id: &mut usize, t: i64| -> Tick {
        let tk = format!("T{}", *id);
        *id += 1;
        ev.push(Event::Created { t, ticket: tk.clone(), project: format!("P{}", (*id) % 4) });
        ev.push(Event::StatusSet { t, ticket: tk.clone(), status: Status::Open });
        tickets.push(tk.clone());
        tk
    };

    // Blocker chains of 3: a <- b <- c (c blocked_by b, b blocked_by a).
    for g in 0..n_chains {
        let a = mk(&mut ev, &mut tickets, &mut id, 0);
        let b = mk(&mut ev, &mut tickets, &mut id, 0);
        let c = mk(&mut ev, &mut tickets, &mut id, 0);
        ev.push(Event::BlockedBy { t: 1, ticket: b.clone(), blocker: a.clone() });
        ev.push(Event::BlockedBy { t: 1, ticket: c.clone(), blocker: b.clone() });
        // assignment
        ev.push(Event::Assigned { t: 2, ticket: a.clone(), user: format!("U{}", g % 5) });
        ev.push(Event::Assigned { t: 2, ticket: b.clone(), user: format!("U{}", (g + 1) % 5) });
        ev.push(Event::Assigned { t: 2, ticket: c.clone(), user: format!("U{}", (g + 2) % 5) });
        // `a` progresses and resolves at t=5, which unblocks `b`.
        ev.push(Event::StatusSet { t: 3, ticket: a.clone(), status: Status::InProgress });
        ev.push(Event::StatusSet { t: 5, ticket: a.clone(), status: Status::Resolved });
        ev.push(Event::Unblocked { t: 5, ticket: b.clone(), blocker: a.clone() });
        // `b` then progresses and resolves at t=8, unblocking `c`.
        ev.push(Event::StatusSet { t: 6, ticket: b.clone(), status: Status::InProgress });
        ev.push(Event::StatusSet { t: 8, ticket: b.clone(), status: Status::Resolved });
        ev.push(Event::Unblocked { t: 8, ticket: c.clone(), blocker: b.clone() });
        // `c` progresses; a reassignment on the first chain exercises evolving
        // single-valued attributes for as_of.
        ev.push(Event::StatusSet { t: 9, ticket: c.clone(), status: Status::InProgress });
        if g == 0 {
            ev.push(Event::Assigned { t: 7, ticket: c.clone(), user: "U9".to_string() });
        }
        // `a` gets closed at t=10; on some chains reopened at t=12.
        ev.push(Event::StatusSet { t: 10, ticket: a.clone(), status: Status::Closed });
        if g % 2 == 0 {
            ev.push(Event::StatusSet { t: 12, ticket: a.clone(), status: Status::Open });
        }
        chains.push(vec![a, b, c]);
    }

    // Epics with two subtasks each.
    for _e in 0..n_epics {
        let epic = mk(&mut ev, &mut tickets, &mut id, 0);
        let s1 = mk(&mut ev, &mut tickets, &mut id, 0);
        let s2 = mk(&mut ev, &mut tickets, &mut id, 0);
        ev.push(Event::ParentOf { t: 1, parent: epic.clone(), child: s1.clone() });
        ev.push(Event::ParentOf { t: 1, parent: epic.clone(), child: s2.clone() });
        ev.push(Event::Assigned { t: 2, ticket: epic.clone(), user: "U0".to_string() });
        ev.push(Event::StatusSet { t: 4, ticket: s1.clone(), status: Status::InProgress });
        ev.push(Event::StatusSet { t: 9, ticket: s1, status: Status::Resolved });
        let _ = &s2; // s2 stays Open (its only assertions are Created + ParentOf)
    }
    // Rebuild epics vec cleanly (the closure borrow above made inline hard).
    let mut epics_out: Vec<(Tick, Vec<Tick>)> = Vec::new();
    {
        // reconstruct epic->children from the ParentOf events we emitted
        let mut m: BTreeMap<Tick, Vec<Tick>> = BTreeMap::new();
        for e in &ev {
            if let Event::ParentOf { parent, child, .. } = e {
                m.entry(parent.clone()).or_default().push(child.clone());
            }
        }
        for (p, mut cs) in m {
            cs.sort();
            epics_out.push((p, cs));
        }
    }

    World {
        events: ev,
        tickets,
        epics: epics_out,
        chains,
        now: 1000,
    }
}

// ───────────────────────── reference model ─────────────────────────────────
//
// The ground truth: straightforward imperative folds over the event log. These
// are the "correct answers" the ontology projection is scored against.

impl World {
    fn status_at(&self, ticket: &str, t: i64) -> Option<Status> {
        let mut cur = None;
        for e in &self.events {
            if e.t() > t {
                continue;
            }
            if let Event::StatusSet { ticket: tk, status, .. } = e {
                if tk == ticket {
                    cur = Some(*status);
                }
            }
        }
        cur
    }

    fn assignee_at(&self, ticket: &str, t: i64) -> Option<User> {
        let mut cur = None;
        for e in &self.events {
            if e.t() > t {
                continue;
            }
            if let Event::Assigned { ticket: tk, user, .. } = e {
                if tk == ticket {
                    cur = Some(user.clone());
                }
            }
        }
        cur
    }

    fn project_of(&self, ticket: &str) -> Option<String> {
        for e in &self.events {
            if let Event::Created { ticket: tk, project, .. } = e {
                if tk == ticket {
                    return Some(project.clone());
                }
            }
        }
        None
    }

    /// The set of tickets directly blocking `ticket` at time `t` (blocked_by
    /// added and not yet removed as of `t`).
    fn blockers_at(&self, ticket: &str, t: i64) -> BTreeSet<Tick> {
        let mut s = BTreeSet::new();
        for e in &self.events {
            if e.t() > t {
                continue;
            }
            match e {
                Event::BlockedBy { ticket: tk, blocker, .. } if tk == ticket => {
                    s.insert(blocker.clone());
                }
                Event::Unblocked { ticket: tk, blocker, .. } if tk == ticket => {
                    s.remove(blocker);
                }
                _ => {}
            }
        }
        s
    }

    /// Transitive blockers up to `max_depth` hops (blocked_by chain) at time `t`.
    fn blockers_transitive(&self, ticket: &str, t: i64, max_depth: usize) -> BTreeSet<Tick> {
        let mut out = BTreeSet::new();
        let mut frontier = vec![ticket.to_string()];
        for _ in 0..max_depth {
            let mut next = Vec::new();
            for node in &frontier {
                for b in self.blockers_at(node, t) {
                    if out.insert(b.clone()) {
                        next.push(b);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }
}

// ───────────────── bitemporal fact encoding (events -> facts) ───────────────
//
// Encode the event log as the append-only bitemporal facts the ontology reads:
// one fact per assertion, with non-overlapping [valid_from, valid_to) intervals
// for evolving single-valued attributes (status, assignee), and open intervals
// closed by the matching Unblocked for blocked_by. Provenance offset = the
// event's index in the log (the "raw" position).

const NS: &str = "issuetracker";

fn ts(t: i64) -> String {
    // logical day index -> a real RFC3339 instant filter_as_of can parse
    let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    (base + chrono::Duration::days(t)).to_rfc3339()
}

fn fact(
    subject: &str,
    predicate: &str,
    object: serde_json::Value,
    valid_from: i64,
    valid_to: Option<i64>,
    off: i64,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "namespace": NS,
        "type": "fact",
        "body": {"subject": subject, "predicate": predicate, "object": object},
        "source": {"topic": format!("mem.raw.{}", NS), "offsets": [off], "extractor": "gen@1"},
        "valid_from": ts(valid_from),
    });
    if let Some(vt) = valid_to {
        v["valid_to"] = serde_json::json!(ts(vt));
    }
    v
}

/// Turn the event log into the bitemporal fact set the ontology projects over.
fn encode_facts(w: &World) -> Vec<serde_json::Value> {
    let mut facts = Vec::new();

    // Evolving single-valued: status, assignee. Emit one fact per change, its
    // interval closed by the next change for the same (ticket, attr).
    for ticket in &w.tickets {
        // status
        let mut hist: Vec<(i64, Status, i64)> = Vec::new();
        for (i, e) in w.events.iter().enumerate() {
            if let Event::StatusSet { t, ticket: tk, status } = e {
                if tk == ticket {
                    hist.push((*t, *status, i as i64));
                }
            }
        }
        for k in 0..hist.len() {
            let (t, st, off) = hist[k].clone();
            let vt = hist.get(k + 1).map(|n| n.0);
            facts.push(fact(ticket, "status", serde_json::json!(st.as_str()), t, vt, off));
        }
        // assignee
        let mut ah: Vec<(i64, String, i64)> = Vec::new();
        for (i, e) in w.events.iter().enumerate() {
            if let Event::Assigned { t, ticket: tk, user } = e {
                if tk == ticket {
                    ah.push((*t, user.clone(), i as i64));
                }
            }
        }
        for k in 0..ah.len() {
            let (t, user, off) = ah[k].clone();
            let vt = ah.get(k + 1).map(|n| n.0);
            facts.push(fact(ticket, "assignee", serde_json::json!(user), t, vt, off));
        }
        // project (immutable, set at creation)
        if let Some(p) = w.project_of(ticket) {
            facts.push(fact(ticket, "project", serde_json::json!(p), 0, None, 0));
        }
    }

    // blocked_by: interval from the BlockedBy to the matching Unblocked (or open).
    for (i, e) in w.events.iter().enumerate() {
        if let Event::BlockedBy { t, ticket, blocker } = e {
            // find the matching Unblocked after this
            let vt = w.events.iter().find_map(|u| match u {
                Event::Unblocked { t: ut, ticket: utk, blocker: ub } if utk == ticket && ub == blocker && *ut >= *t => Some(*ut),
                _ => None,
            });
            facts.push(fact(ticket, "blocked_by", serde_json::json!(blocker), *t, vt, i as i64));
        }
    }

    // parent_of (immutable link)
    for (i, e) in w.events.iter().enumerate() {
        if let Event::ParentOf { t, parent, child } = e {
            facts.push(fact(parent, "parent_of", serde_json::json!(child), *t, None, i as i64));
        }
    }

    facts
}

// ─────────────────────────── Tier A scoring ────────────────────────────────

fn as_of(t: i64) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&ts(t)).ok().map(|d| d.with_timezone(&Utc))
}

/// The single-valued attribute value the ontology reports for `ticket.attr` at
/// `at` (None if absent). Mirrors what `resolve_entity` returns, but pure.
fn onto_attr_at(facts: &[serde_json::Value], ticket: &str, attr: &str, at: Option<DateTime<Utc>>) -> Option<String> {
    let filtered = filter_as_of(facts.to_vec(), at);
    let ev = assemble_entity(&filtered, NS, "subject", ticket, true)?;
    let tri = ev.triples.iter().find(|t| t.name == attr)?;
    tri.values.last().and_then(|v| v.as_str().map(|s| s.to_string()))
}

/// The set of `blocked_by` targets the ontology reports for `ticket` at `at`.
fn onto_blockers_at(facts: &[serde_json::Value], ticket: &str, at: Option<DateTime<Utc>>) -> BTreeSet<String> {
    let filtered = filter_as_of(facts.to_vec(), at);
    assemble_edges(&filtered, NS, ticket, "blocked_by", true, 1)
        .into_iter()
        .filter_map(|e| e.to.as_str().map(|s| s.to_string()))
        .collect()
}

/// Transitive blocked_by via repeated `assemble_edges` (the pure analogue of the
/// async BFS `traverse`), to `max_depth`, at `at`.
fn onto_blockers_transitive(facts: &[serde_json::Value], ticket: &str, at: Option<DateTime<Utc>>, max_depth: usize) -> BTreeSet<String> {
    let filtered = filter_as_of(facts.to_vec(), at);
    let mut out = BTreeSet::new();
    let mut frontier = vec![ticket.to_string()];
    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(ticket.to_lowercase());
    for _ in 0..max_depth {
        let mut next = Vec::new();
        for node in &frontier {
            for e in assemble_edges(&filtered, NS, node, "blocked_by", true, 1) {
                if let Some(t) = e.to.as_str() {
                    if visited.insert(t.to_lowercase()) {
                        out.insert(t.to_string());
                        next.push(t.to_string());
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

fn onto_subtasks(facts: &[serde_json::Value], epic: &str) -> BTreeSet<String> {
    assemble_edges(facts, NS, epic, "parent_of", true, 1)
        .into_iter()
        .filter_map(|e| e.to.as_str().map(|s| s.to_string()))
        .collect()
}

#[derive(Default)]
struct Score {
    pass: usize,
    total: usize,
}
impl Score {
    fn add(&mut self, ok: bool) {
        self.total += 1;
        if ok {
            self.pass += 1;
        }
    }
    fn rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.pass as f64 / self.total as f64
        }
    }
}

#[test]
fn tier_a_ontology_state_machine_correctness() {
    let w = generate(6, 3); // 6 blocker-chains (18) + 3 epics (9) = 27 tickets
    let facts = encode_facts(&w);

    println!(
        "\nOntology state-machine harness (Tier A) — {} tickets, {} events, {} bitemporal facts",
        w.tickets.len(),
        w.events.len(),
        facts.len()
    );

    let mut state = Score::default(); // current status/assignee/project
    let mut asof = Score::default(); // status at historical times
    let mut hop1 = Score::default(); // direct blockers now
    let mut hop2 = Score::default(); // transitive blockers now
    let mut asof_edge = Score::default(); // blockers at historical times
    let mut subtask = Score::default();

    // Current state = as_of(now). On APPEND-ONLY backing "current" is a
    // point-in-time query at the present instant, NOT an unfiltered read — an
    // unfiltered read returns every historical version (correct only over a
    // compacted backing where superseded versions are physically gone).
    let now_ts = as_of(w.now);
    for tk in &w.tickets {
        let want_status = w.status_at(tk, w.now).map(|s| s.as_str().to_string());
        let got_status = onto_attr_at(&facts, tk, "status", now_ts);
        state.add(want_status == got_status);

        let want_asg = w.assignee_at(tk, w.now);
        let got_asg = onto_attr_at(&facts, tk, "assignee", now_ts);
        state.add(want_asg == got_asg);

        let want_proj = w.project_of(tk);
        let got_proj = onto_attr_at(&facts, tk, "project", now_ts);
        state.add(want_proj == got_proj);
    }

    // As_of status at several historical timestamps (point-in-time)
    for tk in &w.tickets {
        for t in [1, 4, 6, 9, 11, 13] {
            let want = w.status_at(tk, t).map(|s| s.as_str().to_string());
            let got = onto_attr_at(&facts, tk, "status", as_of(t));
            asof.add(want == got);
        }
    }

    // Traverse: 1-hop and 2-hop blocked_by chains at `now` (= as_of now, so
    // edges whose valid_to has passed are correctly excluded).
    for tk in &w.tickets {
        let want1 = w.blockers_at(tk, w.now);
        let got1 = onto_blockers_at(&facts, tk, now_ts);
        hop1.add(want1 == got1);

        let want2 = w.blockers_transitive(tk, w.now, 2);
        let got2 = onto_blockers_transitive(&facts, tk, now_ts, 2);
        hop2.add(want2 == got2);
    }

    // As_of traverse: blockers at historical times (they clear as blockers resolve)
    for chain in &w.chains {
        let c = &chain[2]; // the chain tail (blocked_by b blocked_by a)
        for t in [1, 4, 6, 9] {
            let want = w.blockers_transitive(c, t, 2);
            let got = onto_blockers_transitive(&facts, c, as_of(t), 2);
            asof_edge.add(want == got);
        }
    }

    // Traverse parent_of: subtasks
    for (epic, kids) in &w.epics {
        let want: BTreeSet<String> = kids.iter().cloned().collect();
        let got = onto_subtasks(&facts, epic);
        subtask.add(want == got);
    }

    println!("  state (status/assignee/project, now) : {}/{}  = {:.3}", state.pass, state.total, state.rate());
    println!("  as_of  (status @ 6 historical times) : {}/{}  = {:.3}", asof.pass, asof.total, asof.rate());
    println!("  traverse 1-hop blocked_by (now)      : {}/{}  = {:.3}", hop1.pass, hop1.total, hop1.rate());
    println!("  traverse 2-hop blocked_by (now)      : {}/{}  = {:.3}", hop2.pass, hop2.total, hop2.rate());
    println!("  as_of traverse (blockers @ time)     : {}/{}  = {:.3}", asof_edge.pass, asof_edge.total, asof_edge.rate());
    println!("  traverse parent_of (subtasks)        : {}/{}  = {:.3}", subtask.pass, subtask.total, subtask.rate());
    let overall_pass = state.pass + asof.pass + hop1.pass + hop2.pass + asof_edge.pass + subtask.pass;
    let overall_total = state.total + asof.total + hop1.total + hop2.total + asof_edge.total + subtask.total;
    println!("  OVERALL                              : {}/{}  = {:.3}", overall_pass, overall_total, overall_pass as f64 / overall_total as f64);
    println!(
        "\n  (The point vs flat memory: `as_of` and guaranteed multi-hop traversal are\n   capabilities the object model HAS and flat recall structurally cannot do.)\n"
    );

    // The ontology projection must be EXACTLY correct on this structured domain —
    // that is the whole claim. Anything less is a real ontology bug to fix.
    assert_eq!(state.pass, state.total, "current-state resolution must be exact");
    assert_eq!(asof.pass, asof.total, "as_of point-in-time resolution must be exact");
    assert_eq!(hop1.pass, hop1.total, "1-hop traversal must be exact");
    assert_eq!(hop2.pass, hop2.total, "2-hop traversal must be exact");
    assert_eq!(asof_edge.pass, asof_edge.total, "as_of traversal must be exact");
    assert_eq!(subtask.pass, subtask.total, "parent_of traversal must be exact");
}

// ═══════════════════════════ Tier B — validated actions ════════════════════
//
// τ-bench / AppWorld essence: an agent takes ACTIONS against structured state,
// and we verify the OUTCOME against ground truth — not just that a tool was
// called. Here the action engine runs entirely in the ontology layer over the
// CAS-append primitive (`chronik_ontology::CasLog` — the O-3 spike), so it needs
// NO change to the broker produce path (the coordinator/optimistic-CAS approach
// settled earlier). Each action:
//   1. projects the target aggregate's current state from its event stream,
//   2. validates domain invariants (legal transition; a ticket can't be CLOSED
//      while any blocker is still unresolved — the cross-object invariant),
//   3. CAS-appends the resulting events iff the aggregate hasn't moved since the
//      agent read it (the lost-update guard), idempotent on retry.
// We score each (action -> outcome) against the expected outcome, plus a
// concurrency race where exactly one of N racing agents may win.

use chronik_ontology::CasLog;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Outcome {
    Applied,
    Rejected,
    Conflict,
}

enum Act {
    Start,
    Resolve,
    Close,
    Assign(&'static str),
}
impl Act {
    fn events(&self) -> Vec<serde_json::Value> {
        let (kind, value) = match self {
            Act::Start => ("status", "in_progress"),
            Act::Resolve => ("status", "resolved"),
            Act::Close => ("status", "closed"),
            Act::Assign(u) => ("assignee", *u),
        };
        vec![serde_json::json!({"kind": kind, "value": value})]
    }
}

/// Project (status, blockers, assignee) from an aggregate's event stream.
fn project(events: &[serde_json::Value]) -> (String, BTreeSet<String>, Option<String>) {
    let mut status = "open".to_string();
    let mut blockers = BTreeSet::new();
    let mut assignee = None;
    for e in events {
        let val = e.get("value").and_then(|v| v.as_str()).unwrap_or("");
        match e.get("kind").and_then(|v| v.as_str()) {
            Some("status") => status = val.to_string(),
            Some("blocked_by") => {
                blockers.insert(val.to_string());
            }
            Some("unblocked") => {
                blockers.remove(val);
            }
            Some("assignee") => assignee = Some(val.to_string()),
            _ => {}
        }
    }
    (status, blockers, assignee)
}

fn is_done_status(s: &str) -> bool {
    s == "resolved" || s == "closed"
}

/// The action-layer validator: legal transition + cross-object invariants,
/// evaluated against the CURRENT projected state (the read a real Action does
/// via get_object/traverse before proposing).
fn precondition_ok(log: &CasLog, ticket: &str, act: &Act) -> Result<(), String> {
    let (status, blockers, _) = project(&log.snapshot(ticket));
    match act {
        Act::Start => (status == "open")
            .then_some(())
            .ok_or_else(|| format!("start requires open (is {status})")),
        Act::Resolve => (status == "open" || status == "in_progress")
            .then_some(())
            .ok_or_else(|| format!("resolve requires open/in_progress (is {status})")),
        Act::Close => {
            for b in &blockers {
                let (bs, _, _) = project(&log.snapshot(b));
                if !is_done_status(&bs) {
                    return Err(format!("close blocked: {b} is {bs}"));
                }
            }
            Ok(())
        }
        Act::Assign(_) => (status != "closed")
            .then_some(())
            .ok_or_else(|| "cannot assign a closed ticket".to_string()),
    }
}

/// Apply an action: validate, then CAS-append at `expected` head. Rejected on an
/// invariant violation (no append), Conflict if the aggregate moved since the
/// agent read it, Applied on success.
fn apply(log: &CasLog, ticket: &str, act: &Act, expected: i64, idem: &str) -> Outcome {
    if precondition_ok(log, ticket, act).is_err() {
        return Outcome::Rejected;
    }
    match log.cas_append(ticket, expected, act.events(), idem) {
        Ok(_) => Outcome::Applied,
        Err(_) => Outcome::Conflict,
    }
}

#[test]
fn tier_b_validated_actions_and_cas() {
    let log = CasLog::new();
    // Seed: T1 open; T2 open & blocked_by T1; T3 open.
    log.cas_append("T1", -1, vec![serde_json::json!({"kind":"status","value":"open"})], "seed-T1").unwrap();
    log.cas_append(
        "T2",
        -1,
        vec![
            serde_json::json!({"kind":"status","value":"open"}),
            serde_json::json!({"kind":"blocked_by","value":"T1"}),
        ],
        "seed-T2",
    )
    .unwrap();
    log.cas_append("T3", -1, vec![serde_json::json!({"kind":"status","value":"open"})], "seed-T3").unwrap();

    println!("\nOntology validated-action harness (Tier B) — actions over CasLog (O-3 primitive)");
    let mut sc = Score::default();
    let mut check = |label: &str, got: Outcome, want: Outcome| {
        let ok = got == want;
        println!("  {:46} got={:?} want={:?}  {}", label, got, want, if ok { "✓" } else { "✗" });
        sc.add(ok);
    };

    // 1. Invariant: close T2 while blocker T1 is open -> Rejected (no append).
    let v = log.head_offset("T2");
    check("close T2 (blocker open) -> reject", apply(&log, "T2", &Act::Close, v, "a1"), Outcome::Rejected);

    // 2. Legal: resolve T1 -> Applied.
    let v = log.head_offset("T1");
    check("resolve T1 -> apply", apply(&log, "T1", &Act::Resolve, v, "a2"), Outcome::Applied);

    // 3. Now legal: close T2 (blocker resolved) -> Applied.
    let v = log.head_offset("T2");
    check("close T2 (blocker resolved) -> apply", apply(&log, "T2", &Act::Close, v, "a3"), Outcome::Applied);

    // 4. Illegal transition: start T2 (closed) -> Rejected.
    let v = log.head_offset("T2");
    check("start T2 (closed) -> reject", apply(&log, "T2", &Act::Start, v, "a4"), Outcome::Rejected);

    // 5. Lost-update guard: two agents read T3 at the same head; only the first
    //    Assign wins, the second (stale token) Conflicts.
    let v = log.head_offset("T3");
    check("assign T3 @v (agent A) -> apply", apply(&log, "T3", &Act::Assign("U1"), v, "a5a"), Outcome::Applied);
    check("assign T3 @stale-v (agent B) -> conflict", apply(&log, "T3", &Act::Assign("U2"), v, "a5b"), Outcome::Conflict);

    // 6. Idempotent retry: same idempotency key short-circuits to the original
    //    ack (Applied) and appends nothing — even at a now-stale token.
    let v = log.head_offset("T3");
    let before = log.len("T3");
    check("assign T3 (idem X) -> apply", apply(&log, "T3", &Act::Assign("U3"), v, "idemX"), Outcome::Applied);
    check("assign T3 (idem X retry) -> apply(replay)", apply(&log, "T3", &Act::Assign("U3"), v, "idemX"), Outcome::Applied);
    let idem_ok = log.len("T3") == before + 1;
    println!("  {:46} len {}->{}  {}", "idempotent retry appends once", before, log.len("T3"), if idem_ok { "✓" } else { "✗" });
    sc.add(idem_ok);

    // 7. Concurrency: N agents race to resolve a fresh ticket; EXACTLY one wins.
    let shared = std::sync::Arc::new(CasLog::new());
    shared.cas_append("R", -1, vec![serde_json::json!({"kind":"status","value":"open"})], "seed-R").unwrap();
    let head = shared.head_offset("R");
    let mut handles = Vec::new();
    for i in 0..12 {
        let log = std::sync::Arc::clone(&shared);
        handles.push(std::thread::spawn(move || apply(&log, "R", &Act::Resolve, head, &format!("race-{i}"))));
    }
    let outcomes: Vec<Outcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = outcomes.iter().filter(|o| **o == Outcome::Applied).count();
    // A loser is EITHER Conflict (lost the CAS on a stale token) OR Rejected
    // (checked the precondition after the winner already resolved the ticket, so
    // the transition is no longer legal). Both uphold the lost-update guard —
    // the invariant is simply: exactly one Applied, and no double-append.
    let losers = outcomes.iter().filter(|o| **o != Outcome::Applied).count();
    let conflicts = outcomes.iter().filter(|o| **o == Outcome::Conflict).count();
    let rejects = outcomes.iter().filter(|o| **o == Outcome::Rejected).count();
    let race_ok = wins == 1 && losers == 11 && shared.len("R") == 2; // seed + exactly 1 resolve
    println!(
        "  {:46} wins={} losers={} (conflict={} reject={})  {}",
        "12 agents race to resolve -> exactly 1 wins", wins, losers, conflicts, rejects,
        if race_ok { "✓" } else { "✗" }
    );
    sc.add(race_ok);

    println!("  TIER B OVERALL: {}/{}  = {:.3}", sc.pass, sc.total, sc.rate());

    assert_eq!(sc.pass, sc.total, "every validated-action case must match its expected outcome");
}
