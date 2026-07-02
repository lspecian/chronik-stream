//! AM-3.5 cross-namespace ACL grants.
//!
//! Team memory: allow tenant B to *read* namespaces owned by tenant A.
//! Recall handlers with `include_grants=true` fan out across the
//! caller's own namespace patterns plus any granted patterns.
//!
//! # Wire schema
//!
//! Grants are stored on the `Tenant` record (Phase 2 tenant registry)
//! as a parallel field to `namespace_patterns`. Compaction / semantics
//! match — a grant is expressed as a glob pattern the granter's
//! `Tenant.grants_to` maps to the grantee's tenant id.
//!
//! # Semantics
//!
//! - `Tenant.read_grants: [(grantee_tenant_id, namespace_pattern)]` —
//!   "any request authenticated as `grantee_tenant_id` may READ any
//!   namespace matching `namespace_pattern` in this tenant's space".
//! - Writes are never granted cross-tenant (would break audit
//!   attribution).
//! - Recall fan-out expands from `[caller_own_patterns...]` to
//!   `[caller_own_patterns..., granted_patterns...]`, dedup on exact
//!   namespace hit.
//!
//! # Scope
//!
//! - Pure data structures + matching. Wire-up to `TenantRegistry`
//!   and the recall fan-out lives in `chronik-server`.

use crate::tenants::Tenant;
use serde::{Deserialize, Serialize};

/// One `(grantee_tenant, pattern)` grant. Attached to a `Tenant` via
/// [`TenantWithGrants::grants`]. The pattern uses the same glob
/// vocabulary as `Tenant.namespace_patterns` — `*` matches one segment,
/// `**` matches to end of string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// Tenant that receives read access.
    pub grantee_tenant_id: String,
    /// Namespace glob within the granter's space that the grantee can read.
    pub namespace_pattern: String,
}

/// Extension: a tenant plus its outgoing grants. Wire-schema
/// compatible with `Tenant` — grants are an additive field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantWithGrants {
    /// Base tenant record.
    pub tenant: Tenant,
    /// Outgoing grants — namespaces this tenant lets others read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
}

/// Which namespace patterns should recall fan out across for
/// `(caller_tenant, include_grants=true)`?
///
/// Returns the caller's own `namespace_patterns` plus every pattern
/// they've been granted by other tenants in `all_tenants`.
///
/// Pure — no I/O.
pub fn expand_patterns_for_caller(
    caller_tenant_id: &str,
    caller_own: &Tenant,
    all_tenants: &[TenantWithGrants],
) -> Vec<String> {
    let mut out: Vec<String> = caller_own.namespace_patterns.clone();
    for t in all_tenants {
        // Skip self — the caller's own patterns already fan out.
        if t.tenant.tenant_id == caller_tenant_id {
            continue;
        }
        for grant in &t.grants {
            if grant.grantee_tenant_id == caller_tenant_id {
                out.push(grant.namespace_pattern.clone());
            }
        }
    }
    // Dedup while preserving order (small N — linear scan is fine).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// Given a list of namespaces the caller is trying to read and the
/// caller's expanded patterns, return the subset they're authorized
/// for. Order is preserved.
pub fn authorize_namespaces(
    patterns: &[String],
    namespaces: &[String],
) -> Vec<String> {
    namespaces
        .iter()
        .filter(|ns| {
            patterns
                .iter()
                .any(|p| crate::tenants::pattern_matches_public(p, ns))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenants::{Tenant, TenantQuotas};

    fn t(id: &str, patterns: &[&str]) -> Tenant {
        Tenant {
            tenant_id: id.into(),
            api_keys: vec!["k".into()],
            namespace_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            display_name: None,
            quotas: TenantQuotas::default(),
        }
    }

    #[test]
    fn caller_gets_own_patterns_when_no_grants() {
        let caller = t("acme", &["acme:*"]);
        let out = expand_patterns_for_caller("acme", &caller, &[]);
        assert_eq!(out, vec!["acme:*".to_string()]);
    }

    #[test]
    fn caller_gets_granted_patterns_from_other_tenant() {
        let caller = t("beta", &["beta:*"]);
        let granter = TenantWithGrants {
            tenant: t("acme", &["acme:*"]),
            grants: vec![Grant {
                grantee_tenant_id: "beta".into(),
                namespace_pattern: "acme:team:public:*".into(),
            }],
        };
        let out = expand_patterns_for_caller("beta", &caller, &[granter]);
        assert_eq!(
            out,
            vec![
                "beta:*".to_string(),
                "acme:team:public:*".to_string()
            ]
        );
    }

    #[test]
    fn grants_to_other_tenants_are_ignored() {
        let caller = t("beta", &["beta:*"]);
        let granter = TenantWithGrants {
            tenant: t("acme", &["acme:*"]),
            grants: vec![Grant {
                grantee_tenant_id: "gamma".into(),
                namespace_pattern: "acme:for:gamma:*".into(),
            }],
        };
        let out = expand_patterns_for_caller("beta", &caller, &[granter]);
        assert_eq!(out, vec!["beta:*".to_string()]);
    }

    #[test]
    fn duplicate_grants_are_deduped() {
        let caller = t("beta", &["beta:*"]);
        let t1 = TenantWithGrants {
            tenant: t("acme", &["acme:*"]),
            grants: vec![Grant {
                grantee_tenant_id: "beta".into(),
                namespace_pattern: "shared:*".into(),
            }],
        };
        let t2 = TenantWithGrants {
            tenant: t("gamma", &["gamma:*"]),
            grants: vec![Grant {
                grantee_tenant_id: "beta".into(),
                namespace_pattern: "shared:*".into(),
            }],
        };
        let out = expand_patterns_for_caller("beta", &caller, &[t1, t2]);
        assert_eq!(out, vec!["beta:*".to_string(), "shared:*".to_string()]);
    }

    #[test]
    fn self_tenant_grants_are_skipped() {
        // A tenant granting to itself is redundant — its own patterns
        // are already added — but the function must not double-count.
        let caller = t("acme", &["acme:*"]);
        let self_t = TenantWithGrants {
            tenant: t("acme", &["acme:*"]),
            grants: vec![Grant {
                grantee_tenant_id: "acme".into(),
                namespace_pattern: "acme:extra:*".into(),
            }],
        };
        let out = expand_patterns_for_caller("acme", &caller, &[self_t]);
        assert_eq!(out, vec!["acme:*".to_string()]);
    }

    #[test]
    fn authorize_namespaces_keeps_matching() {
        let patterns = vec![
            "acme:*".to_string(),
            "shared:public:*".to_string(),
        ];
        let requested = vec![
            "acme:agent:bot:user:luis".to_string(),
            "shared:public:kb".to_string(),
            "other:agent:bot".to_string(),
        ];
        let out = authorize_namespaces(&patterns, &requested);
        assert_eq!(
            out,
            vec![
                "acme:agent:bot:user:luis".to_string(),
                "shared:public:kb".to_string()
            ]
        );
    }

    #[test]
    fn grant_json_roundtrip() {
        let g = Grant {
            grantee_tenant_id: "beta".into(),
            namespace_pattern: "acme:shared:*".into(),
        };
        let s = serde_json::to_string(&g).unwrap();
        let back: Grant = serde_json::from_str(&s).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn tenant_with_grants_omits_empty_grants_from_json() {
        let t = TenantWithGrants {
            tenant: t("acme", &["acme:*"]),
            grants: vec![],
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(!s.contains("\"grants\""));
    }
}
