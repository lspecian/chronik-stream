//! Instance resolution — materialize an [`ObjectType`] instance from its backing
//! projection, on demand, with provenance. Milestone B (`get_object`).
//!
//! Reuses Chronik's `/_search` endpoint (the same `_source` shape `recall.rs`
//! parses): query the backing `mem.fact.{namespace}` topic, filter hits to the
//! requested identity, and map each fact's `predicate` to a declared attribute —
//! citing the fact's `source.{topic,offsets}` as the attribute's provenance.
//!
//! The HTTP call is a thin shell around [`assemble_instance`], which is pure and
//! unit-tested without a broker.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::object_type::ObjectType;

/// A source-event citation for a resolved attribute value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    pub topic: String,
    pub offsets: Vec<i64>,
}

/// One resolved attribute of an object instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedAttr {
    pub name: String,
    /// Value(s). Single-valued attributes keep the most recent; multi keep all.
    pub values: Vec<serde_json::Value>,
    /// The source event(s) that justify this attribute (the provenance cite).
    pub provenance: Vec<SourceRef>,
}

/// A materialized object instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectInstance {
    pub type_name: String,
    pub id: String,
    pub namespace: String,
    pub attributes: Vec<ResolvedAttr>,
    /// How many backing records contributed (diagnostic).
    pub backing_records: usize,
}

/// Resolution errors.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("backing search request failed: {0}")]
    Http(String),
    #[error("backing search returned status {0}")]
    Status(u16),
    #[error("backing search response unparseable: {0}")]
    BadResponse(String),
}

// ---- /_search wire shapes (mirror recall.rs) --------------------------------

