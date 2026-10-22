use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A model's metadata, from the owner-edited catalog (MODEL-HARNESS-CATALOG).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    /// Blended USD per million tokens. 0.0 for free.
    #[serde(default)]
    pub usd_per_mtok: f64,
    /// Flat USD per month for subscription models (Devin). None for token-priced.
    #[serde(default)]
    pub usd_per_month: Option<f64>,
    /// 1 (weak) .. 5 (strongest). None when unset.
    #[serde(default)]
    pub strength: Option<u8>,
    /// May take code cards.
    #[serde(default)]
    pub code: bool,
    /// Requests per minute (0 = unset).
    #[serde(default)]
    pub rpm: u32,
    /// Requests per day (0 = unset).
    #[serde(default)]
    pub rpd: u32,
    /// Max concurrent lanes on this model (0 = no cap).
    #[serde(default)]
    pub max_lanes: u32,
}

/// Registry keyed by model id. Empty when no catalog is loaded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRegistry {
    pub models: BTreeMap<String, ModelSpec>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_path(path: &std::path::Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    pub fn get(&self, model: &str) -> Option<&ModelSpec> {
        if model.is_empty() {
            None
        } else {
            self.models.get(model)
        }
    }

    /// Resolve cost and strength for an Actor's model label.
    pub fn resolve(&self, model: &str) -> (Option<f64>, Option<u8>) {
        match self.get(model) {
            Some(spec) => (Some(spec.usd_per_mtok), spec.strength),
            None => (None, None),
        }
    }
}
