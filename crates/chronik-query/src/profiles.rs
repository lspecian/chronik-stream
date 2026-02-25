//! Ranking profiles with hot reload support.
//!
//! Profiles define how candidates are scored after RRF fusion. Each profile
//! specifies feature weights and optional boost rules. Profiles can be loaded
//! from JSON files and reloaded at runtime without restart.
//!
//! ## Built-in Profiles
//!
//! - `default` — Balanced weights across all features
//! - `freshness` — Prioritizes recent messages (for real-time monitoring)
//! - `relevance` — Prioritizes text/vector match quality (for search)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A ranking profile defining feature weights and boost rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingProfile {
    /// Profile name
    pub name: String,
    /// Feature weights: feature_name → weight
    pub weights: HashMap<String, f64>,
    /// Optional boost rules applied after weighted scoring
    #[serde(default)]
    pub boosts: Vec<BoostRule>,
}

/// A conditional boost that multiplies the score when a feature meets a condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoostRule {
    /// Feature name to evaluate
    pub feature: String,
    /// Comparison operator
    pub op: CompareOp,
    /// Threshold value
    pub threshold: f64,
    /// Score multiplier when condition is true
    pub multiplier: f64,
}

/// Comparison operators for boost rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Gt,
    Gte,
    Lt,
    Lte,
    Eq,
}

impl BoostRule {
    /// Evaluate this boost rule against the given feature values.
    pub fn evaluate(&self, features: &HashMap<String, f64>) -> bool {
        if let Some(&value) = features.get(&self.feature) {
            match self.op {
                CompareOp::Gt => value > self.threshold,
                CompareOp::Gte => value >= self.threshold,
                CompareOp::Lt => value < self.threshold,
                CompareOp::Lte => value <= self.threshold,
                CompareOp::Eq => (value - self.threshold).abs() < f64::EPSILON,
            }
        } else {
            false
        }
    }
}

/// Store for managing ranking profiles with hot reload.
pub struct ProfileStore {
    profiles: Arc<RwLock<HashMap<String, RankingProfile>>>,
}

impl ProfileStore {
    /// Create a new profile store with built-in default profiles.
    pub fn new() -> Self {
        let mut profiles = HashMap::new();

        // Default profile: balanced
        profiles.insert("default".to_string(), Self::default_profile());
        profiles.insert("freshness".to_string(), Self::freshness_profile());
        profiles.insert("relevance".to_string(), Self::relevance_profile());

        Self {
            profiles: Arc::new(RwLock::new(profiles)),
        }
    }

    /// Get a profile by name. Returns the "default" profile if not found.
    pub async fn get(&self, name: &str) -> RankingProfile {
        let profiles = self.profiles.read().await;
        profiles
            .get(name)
            .cloned()
            .unwrap_or_else(|| Self::default_profile())
    }

    /// List all available profile names.
    pub async fn list(&self) -> Vec<String> {
        let profiles = self.profiles.read().await;
        profiles.keys().cloned().collect()
    }

    /// Add or update a profile.
    pub async fn set(&self, profile: RankingProfile) {
        let mut profiles = self.profiles.write().await;
        profiles.insert(profile.name.clone(), profile);
    }