#[derive(Deserialize)]
struct SearchResponse {
    hits: HitsInfo,
}
#[derive(Deserialize)]
struct HitsInfo {
    hits: Vec<Hit>,
}
#[derive(Deserialize)]
struct Hit {
    #[serde(rename = "_source")]
    source: serde_json::Value,
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Extract the memory envelope object from a search hit `_source` — tolerates the
/// **direct** shape (envelope fields present) and the **wrapped** shape (the
/// envelope JSON is a string under `value`/`_value`/`_json_content`).
fn envelope_from_source(source: &serde_json::Value) -> Option<serde_json::Value> {
    if source.get("body").is_some() || source.get("type").is_some() {
        return Some(source.clone());
    }
    for f in ["value", "_value", "_json_content"] {
        if let Some(s) = source.get(f).and_then(|v| v.as_str()) {
            if let Ok(inner) = serde_json::from_str::<serde_json::Value>(s) {
                return Some(inner);
            }
        }
    }
    None
}

/// PURE point-in-time pre-filter: keep only records effective at or before
/// `as_of` (by their `valid_from`). Records with a missing/unparseable
/// `valid_from` are kept (lenient). `None` = no filter.
///
/// **Correctness scope (roadmap §9):** this is exact only over **append-only**
/// backing, where every version is retained. Over a **compacted** backing
/// (e.g. `mem.fact`), superseded versions may be physically gone, so an
/// `as_of` in the past is best-effort — it cannot resurrect history that
/// compaction erased. The cross-projection consistent-snapshot token is
/// deferred (a single-projection read here).
pub fn filter_as_of(sources: Vec<serde_json::Value>, as_of: Option<DateTime<Utc>>) -> Vec<serde_json::Value> {
    let Some(t) = as_of else {
        return sources;
    };
    let parse = |env: &serde_json::Value, field: &str| -> Option<DateTime<Utc>> {
        env.get(field)
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
    };
    sources
        .into_iter()
        .filter(|src| {
            let Some(env) = envelope_from_source(src) else {
                return true; // can't inspect -> keep (lenient)
            };
            // Bi-temporal: effective at `t` iff valid_from <= t < valid_to.
            // valid_to = None means still valid (the roadmap's invalidate-not-
            // delete model — a superseded/expired edge sets valid_to instead of
            // being removed).
            let after_start = parse(&env, "valid_from").map(|vf| vf <= t).unwrap_or(true);
            let before_end = parse(&env, "valid_to").map(|vt| t < vt).unwrap_or(true);
            after_start && before_end
        })
        .collect()
}

/// PURE assembly: given the raw `_source` values of backing fact records, filter
/// to the requested identity and map predicates to the type's declared
/// attributes, carrying provenance. Returns `None` when nothing matches.
pub fn assemble_instance(
    sources: &[serde_json::Value],
    namespace: &str,
    ty: &ObjectType,
    id: &str,
) -> Option<ObjectInstance> {
    let want_id = if ty.identity.normalize {
        normalize(id)
    } else {
        id.to_string()
    };

    // predicate -> (values, provenance), insertion-ordered by first sight.
    use std::collections::BTreeMap;
    let mut by_pred: BTreeMap<String, (Vec<serde_json::Value>, Vec<SourceRef>)> = BTreeMap::new();
    let mut contributing = 0usize;

    for src in sources {
        let Some(env) = envelope_from_source(src) else {
            continue;
        };
        // Skip tombstones.
        if env.get("tombstoned").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        // Namespace isolation: `mem.fact.{tenant}` can hold facts from several
        // namespaces under the tenant, distinguished by the record's
        // `namespace` field. Require a match when the field is present.
        if let Some(rec_ns) = env.get("namespace").and_then(|v| v.as_str()) {
            if rec_ns != namespace {
                continue;
            }
        }
        // Fact fields live under the flattened `body`; fall back to the envelope
        // itself if the search layer already unwrapped them.
        let fbody = env.get("body").unwrap_or(&env);
        let Some(subject) = fbody.get(&ty.identity.id_field).and_then(|v| v.as_str()) else {
            continue;
        };
        let subj_key = if ty.identity.normalize {
            normalize(subject)
        } else {
            subject.to_string()
        };
        if subj_key != want_id {
            continue;
        }
        let predicate = fbody
            .get("predicate")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let object = fbody.get("object").cloned().unwrap_or(serde_json::Value::Null);
        let prov = env.get("source").map(|s| SourceRef {
            topic: s
                .get("topic")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            offsets: s
                .get("offsets")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|o| o.as_i64()).collect())
                .unwrap_or_default(),
        });
        let entry = by_pred.entry(predicate).or_default();
        entry.0.push(object);
        if let Some(p) = prov {
            entry.1.push(p);
        }
        contributing += 1;
    }

    if contributing == 0 {
        return None;
    }

    // Map matched predicates onto the declared attributes (declaration order).
    let mut attributes = Vec::new();
    for attr in &ty.attributes {
        let pred = attr.from_predicate.as_deref().unwrap_or(&attr.name);
        if let Some((values, provenance)) = by_pred.get(pred) {
            let vals = if attr.multi {
                values.clone()
            } else {
                // single-valued: keep the most recent contribution
                values.last().cloned().into_iter().collect()
            };
            attributes.push(ResolvedAttr {
                name: attr.name.clone(),
                values: vals,
                provenance: provenance.clone(),
            });
        }
    }

    Some(ObjectInstance {
        type_name: ty.type_name.clone(),
        id: id.to_string(),
        namespace: namespace.to_string(),
        attributes,
        backing_records: contributing,
    })
}

