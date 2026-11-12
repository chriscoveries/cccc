//! Requested browser UI selection, distinct from the descriptive model label.
//! Observed UI state must be validated separately immediately before activation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChatgptModel {
    #[serde(rename = "gpt-5.6-pro")]
    Gpt56Pro,
    #[serde(rename = "gpt-6")]
    Gpt6,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ChatgptEffort {
    Instant,
    Medium,
    High,
    ExtraHigh,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct WebModelSelection {
    pub model: ChatgptModel,
    #[serde(default)]
    pub effort: Option<ChatgptEffort>,
}

impl WebModelSelection {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.model == ChatgptModel::Gpt56Pro && self.effort.is_some() {
            return Err("gpt-5.6-pro has an intrinsic mode; effort must be null");
        }
        Ok(())
    }

    /// Both values are canonical configuration patterns, not raw DOM labels.
    fn covers(self, selection: Self) -> bool {
        self.model == selection.model && (self.effort.is_none() || self.effort == selection.effort)
    }

    /// Null on a requested GPT-6 pin leaves effort unconstrained; null on an
    /// observation is unreadable and cannot authorize submission.
    pub fn accepts_observed(self, observed: Self) -> bool {
        observed.validate().is_ok()
            && (observed.model != ChatgptModel::Gpt6 || observed.effort.is_some())
            && self.covers(observed)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelPinUnavailable {
    #[default]
    Hold,
    Fallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebModelPin {
    pub model: ChatgptModel,
    #[serde(default)]
    pub effort: Option<ChatgptEffort>,
    #[serde(default)]
    pub on_unavailable: ModelPinUnavailable,
    #[serde(default)]
    pub fallback: Option<WebModelSelection>,
    #[serde(default)]
    pub deny: Vec<WebModelSelection>,
}

impl WebModelPin {
    pub fn primary(&self) -> WebModelSelection {
        WebModelSelection {
            model: self.model,
            effort: self.effort,
        }
    }

    pub fn normalize(&mut self) -> Result<(), &'static str> {
        self.primary().validate()?;
        if self.deny.len() > 16 {
            return Err("web_model_pin.deny accepts at most 16 selections");
        }
        for selection in &self.deny {
            selection.validate()?;
        }
        self.deny.sort_unstable();
        self.deny.dedup();
        if self.is_denied(self.primary()) {
            return Err("web_model_pin primary selection is denied");
        }
        match (self.on_unavailable, self.fallback) {
            (ModelPinUnavailable::Hold, None) => {}
            (ModelPinUnavailable::Fallback, Some(selection)) => {
                selection.validate()?;
                if self.is_denied(selection) {
                    return Err("web_model_pin fallback selection is denied");
                }
            }
            _ => {
                return Err(
                    "web_model_pin fallback requires on_unavailable=fallback and one selection",
                );
            }
        }
        Ok(())
    }

    pub fn is_denied(&self, selection: WebModelSelection) -> bool {
        self.deny.iter().any(|denied| denied.covers(selection))
    }

    pub fn accepts_primary_observation(&self, observed: WebModelSelection) -> bool {
        !self.is_denied(observed) && self.primary().accepts_observed(observed)
    }

    pub fn accepts_fallback_observation(&self, observed: WebModelSelection) -> bool {
        self.on_unavailable == ModelPinUnavailable::Fallback
            && !self.is_denied(observed)
            && self
                .fallback
                .is_some_and(|fallback| fallback.accepts_observed(observed))
    }
}