    /// Load profiles from a directory of JSON files.
    ///
    /// Each file should contain a single `RankingProfile` serialized as JSON.
    /// Files are named `<profile_name>.json`.
    pub async fn load_from_directory(&self, dir: &Path) -> Result<usize, std::io::Error> {
        let mut count = 0;

        if !dir.exists() {
            tracing::debug!(path = %dir.display(), "Profile directory does not exist, using defaults");
            return Ok(0);
        }

        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "json") {
                match self.load_profile_file(&path).await {
                    Ok(profile) => {
                        tracing::info!(name = %profile.name, "Loaded ranking profile");
                        self.set(profile).await;
                        count += 1;
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "Failed to load profile");
                    }
                }
            }
        }

        Ok(count)
    }

    /// Load a single profile from a JSON file.
    async fn load_profile_file(
        &self,
        path: &Path,
    ) -> Result<RankingProfile, Box<dyn std::error::Error + Send + Sync>> {
        let contents = tokio::fs::read_to_string(path).await?;
        let profile: RankingProfile = serde_json::from_str(&contents)?;
        Ok(profile)
    }

    /// Built-in "default" profile: balanced weights.
    pub fn default_profile() -> RankingProfile {
        let mut weights = HashMap::new();
        weights.insert("rrf_score".to_string(), 100.0);
        weights.insert("freshness".to_string(), 5.0);
        weights.insert("source_count".to_string(), 10.0);
        weights.insert("text_rank".to_string(), 3.0);
        weights.insert("vector_rank".to_string(), 3.0);
        weights.insert("sql_rank".to_string(), 3.0);
        weights.insert("text_score".to_string(), 1.0);
        weights.insert("vector_score".to_string(), 1.0);

        RankingProfile {
            name: "default".to_string(),
            weights,
            boosts: vec![
                // Boost candidates found by multiple backends
                BoostRule {
                    feature: "source_count".to_string(),
                    op: CompareOp::Gt,
                    threshold: 1.0,
                    multiplier: 1.2,
                },
            ],
        }
    }

    /// Built-in "freshness" profile: prioritizes recent messages.
    pub fn freshness_profile() -> RankingProfile {
        let mut weights = HashMap::new();
        weights.insert("rrf_score".to_string(), 50.0);
        weights.insert("freshness".to_string(), 50.0);
        weights.insert("source_count".to_string(), 5.0);
        weights.insert("text_rank".to_string(), 2.0);
        weights.insert("vector_rank".to_string(), 2.0);
        weights.insert("sql_rank".to_string(), 2.0);

        RankingProfile {
            name: "freshness".to_string(),
            weights,
            boosts: vec![],
        }
    }

    /// Built-in "relevance" profile: prioritizes match quality.
    pub fn relevance_profile() -> RankingProfile {
        let mut weights = HashMap::new();
        weights.insert("rrf_score".to_string(), 100.0);
        weights.insert("freshness".to_string(), 1.0);
        weights.insert("source_count".to_string(), 15.0);
        weights.insert("text_rank".to_string(), 10.0);
        weights.insert("vector_rank".to_string(), 10.0);
        weights.insert("text_score".to_string(), 5.0);
        weights.insert("vector_score".to_string(), 5.0);

        RankingProfile {
            name: "relevance".to_string(),
            weights,
            boosts: vec![
                // Strong boost for multi-backend matches
                BoostRule {
                    feature: "source_count".to_string(),
                    op: CompareOp::Gt,
                    threshold: 1.0,
                    multiplier: 1.5,
                },
            ],
        }
    }

    /// Start a background task that polls a directory for profile changes.
    ///
    /// Reloads profiles from the directory every `interval` seconds.
    /// This runs until the returned handle is dropped.
    pub fn start_hot_reload(
        self: &Arc<Self>,
        dir: std::path::PathBuf,
        interval_secs: u64,
    ) -> tokio::task::JoinHandle<()> {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(interval_secs)
            );
            // Skip the first tick (immediate)
            interval.tick().await;

            loop {
                interval.tick().await;
                match store.load_from_directory(&dir).await {
                    Ok(count) => {
                        if count > 0 {
                            tracing::info!(count, path = %dir.display(), "Reloaded ranking profiles");
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, path = %dir.display(), "Profile directory not available");
                    }
                }
            }
        })
    }
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boost_rule_gt() {
        let rule = BoostRule {
            feature: "source_count".to_string(),
            op: CompareOp::Gt,
            threshold: 1.0,
            multiplier: 2.0,
        };

        let mut features = HashMap::new();
        features.insert("source_count".to_string(), 2.0);
        assert!(rule.evaluate(&features));

        features.insert("source_count".to_string(), 1.0);
        assert!(!rule.evaluate(&features));
    }

    #[test]
    fn test_boost_rule_missing_feature() {
        let rule = BoostRule {
            feature: "nonexistent".to_string(),
            op: CompareOp::Gt,
            threshold: 0.0,
            multiplier: 2.0,
        };
        assert!(!rule.evaluate(&HashMap::new()));
    }

    #[tokio::test]
    async fn test_profile_store_defaults() {
        let store = ProfileStore::new();
        let names = store.list().await;

        assert!(names.contains(&"default".to_string()));
        assert!(names.contains(&"freshness".to_string()));
        assert!(names.contains(&"relevance".to_string()));
    }

    #[tokio::test]
    async fn test_profile_store_get() {
        let store = ProfileStore::new();

        let default = store.get("default").await;
        assert_eq!(default.name, "default");
        assert!(default.weights.contains_key("rrf_score"));

        let freshness = store.get("freshness").await;
        assert_eq!(freshness.name, "freshness");
        // Freshness profile should have high freshness weight
        assert!(*freshness.weights.get("freshness").unwrap() > *default.weights.get("freshness").unwrap());
    }

    #[tokio::test]
    async fn test_profile_store_fallback() {
        let store = ProfileStore::new();
        let unknown = store.get("nonexistent").await;
        assert_eq!(unknown.name, "default");
    }

    #[tokio::test]
    async fn test_profile_store_custom() {
        let store = ProfileStore::new();

        let custom = RankingProfile {
            name: "custom".to_string(),
            weights: {
                let mut w = HashMap::new();
                w.insert("rrf_score".to_string(), 200.0);
                w
            },
            boosts: vec![],
        };

        store.set(custom).await;

        let loaded = store.get("custom").await;
        assert_eq!(loaded.name, "custom");
        assert_eq!(*loaded.weights.get("rrf_score").unwrap(), 200.0);
    }

    #[test]
    fn test_profile_serialization_roundtrip() {
        let profile = ProfileStore::default_profile();
        let json = serde_json::to_string_pretty(&profile).unwrap();
        let deserialized: RankingProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, profile.name);
        assert_eq!(deserialized.weights.len(), profile.weights.len());
    }
}