/// PURE: assemble EVERY instance present in the backing records — the distinct
/// identity values (by `identity.id_field`), each resolved to a full instance.
/// (O-2 `query_objects`, read side.) One scan finds the distinct ids; each is
/// then assembled from the same record set.
pub fn assemble_all_instances(
    sources: &[serde_json::Value],
    namespace: &str,
    ty: &ObjectType,
) -> Vec<ObjectInstance> {
    let mut seen = std::collections::HashSet::new();
    let mut ids: Vec<String> = Vec::new();
    for src in sources {
        let Some(env) = envelope_from_source(src) else {
            continue;
        };
        if env.get("tombstoned").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if let Some(rec_ns) = env.get("namespace").and_then(|v| v.as_str()) {
            if rec_ns != namespace {
                continue;
            }
        }
        let fbody = env.get("body").unwrap_or(&env);
        let Some(subject) = fbody.get(&ty.identity.id_field).and_then(|v| v.as_str()) else {
            continue;
        };
        let key = if ty.identity.normalize {
            normalize(subject)
        } else {
            subject.to_string()
        };
        if seen.insert(key) {
            ids.push(subject.to_string());
        }
    }
    ids.into_iter()
        .filter_map(|id| assemble_instance(sources, namespace, ty, &id))
        .collect()
}

/// List instances of a type from its backing projection (O-2 `query_objects`,
/// read side). `max_facts` bounds the scan; `as_of` applies point-in-time.
pub async fn query_objects(
    http: &reqwest::Client,
    api_base: &str,
    namespace: &str,
    ty: &ObjectType,
    max_facts: usize,
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<ObjectInstance>, ResolveError> {
    let tenant = namespace.split(':').next().unwrap_or(namespace);
    let topic = format!("{}.{}", ty.backing.topic_prefix, tenant);
    // Enumerate the namespace's records: every fact carries its `namespace` in
    // the tokenized `_all`, so matching the namespace returns them all (the
    // broker has no match_all). assemble_all_instances then exact-filters by
    // namespace, so a loose superset here is harmless.
    let req = serde_json::json!({
        "index": topic,
        "size": max_facts,
        "query": {"match": {"_all": namespace}}
    });
    let url = format!("{}/_search", api_base.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&req)
        .send()
        .await
        .map_err(|e| ResolveError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        if resp.status().as_u16() == 404 {
            return Ok(vec![]);
        }
        return Err(ResolveError::Status(resp.status().as_u16()));
    }
    let parsed: SearchResponse = resp
        .json()
        .await
        .map_err(|e| ResolveError::BadResponse(e.to_string()))?;
    let sources = filter_as_of(
        parsed.hits.hits.into_iter().map(|h| h.source).collect(),
        as_of,
    );
    Ok(assemble_all_instances(&sources, namespace, ty))
}

/// Resolve one object instance by querying its backing projection over `/_search`.
///
/// `api_base` is the Unified API base (e.g. `http://localhost:6092`). Returns
/// `Ok(None)` when the backing topic is absent (404) or nothing matches the id.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_object(
    http: &reqwest::Client,
    api_base: &str,
    namespace: &str,
    ty: &ObjectType,
    id: &str,
    max_facts: usize,
    as_of: Option<DateTime<Utc>>,
) -> Result<Option<ObjectInstance>, ResolveError> {
    // The typed memory topics are keyed by TENANT — the first ':'-segment of the
    // namespace (`agent:x:user:y` -> tenant `agent`; a colon-free namespace is
    // its own tenant). One topic can hold several namespaces, which
    // `assemble_instance` filters on the record's `namespace` field.
    let tenant = namespace.split(':').next().unwrap_or(namespace);
    let topic = format!("{}.{}", ty.backing.topic_prefix, tenant);
    // BM25 recall on the id narrows the candidate set; `assemble_instance` then
    // applies the EXACT identity filter, so a loose match here is harmless.
    let req = serde_json::json!({
        "index": topic,
        "size": max_facts,
        "query": {"match": {"_all": id}}
    });
    let url = format!("{}/_search", api_base.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&req)
        .send()
        .await
        .map_err(|e| ResolveError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        if resp.status().as_u16() == 404 {
            return Ok(None); // backing topic not created yet
        }
        return Err(ResolveError::Status(resp.status().as_u16()));
    }
    let parsed: SearchResponse = resp
        .json()
        .await
        .map_err(|e| ResolveError::BadResponse(e.to_string()))?;
    let sources: Vec<serde_json::Value> = parsed.hits.hits.into_iter().map(|h| h.source).collect();
    let sources = filter_as_of(sources, as_of);
    Ok(assemble_instance(&sources, namespace, ty, id))
}

