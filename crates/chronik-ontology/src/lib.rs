//! Chronik Ontology — the event-native Ontology layer for agents.
//!
//! Phase **O-0 (Object Types)**: user-declared [`ObjectType`]s materialized from
//! existing Chronik projections with identity + provenance + as-of. See
//! `docs/ROADMAP_ONTOLOGY.md`.
//!
//! Boundary discipline (roadmap §9 / AD-1): the Ontology is a layer ON Chronik —
//! this crate plus a Unified-API mount under `/ontology/v1/*` — never domain
//! semantics inside `chronik-server`.
//!
//! Milestone A (this module set): the [`OntTypeIndex`] registry, the 7th instance
//! of the "consumer-maintained keyed in-memory index rebuilt from a compacted
//! Kafka topic" pattern (cf. `chronik-memory/src/mem_config_consumer.rs`).

pub mod cas;
pub mod edge_index;
pub mod object_type;
pub mod ont_types_consumer;
pub mod resolve;
pub mod traverse;

pub use cas::{ActionApply, AppendAck, ApplyOutcome, CasConflict, CasLog};

pub use edge_index::{
    apply_edge_event, parse_edge_from_fact, run_edge_consumer, spawn_edge_consumer, EdgeApply,
    EdgeConsumerConfig, EdgeEvent, EdgeStats, IndexedEdge, ParseError as EdgeParseError,
    RelationshipIndex, MEM_FACT_TOPICS_REGEX,
};

pub use object_type::{AttrSpec, AttrType, BackingBinding, IdentitySpec, ObjectType};
pub use resolve::{
    assemble_all_instances, assemble_entity, assemble_instance, filter_as_of, query_objects,
    resolve_entity, resolve_object, EntityView, ObjectInstance, ResolveError, ResolvedAttr,
    SourceRef,
};
pub use traverse::{assemble_edges, traverse, Edge, TraverseError};
pub use ont_types_consumer::{
    apply_event, parse_ont_type_record, run_consumer, spawn_ont_types_consumer, OntTypeApply,
    OntTypeEvent, OntTypeIndex, OntTypeKey, OntTypeRecordEnvelope, OntTypeStats,
    OntTypesConsumerConfig, ParseError as OntTypeParseError, ONT_TYPES_TOPICS_REGEX,
};
