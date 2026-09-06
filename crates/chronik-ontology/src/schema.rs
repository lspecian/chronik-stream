//! The developer-facing **Ontology SDK schema** — one declarative file that
//! defines a namespace's object types + link types, applied to a running broker
//! with `chronik-server ontology apply`.
//!
//! Consumption is already MCP-native (`/ontology/v1/mcp`); this module is the
//! **authoring** half of the SDK: it parses a friendly YAML/JSON schema, validates
//! it, and converts it into the exact keyed wire records the registry consumers
//! read — `ont.types.{tenant}` ([`crate::ont_types_consumer`]),
//! `ont.links.{tenant}` ([`crate::link_type`]), and the `mem.fact.{tenant}` facts
//! the edge index + resolver read. Everything here is pure and I/O-free; the CLI
//! layer produces the returned records to Kafka.
//!
//! ```yaml
//! schema_version: 1
//! namespace: issues
//! object_types:
//!   - name: Ticket
//!     attributes:
//!       - { name: status,   from_predicate: status }
//!       - { name: assignee, from_predicate: assignee }
//!     identity: { field: subject }
//! link_types:
//!   - { name: blocked_by, inverse: blocks }
//!   - { name: parent_of,  inverse: subtask_of }
//! ```

use serde::{Deserialize, Serialize};

use crate::link_type::LinkType;
use crate::object_type::{AttrSpec, AttrType, BackingBinding, IdentitySpec, ObjectType};

/// Current on-the-wire envelope version (matches the consumers' default).
pub const SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}
fn default_attr_type() -> AttrType {
    AttrType::String
}
fn default_id_field() -> String {
    "subject".to_string()
}
fn default_topic_prefix() -> String {
    "mem.fact".to_string()
}
fn default_true() -> bool {
    true
}

/// A parse or validation failure, surfaced to the CLI with a human message.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("failed to parse schema: {0}")]
    Parse(String),
    #[error("invalid schema: {0}")]
    Invalid(String),
}

/// One keyed record to publish: `(topic, key, value)` — the value is compact JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OntRecord {
    pub topic: String,
    pub key: String,
    pub value: String,
}

/// The whole declarative schema for one namespace.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OntologySchema {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Namespace these definitions belong to. Topics are per-**tenant** (the
    /// segment before the first `:`); a colon-free namespace is its own tenant.
    pub namespace: String,
    #[serde(default)]
    pub object_types: Vec<SchemaObjectType>,
    #[serde(default)]
    pub link_types: Vec<SchemaLinkType>,
}

/// A user-declared object type (friendly names; maps to [`ObjectType`]).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaObjectType {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub attributes: Vec<SchemaAttribute>,
    #[serde(default)]
    pub identity: SchemaIdentity,
    #[serde(default)]
    pub backing: SchemaBacking,
}

/// One attribute of an object type.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaAttribute {
    pub name: String,
    #[serde(rename = "type", default = "default_attr_type")]
    pub attr_type: AttrType,
    /// The backing `predicate` that supplies this attribute; defaults to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_predicate: Option<String>,
    #[serde(default)]
    pub multi: bool,
}

/// How an instance's identity is derived (maps to [`IdentitySpec`]).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaIdentity {
    #[serde(default = "default_id_field")]
    pub field: String,
    #[serde(default = "default_true")]
    pub normalize: bool,
}
impl Default for SchemaIdentity {
    fn default() -> Self {
        Self { field: default_id_field(), normalize: true }
    }
}

/// Which projection backs the type (maps to [`BackingBinding`]).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaBacking {
    #[serde(default = "default_topic_prefix")]
    pub topic_prefix: String,
    #[serde(default)]
    pub append_only: bool,
}
impl Default for SchemaBacking {
    fn default() -> Self {
        Self { topic_prefix: default_topic_prefix(), append_only: false }
    }
}

/// A user-declared relation (maps to [`LinkType`]). `predicate` defaults to `name`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaLinkType {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One fact to ingest — a `(subject, predicate, object)` triple with optional
/// bi-temporal validity. Read from a JSONL file by the `ingest` CLI.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FactInput {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    /// RFC3339 timestamp the fact becomes valid (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    /// RFC3339 timestamp the fact stops being valid (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
}