/// A schema-less, open-domain view of an entity: EVERY predicate asserted about
/// the subject, with its object value(s) and provenance. This is the
/// "domain-agnostic Memory" case (roadmap O-0 tagline) — unlike
/// [`assemble_instance`], which emits only the attributes a declared
/// [`ObjectType`] names, this passes through *all* predicates, so an entity can
/// be resolved with no pre-declared schema (needed for open-domain corpora like
/// LongMemEval where predicates are not known ahead of time).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityView {
    pub id: String,
    pub namespace: String,
    /// One entry per predicate; `name` is the predicate. Reuses [`ResolvedAttr`]
    /// so provenance carries through identically.
    pub triples: Vec<ResolvedAttr>,
    /// How many backing records contributed (diagnostic).
    pub backing_records: usize,
}

/// PURE open-domain assembly: gather every `(predicate -> objects)` asserted
/// about `id` in the backing records, carrying provenance. Same identity +
/// namespace-isolation + tombstone rules as [`assemble_instance`], but emits
/// all predicates rather than mapping to declared attributes. `None` when
/// nothing matches.
pub fn assemble_entity(
    sources: &[serde_json::Value],
    namespace: &str,
    id_field: &str,
    id: &str,
    normalize_id: bool,
) -> Option<EntityView> {
    let want_id = if normalize_id { normalize(id) } else { id.to_string() };
    use std::collections::BTreeMap;
    let mut by_pred: BTreeMap<String, (Vec<serde_json::Value>, Vec<SourceRef>)> = BTreeMap::new();
    let mut contributing = 0usize;

    for src in sources {
        let Some(env) = envelope_from_source(src) else {
            continue;
        };
        if env.get("tombstoned").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if let Some(rec_ns) = env.get("namespace").and_then(|v| v.as_str()) {
            if rec_ns != namespace {
                continue;
            }
        }
        let fbody = env.get("body").unwrap_or(&env);
        let Some(subject) = fbody.get(id_field).and_then(|v| v.as_str()) else {
            continue;
        };
        let subj_key = if normalize_id {
            normalize(subject)
        } else {
            subject.to_string()
        };
        if subj_key != want_id {
            continue;
        }
        let predicate = fbody
            .get("predicate")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if predicate.is_empty() {
            continue;
        }
        let object = fbody.get("object").cloned().unwrap_or(serde_json::Value::Null);
        let prov = env.get("source").map(|s| SourceRef {
            topic: s.get("topic").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            offsets: s
                .get("offsets")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|o| o.as_i64()).collect())
                .unwrap_or_default(),
        });
        let entry = by_pred.entry(predicate).or_default();
        entry.0.push(object);
        if let Some(p) = prov {
            entry.1.push(p);
        }
        contributing += 1;
    }

    if contributing == 0 {
        return None;
    }
    let triples = by_pred
        .into_iter()
        .map(|(pred, (values, provenance))| ResolvedAttr {
            name: pred,
            values,
            provenance,
        })
        .collect();
    Some(EntityView {
        id: id.to_string(),
        namespace: namespace.to_string(),
        triples,
        backing_records: contributing,
    })
}

