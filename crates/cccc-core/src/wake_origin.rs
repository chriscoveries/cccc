use serde::{Deserialize, Serialize};
use std::io;

/// Private launch metadata. This records an attempted association, not proof
/// that an owner is alive; callers must also verify the same registered owner.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WakeOrigin {
    pub attempt_id: String,
    pub actor_generation: String,
    pub policy_fingerprint: String,
    pub source_event_id: String,
}

impl WakeOrigin {
    pub fn validate(&self) -> io::Result<()> {
        if [
            &self.attempt_id,
            &self.actor_generation,
            &self.source_event_id,
        ]
        .into_iter()
        .any(|value| value.is_empty() || value.len() > 256)
            || self.policy_fingerprint.len() != 64
            || !self
                .policy_fingerprint
                .bytes()
                .all(|value| value.is_ascii_hexdigit())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid wake origin",
            ));
        }
        Ok(())
    }
}