/// Parse a schema from YAML **or** JSON text (JSON is a YAML subset).
pub fn parse_schema(text: &str) -> Result<OntologySchema, SchemaError> {
    serde_yaml::from_str(text).map_err(|e| SchemaError::Parse(e.to_string()))
}

/// Parse one line of the facts JSONL file.
pub fn parse_fact_line(line: &str) -> Result<FactInput, SchemaError> {
    serde_json::from_str(line).map_err(|e| SchemaError::Parse(e.to_string()))
}

impl SchemaObjectType {
    fn to_object_type(&self) -> ObjectType {
        ObjectType {
            type_name: self.name.clone(),
            attributes: self
                .attributes
                .iter()
                .map(|a| AttrSpec {
                    name: a.name.clone(),
                    attr_type: a.attr_type,
                    from_predicate: a.from_predicate.clone(),
                    multi: a.multi,
                })
                .collect(),
            identity: IdentitySpec { id_field: self.identity.field.clone(), normalize: self.identity.normalize },
            backing: BackingBinding {
                topic_prefix: self.backing.topic_prefix.clone(),
                append_only: self.backing.append_only,
            },
            description: self.description.clone(),
        }
    }
}

impl SchemaLinkType {
    fn to_link_type(&self) -> LinkType {
        LinkType {
            name: self.name.clone(),
            // The relation name IS the predicate unless overridden.
            predicate: self.predicate.clone().unwrap_or_else(|| self.name.clone()),
            inverse: self.inverse.clone(),
            description: self.description.clone(),
        }
    }
}

impl OntologySchema {
    /// Tenant = the namespace up to the first `:`. Topics are per-tenant; a
    /// record's full namespace is stamped on facts. Colon-free namespace == tenant.
    pub fn tenant(&self) -> &str {
        self.namespace.split(':').next().unwrap_or(&self.namespace)
    }

    /// Validate structural + referential rules. Returns the first violation.
    pub fn validate(&self) -> Result<(), SchemaError> {
        let inv = |m: String| Err(SchemaError::Invalid(m));

        if self.namespace.trim().is_empty() {
            return inv("namespace must not be empty".into());
        }
        // Topic segments cannot contain '.', so neither can the namespace/tenant.
        if self.namespace.contains('.') {
            return inv(format!("namespace {:?} must not contain '.'", self.namespace));
        }
        if self.tenant().is_empty() {
            return inv(format!("namespace {:?} has an empty tenant (leading ':')", self.namespace));
        }
        if self.object_types.is_empty() && self.link_types.is_empty() {
            return inv("schema defines no object_types and no link_types".into());
        }

        // Object types: unique names, non-empty identity/backing, unique attrs.
        let mut type_names = std::collections::HashSet::new();
        for ot in &self.object_types {
            if ot.name.trim().is_empty() {
                return inv("an object_type has an empty name".into());
            }
            if !type_names.insert(ot.name.as_str()) {
                return inv(format!("duplicate object_type name {:?}", ot.name));
            }
            if ot.identity.field.trim().is_empty() {
                return inv(format!("object_type {:?}: identity.field must not be empty", ot.name));
            }
            if ot.backing.topic_prefix.trim().is_empty() {
                return inv(format!("object_type {:?}: backing.topic_prefix must not be empty", ot.name));
            }
            let mut attrs = std::collections::HashSet::new();
            for a in &ot.attributes {
                if a.name.trim().is_empty() {
                    return inv(format!("object_type {:?} has an attribute with an empty name", ot.name));
                }
                if !attrs.insert(a.name.as_str()) {
                    return inv(format!("object_type {:?}: duplicate attribute {:?}", ot.name, a.name));
                }
            }
        }

        // Link types: every relation name AND inverse must be globally unique,
        // because the registry resolves both a forward and an inverse name.
        let mut rel_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for lt in &self.link_types {
            if lt.name.trim().is_empty() {
                return inv("a link_type has an empty name".into());
            }
            if let Some(p) = &lt.predicate {
                if p.trim().is_empty() {
                    return inv(format!("link_type {:?}: predicate must not be empty when set", lt.name));
                }
            }
            if !rel_names.insert(lt.name.clone()) {
                return inv(format!("duplicate relation name {:?} (a name or inverse is used twice)", lt.name));
            }
            if let Some(invn) = &lt.inverse {
                if invn.trim().is_empty() {
                    return inv(format!("link_type {:?}: inverse must not be empty when set", lt.name));
                }
                if invn == &lt.name {
                    return inv(format!("link_type {:?}: inverse must differ from name", lt.name));
                }
                if !rel_names.insert(invn.clone()) {
                    return inv(format!("relation name {:?} collides (used as another name or inverse)", invn));
                }
            }
        }
        Ok(())
    }