/// Resolve one entity **open-domain** (all predicates) over the memory dogfood
/// backing (`mem.fact.{tenant}`, `id_field = subject`, normalized identity) via
/// `/_search`. Mirrors [`resolve_object`]'s tenant/topic derivation and
/// point-in-time filter. Returns `Ok(None)` on a 404 backing topic or no match.
pub async fn resolve_entity(
    http: &reqwest::Client,
    api_base: &str,
    namespace: &str,
    id: &str,
    max_facts: usize,
    as_of: Option<DateTime<Utc>>,
) -> Result<Option<EntityView>, ResolveError> {
    let tenant = namespace.split(':').next().unwrap_or(namespace);
    let topic = format!("mem.fact.{}", tenant);
    let req = serde_json::json!({
        "index": topic,
        "size": max_facts,
        "query": {"match": {"_all": id}}
    });
    let url = format!("{}/_search", api_base.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&req)
        .send()
        .await
        .map_err(|e| ResolveError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        return Err(ResolveError::Status(resp.status().as_u16()));
    }
    let parsed: SearchResponse = resp
        .json()
        .await
        .map_err(|e| ResolveError::BadResponse(e.to_string()))?;
    let sources: Vec<serde_json::Value> = parsed.hits.hits.into_iter().map(|h| h.source).collect();
    let sources = filter_as_of(sources, as_of);
    Ok(assemble_entity(&sources, namespace, "subject", id, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_type::{AttrSpec, AttrType, BackingBinding, IdentitySpec};

    fn entity_type() -> ObjectType {
        ObjectType {
            type_name: "Entity".to_string(),
            attributes: vec![
                AttrSpec {
                    name: "degree".to_string(),
                    attr_type: AttrType::String,
                    from_predicate: Some("has_degree".to_string()),
                    multi: false,
                },
                AttrSpec {
                    name: "hobbies".to_string(),
                    attr_type: AttrType::String,
                    from_predicate: Some("enjoys".to_string()),
                    multi: true,
                },
            ],
            identity: IdentitySpec {
                id_field: "subject".to_string(),
                normalize: true,
            },
            backing: BackingBinding {
                topic_prefix: "mem.fact".to_string(),
                append_only: false,
            },
            description: None,
        }
    }

    /// A `_source` in the flattened envelope shape.
    fn fact(subject: &str, predicate: &str, object: serde_json::Value, raw_offset: i64) -> serde_json::Value {
        serde_json::json!({
            "memory_id": format!("id-{predicate}-{raw_offset}"),
            "namespace": "ns1",
            "type": "fact",
            "body": {"subject": subject, "predicate": predicate, "object": object, "text": "..."},
            "source": {"topic": "mem.raw.acme.ns1.conv1", "offsets": [raw_offset], "extractor": "x@1"}
        })
    }

    #[test]
    fn resolves_attributes_with_provenance() {
        let ty = entity_type();
        let sources = vec![
            fact("Alice", "has_degree", serde_json::json!("Business Administration"), 12),
            fact("Alice", "enjoys", serde_json::json!("hiking"), 20),
            fact("Alice", "enjoys", serde_json::json!("chess"), 41),
            fact("Bob", "has_degree", serde_json::json!("Physics"), 5), // different subject, excluded
        ];
        let inst = assemble_instance(&sources, "ns1", &ty, "alice").unwrap();
        assert_eq!(inst.type_name, "Entity");
        assert_eq!(inst.backing_records, 3); // 3 Alice facts, Bob excluded

        let degree = inst.attributes.iter().find(|a| a.name == "degree").unwrap();
        assert_eq!(degree.values, vec![serde_json::json!("Business Administration")]);
        // provenance cites the source raw-turn offsets — the hard gate.
        assert_eq!(degree.provenance, vec![SourceRef { topic: "mem.raw.acme.ns1.conv1".into(), offsets: vec![12] }]);

        let hobbies = inst.attributes.iter().find(|a| a.name == "hobbies").unwrap();
        assert_eq!(hobbies.values, vec![serde_json::json!("hiking"), serde_json::json!("chess")]); // multi keeps all
        assert_eq!(hobbies.provenance.len(), 2);
    }

    #[test]
    fn every_resolved_value_carries_provenance() {
        // The ≥95% provenance-cite gate: every attribute here must have a cite.
        let ty = entity_type();
        let sources = vec![fact("Alice", "has_degree", serde_json::json!("BA"), 7)];
        let inst = assemble_instance(&sources, "ns1", &ty, "Alice").unwrap();
        assert!(inst.attributes.iter().all(|a| !a.provenance.is_empty()));
    }

    #[test]
    fn unknown_id_returns_none() {
        let ty = entity_type();
        let sources = vec![fact("Alice", "has_degree", serde_json::json!("BA"), 7)];
        assert!(assemble_instance(&sources, "ns1", &ty, "Nobody").is_none());
    }

    #[test]
    fn tombstoned_facts_are_skipped() {
        let ty = entity_type();
        let mut tomb = fact("Alice", "has_degree", serde_json::json!("BA"), 7);
        tomb["tombstoned"] = serde_json::json!(true);
        assert!(assemble_instance(&[tomb], "ns1", &ty, "Alice").is_none());
    }

    #[test]
    fn wrapped_source_shape_is_parsed() {
        // Some search responses wrap the envelope JSON as a string under `value`.
        let ty = entity_type();
        let env = fact("Alice", "has_degree", serde_json::json!("BA"), 7);
        let wrapped = serde_json::json!({ "value": serde_json::to_string(&env).unwrap() });
        let inst = assemble_instance(&[wrapped], "ns1", &ty, "Alice").unwrap();
        assert_eq!(inst.backing_records, 1);
        assert_eq!(inst.attributes[0].values, vec![serde_json::json!("BA")]);
    }

    #[test]
    fn assemble_all_instances_lists_distinct_entities() {
        let ty = entity_type();
        let sources = vec![
            fact("Alice", "has_degree", serde_json::json!("BA"), 1),
            fact("Alice", "enjoys", serde_json::json!("hiking"), 2),
            fact("Bob", "has_degree", serde_json::json!("Physics"), 3),
        ];
        let mut names: Vec<String> = assemble_all_instances(&sources, "ns1", &ty)
            .into_iter()
            .map(|i| i.id)
            .collect();
        names.sort();
        assert_eq!(names, vec!["Alice".to_string(), "Bob".to_string()]);
    }

    #[test]
    fn cross_namespace_facts_excluded() {
        // mem.fact.{tenant} may mix namespaces; a same-subject fact from another
        // namespace must not leak into this instance.
        let ty = entity_type();
        let mut other = fact("Alice", "has_degree", serde_json::json!("BA"), 7);
        other["namespace"] = serde_json::json!("different-ns");
        assert!(assemble_instance(&[other], "ns1", &ty, "Alice").is_none());
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn as_of_filters_by_valid_from() {
        let ty = entity_type();
        let mk = |obj: &str, vf: &str| {
            let mut f = fact("Alice", "enjoys", serde_json::json!(obj), 1);
            f["valid_from"] = serde_json::json!(vf);
            f
        };
        let sources = vec![
            mk("hiking", "2024-01-01T00:00:00Z"),
            mk("chess", "2024-06-01T00:00:00Z"),
        ];
        // As of March 2024: only hiking is effective yet.
        let early = filter_as_of(sources.clone(), Some(t("2024-03-01T00:00:00Z")));
        let inst = assemble_instance(&early, "ns1", &ty, "Alice").unwrap();
        let hobbies = inst.attributes.iter().find(|a| a.name == "hobbies").unwrap();
        assert_eq!(hobbies.values, vec![serde_json::json!("hiking")]);
        // As of December 2024: both.
        let late = filter_as_of(sources, Some(t("2024-12-01T00:00:00Z")));
        let inst = assemble_instance(&late, "ns1", &ty, "Alice").unwrap();
        let hobbies = inst.attributes.iter().find(|a| a.name == "hobbies").unwrap();
        assert_eq!(hobbies.values.len(), 2);
    }

    #[test]
    fn as_of_none_keeps_all_and_missing_valid_from_is_lenient() {
        // None = no filter; and a record without valid_from is kept.
        let sources = vec![fact("Alice", "has_degree", serde_json::json!("BA"), 1)];
        assert_eq!(filter_as_of(sources.clone(), None).len(), 1);
        assert_eq!(filter_as_of(sources, Some(t("2000-01-01T00:00:00Z"))).len(), 1);
    }

    #[test]
    fn as_of_respects_valid_to_invalidation() {
        let ty = entity_type();
        // Effective only 2023-01 .. 2023-12 (then invalidated via valid_to).
        let mut f = fact("Alice", "enjoys", serde_json::json!("hiking"), 1);
        f["valid_from"] = serde_json::json!("2023-01-01T00:00:00Z");
        f["valid_to"] = serde_json::json!("2023-12-31T00:00:00Z");
        // As of mid-2023: still effective.
        assert!(assemble_instance(
            &filter_as_of(vec![f.clone()], Some(t("2023-06-01T00:00:00Z"))),
            "ns1", &ty, "Alice"
        )
        .is_some());
        // As of 2024: invalidated (valid_to passed) -> excluded.
        assert!(assemble_instance(
            &filter_as_of(vec![f], Some(t("2024-06-01T00:00:00Z"))),
            "ns1", &ty, "Alice"
        )
        .is_none());
    }

    #[test]
    fn predicate_without_declared_attribute_is_ignored() {
        // A fact whose predicate maps to no declared attribute contributes to the
        // record count but produces no attribute.
        let ty = entity_type();
        let sources = vec![fact("Alice", "unmapped_predicate", serde_json::json!("x"), 3)];
        let inst = assemble_instance(&sources, "ns1", &ty, "Alice").unwrap();
        assert_eq!(inst.backing_records, 1);
        assert!(inst.attributes.is_empty());
    }

    #[test]
    fn assemble_entity_passes_through_all_predicates_with_provenance() {
        // Open-domain: EVERY predicate is emitted (no declared schema), each
        // carrying its source cite — including predicates no ObjectType names.
        let sources = vec![
            fact("Alice", "has_degree", serde_json::json!("BA"), 12),
            fact("Alice", "enjoys", serde_json::json!("hiking"), 20),
            fact("Alice", "enjoys", serde_json::json!("chess"), 41),
            fact("Alice", "works_at", serde_json::json!("Acme"), 50), // undeclared
            fact("Bob", "has_degree", serde_json::json!("Physics"), 5), // other subject
        ];
        let ev = assemble_entity(&sources, "ns1", "subject", "alice", true).unwrap();
        assert_eq!(ev.backing_records, 4); // 4 Alice facts, Bob excluded
        let preds: Vec<&str> = ev.triples.iter().map(|t| t.name.as_str()).collect();
        // BTreeMap order: sorted predicates, all present incl. undeclared works_at.
        assert_eq!(preds, vec!["enjoys", "has_degree", "works_at"]);
        let enjoys = ev.triples.iter().find(|t| t.name == "enjoys").unwrap();
        assert_eq!(enjoys.values.len(), 2); // multi objects kept
        // Every triple cites provenance (the ≥95% gate, open-domain).
        assert!(ev.triples.iter().all(|t| !t.provenance.is_empty()));
    }

    #[test]
    fn assemble_entity_unknown_id_is_none() {
        let sources = vec![fact("Alice", "has_degree", serde_json::json!("BA"), 7)];
        assert!(assemble_entity(&sources, "ns1", "subject", "Nobody", true).is_none());
    }

    #[test]
    fn assemble_entity_respects_namespace_and_tombstones() {
        // Cross-namespace and tombstoned facts must not leak into the view.
        let mut other_ns = fact("Alice", "has_degree", serde_json::json!("BA"), 7);
        other_ns["namespace"] = serde_json::json!("different-ns");
        let mut tomb = fact("Alice", "enjoys", serde_json::json!("hiking"), 8);
        tomb["tombstoned"] = serde_json::json!(true);
        assert!(assemble_entity(&[other_ns, tomb], "ns1", "subject", "Alice", true).is_none());
    }
}
