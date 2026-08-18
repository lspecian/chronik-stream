//! ObjectType schema — the user-declared definition of an Ontology object type.
//!
//! Stored (JSON) in the compacted `ont.types.{tenant}` topic and materialized
//! into the registry ([`crate::ont_types_consumer`]). Phase O-0 dogfoods the
//! memory domain: an ObjectType binds to an existing `mem.*` projection (its
//! [`BackingBinding`]) and declares which field carries its identity.

use serde::{Deserialize, Serialize};

/// The value type an attribute may hold. Intentionally small for O-0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttrType {
    String,
    Number,
    Bool,
    Timestamp,
}

/// One attribute of an ObjectType.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttrSpec {
    /// Attribute name as seen by agents (e.g. "degree", "employer").
    pub name: String,
    #[serde(rename = "type")]
    pub attr_type: AttrType,
    /// The value in the backing projection's `predicate` field that supplies
    /// this attribute (memory dogfood: a fact with predicate="has_degree" maps
    /// to the "degree" attribute). `None` = match by attribute `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_predicate: Option<String>,
    /// Whether the attribute may hold multiple values (list) vs a single value.
    #[serde(default)]
    pub multi: bool,
}

/// How an instance's identity is derived. O-0 is deterministic-only; fuzzy
/// entity resolution is deferred (roadmap §9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdentitySpec {
    /// The backing-projection field whose value is the object id (memory
    /// dogfood: "subject" — a fact's subject is the entity id).
    pub id_field: String,
    /// Deterministic canonicalization only: lowercase + trim the id before
    /// matching. No embedding/LLM resolution in O-0.
    #[serde(default = "default_true")]
    pub normalize: bool,
}

/// Which existing projection backs this ObjectType and whether it is safe for
/// point-in-time reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackingBinding {
    /// Logical projection topic *prefix*; the tenant/namespace is appended at
    /// resolution time (e.g. "mem.fact" -> "mem.fact.{namespace}").
    pub topic_prefix: String,
    /// `true` = append-only backing (safe for offset/time `as_of`).
    /// `false` = compacted backing (superseded history may be gone -> `as_of`
    /// unreliable). Roadmap §9: point-in-time is only reliable on append-only.
    #[serde(default)]
    pub append_only: bool,
}

/// A user-declared Object Type — the unit stored in `ont.types.{tenant}` and
/// held in the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectType {
    /// Type name, unique within a tenant (e.g. "Entity", "Repository").
    pub type_name: String,
    pub attributes: Vec<AttrSpec>,
    pub identity: IdentitySpec,
    pub backing: BackingBinding,
    /// Free-form human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample ObjectType used across the crate's tests: the memory-dogfood
    /// `Entity`, resolved from `mem.fact` by `subject`.
    pub(crate) fn sample_entity() -> ObjectType {
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
                    name: "employer".to_string(),
                    attr_type: AttrType::String,
                    from_predicate: Some("works_at".to_string()),
                    multi: false,
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
            description: Some("A person/place/thing the memory layer extracted facts about.".to_string()),
        }
    }

    #[test]
    fn object_type_json_round_trip() {
        let ty = sample_entity();
        let json = serde_json::to_vec(&ty).unwrap();
        let back: ObjectType = serde_json::from_slice(&json).unwrap();
        assert_eq!(ty, back);
    }

    #[test]
    fn identity_normalize_defaults_true() {
        // A body that omits `normalize` should default it to true.
        let json = r#"{
            "type_name":"Entity",
            "attributes":[],
            "identity":{"id_field":"subject"},
            "backing":{"topic_prefix":"mem.fact"}
        }"#;
        let ty: ObjectType = serde_json::from_str(json).unwrap();
        assert!(ty.identity.normalize);
        assert!(!ty.backing.append_only); // defaults false
    }

    #[test]
    fn attr_type_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AttrType::Timestamp).unwrap(), "\"timestamp\"");
    }
}