    /// The keyed `ont.types.{tenant}` records for every object type.
    pub fn object_type_records(&self) -> Vec<OntRecord> {
        let topic = format!("ont.types.{}", self.tenant());
        self.object_types
            .iter()
            .map(|ot| {
                let value = serde_json::json!({
                    "schema_version": SCHEMA_VERSION,
                    "object_type": ot.to_object_type(),
                });
                OntRecord { topic: topic.clone(), key: ot.name.clone(), value: value.to_string() }
            })
            .collect()
    }

    /// The keyed `ont.links.{tenant}` records for every link type.
    pub fn link_type_records(&self) -> Vec<OntRecord> {
        let topic = format!("ont.links.{}", self.tenant());
        self.link_types
            .iter()
            .map(|lt| {
                let value = serde_json::json!({
                    "schema_version": SCHEMA_VERSION,
                    "link_type": lt.to_link_type(),
                });
                OntRecord { topic: topic.clone(), key: lt.name.clone(), value: value.to_string() }
            })
            .collect()
    }
}

/// Build one `mem.fact.{tenant}` record from a fact triple. `offset` is a
/// monotonic synthetic provenance offset (position in the ingest file).
pub fn fact_record(namespace: &str, tenant: &str, f: &FactInput, offset: i64) -> OntRecord {
    let topic = format!("mem.fact.{tenant}");
    let key = format!("{}|{}|{}", f.subject, f.predicate, f.object);
    let mut value = serde_json::json!({
        "namespace": namespace,
        "key": format!("{}|{}", f.subject, f.predicate),
        "confidence": 1.0,
        "source": {
            "topic": format!("mem.raw.{tenant}"),
            "offsets": [offset],
            "extractor": "ontology-sdk@1",
        },
        "type": "fact",
        "body": {
            "subject": f.subject,
            "predicate": f.predicate,
            "object": f.object,
            "text": format!("{} {} {}", f.subject, f.predicate, f.object),
        },
    });
    if let Some(vf) = &f.valid_from {
        value["valid_from"] = serde_json::json!(vf);
    }
    if let Some(vt) = &f.valid_to {
        value["valid_to"] = serde_json::json!(vt);
    }
    OntRecord { topic, key, value: value.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge_index::{parse_edge_from_fact, EdgeEvent};
    use crate::link_type::parse_ont_link_record;
    use crate::ont_types_consumer::{parse_ont_type_record, OntTypeEvent};

    const SAMPLE: &str = r#"
schema_version: 1
namespace: issues
object_types:
  - name: Ticket
    description: A unit of work.
    attributes:
      - { name: status,   from_predicate: status }
      - { name: assignee, from_predicate: assignee }
      - { name: project,  type: string }
    identity: { field: subject }
    backing: { topic_prefix: mem.fact, append_only: false }
link_types:
  - { name: blocked_by, inverse: blocks, description: "A is blocked by B." }
  - { name: parent_of,  inverse: subtask_of }
"#;

    #[test]
    fn parses_and_validates_sample() {
        let s = parse_schema(SAMPLE).unwrap();
        s.validate().unwrap();
        assert_eq!(s.tenant(), "issues");
        assert_eq!(s.object_types.len(), 1);
        assert_eq!(s.link_types.len(), 2);
    }

    #[test]
    fn attribute_and_identity_defaults() {
        // `type` omitted -> string; identity omitted -> field "subject", normalize true.
        let s = parse_schema("namespace: n\nobject_types:\n  - name: T\n    attributes:\n      - { name: a }\n").unwrap();
        let ot = &s.object_types[0];
        assert_eq!(ot.attributes[0].attr_type, AttrType::String);
        assert_eq!(ot.identity.field, "subject");
        assert!(ot.identity.normalize);
        assert_eq!(ot.backing.topic_prefix, "mem.fact");
    }

    #[test]
    fn object_type_record_is_consumable_by_the_registry() {
        // The honest proof: SDK output parses through the REAL registry consumer.
        let s = parse_schema(SAMPLE).unwrap();
        let recs = s.object_type_records();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.topic, "ont.types.issues");
        assert_eq!(r.key, "Ticket");
        let ev = parse_ont_type_record(&r.topic, r.key.as_bytes(), Some(r.value.as_bytes())).unwrap();
        match ev {
            OntTypeEvent::Upsert { tenant, object_type } => {
                assert_eq!(tenant, "issues");
                assert_eq!(object_type.type_name, "Ticket");
                assert_eq!(object_type.attributes.len(), 3);
                assert_eq!(object_type.attributes[0].from_predicate.as_deref(), Some("status"));
            }
            other => panic!("expected Upsert, got {other:?}"),
        }
    }

    #[test]
    fn link_type_record_is_consumable_and_predicate_defaults_to_name() {
        let s = parse_schema(SAMPLE).unwrap();
        let recs = s.link_type_records();
        assert_eq!(recs.len(), 2);
        let r = &recs[0];
        assert_eq!(r.topic, "ont.links.issues");
        assert_eq!(r.key, "blocked_by");
        let ev = parse_ont_link_record(&r.topic, r.key.as_bytes(), Some(r.value.as_bytes())).unwrap();
        match ev {
            crate::link_type::OntLinkEvent::Upsert { tenant, link_type } => {
                assert_eq!(tenant, "issues");
                assert_eq!(link_type.name, "blocked_by");
                assert_eq!(link_type.predicate, "blocked_by"); // defaulted to name
                assert_eq!(link_type.inverse.as_deref(), Some("blocks"));
            }
            other => panic!("expected Upsert, got {other:?}"),
        }
    }

    #[test]
    fn fact_record_is_consumable_as_an_edge() {
        let f = FactInput {
            subject: "C0_5".into(),
            predicate: "blocked_by".into(),
            object: "C0_4".into(),
            valid_from: Some("2026-01-01T00:00:00Z".into()),
            valid_to: None,
        };
        let r = fact_record("issues", "issues", &f, 7);
        assert_eq!(r.topic, "mem.fact.issues");
        let ev = parse_edge_from_fact(&r.topic, Some(r.value.as_bytes())).unwrap();
        match ev {
            Some(EdgeEvent::Upsert(edge)) => {
                assert_eq!(edge.namespace, "issues");
                assert_eq!(edge.from, "C0_5");
                assert_eq!(edge.edge_type, "blocked_by");
                assert_eq!(edge.to, "C0_4");
                assert!(edge.valid_from.is_some());
            }
            other => panic!("expected an edge Upsert, got {other:?}"),
        }
    }

    #[test]
    fn tenant_is_the_segment_before_the_colon() {
        let s = parse_schema("namespace: acme:agent1\nlink_types:\n  - { name: r }\n").unwrap();
        assert_eq!(s.tenant(), "acme");
        assert_eq!(s.link_type_records()[0].topic, "ont.links.acme");
    }

    #[test]
    fn rejects_empty_namespace() {
        let s = parse_schema("namespace: \"\"\nlink_types:\n  - { name: r }\n").unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_namespace_with_dot() {
        let s = parse_schema("namespace: a.b\nlink_types:\n  - { name: r }\n").unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_object_type() {
        let s = parse_schema("namespace: n\nobject_types:\n  - name: T\n  - name: T\n").unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_attribute() {
        let s = parse_schema("namespace: n\nobject_types:\n  - name: T\n    attributes:\n      - { name: a }\n      - { name: a }\n").unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_inverse_equal_to_name() {
        let s = parse_schema("namespace: n\nlink_types:\n  - { name: r, inverse: r }\n").unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_relation_name_collision() {
        // `blocks` is both an inverse and a forward name.
        let s = parse_schema("namespace: n\nlink_types:\n  - { name: blocked_by, inverse: blocks }\n  - { name: blocks }\n").unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_empty_schema() {
        let s = parse_schema("namespace: n\n").unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn parses_json_too() {
        // JSON is a YAML subset, so the same parser accepts `.json`.
        let s = parse_schema(r#"{"namespace":"n","link_types":[{"name":"r"}]}"#).unwrap();
        s.validate().unwrap();
        assert_eq!(s.tenant(), "n");
    }
}
