use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A model's metadata, from the operator-edited catalog file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    /// Blended USD per million tokens. None when unset (unpriced).
    #[serde(default)]
    pub usd_per_mtok: Option<f64>,
    /// Flat USD per month for subscription models. None when unset.
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

/// What a turn cost, or why it cannot be priced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TurnCost {
    /// Metered tokens and a price: the turn has a cost.
    Priced { usd: f64, tokens: u64 },
    /// Metered tokens, no price for the model: recorded as unpriced.
    Unpriced { tokens: u64 },
    /// The turn reported no token counts, so there is nothing to price.
    Unmetered,
}

/// The turn's metered tokens: input, output and cache reads and writes.
///
/// `None` when the usage record carries no token count at all — a provider that
/// reports nothing is not a provider that reported zero tokens.
fn metered_tokens(usage: &serde_json::Value) -> Option<u64> {
    let mut seen = false;
    let mut tokens = 0_u64;
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        if let Some(count) = usage.get(field).and_then(serde_json::Value::as_u64) {
            seen = true;
            tokens = tokens.saturating_add(count);
        }
    }
    seen.then_some(tokens)
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

    /// What one turn cost at this catalog's blended price.
    ///
    /// The price is per million tokens, so a turn's cost is the metered token
    /// count scaled by it. A model with no `usd_per_mtok` — absent from the
    /// catalog, or flat-priced per month — yields `Unpriced`: the turn is
    /// counted, never guessed at. A turn that reported no token counts at all
    /// yields `Unmetered`, which is not the same as a free turn.
    pub fn turn_cost(&self, model_id: Option<&str>, usage: &serde_json::Value) -> TurnCost {
        let tokens = metered_tokens(usage);
        let Some(tokens) = tokens else {
            return TurnCost::Unmetered;
        };
        match model_id.and_then(|model| self.get(model)) {
            Some(spec) => match spec.usd_per_mtok {
                Some(price) if price.is_finite() && price >= 0.0 => TurnCost::Priced {
                    usd: tokens as f64 / 1_000_000.0 * price,
                    tokens,
                },
                _ => TurnCost::Unpriced { tokens },
            },
            None => TurnCost::Unpriced { tokens },
        }
    }

    /// Resolve cost and strength for an Actor's model label.
    pub fn resolve(&self, model: &str) -> (Option<f64>, Option<u8>) {
        match self.get(model) {
            Some(spec) => (spec.usd_per_mtok, spec.strength),
            None => (None, None),
        }
    }
}
