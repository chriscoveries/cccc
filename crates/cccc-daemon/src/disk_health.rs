//! Capacity of the CCCC home volume. Reaction and cleanup policy belongs to consumers.
use cccc_core::HomeLayout;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io;

mod journal;
#[cfg(test)]
mod tests;

pub(crate) use journal::read_events;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Thresholds {
    pub warning_percent: f64,
    pub critical_percent: f64,
    pub minimum_available_bytes: u64,
    pub recovery_percent: f64,
    pub recovery_available_bytes: u64,
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            warning_percent: 85.0,
            critical_percent: 90.0,
            minimum_available_bytes: 8 * 1024_u64.pow(3),
            recovery_percent: 1.0,
            recovery_available_bytes: 1024_u64.pow(3),
        }
    }
}
impl Thresholds {
    fn load(home: &HomeLayout) -> io::Result<Self> {
        let settings: Value = match cccc_core::fs::read_yaml(&home.root().join("settings.yaml")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Null,
            Err(error) => return Err(error),
        };
        let value = settings.pointer("/observability/disk_health");
        let thresholds: Self = match value {
            None | Some(Value::Null) => Self::default(),
            Some(value) => serde_json::from_value(value.clone()).map_err(io::Error::other)?,
        };
        thresholds.validate()?;
        Ok(thresholds)
    }
    fn validate(&self) -> io::Result<()> {
        if !self.warning_percent.is_finite()
            || !self.critical_percent.is_finite()
            || !self.recovery_percent.is_finite()
            || self.warning_percent <= 0.0
            || self.warning_percent >= self.critical_percent
            || self.critical_percent > 100.0
            || self.recovery_percent < 0.0
            || self.recovery_percent >= self.warning_percent
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid disk-health thresholds",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    #[default]
    Normal,
    Warning,
    Critical,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Snapshot {
    pub scope: String,
    pub volume_id: String,
    pub checked_at: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: f64,
    pub severity: Severity,
    pub thresholds: Thresholds,
}
impl Snapshot {
    fn from_capacity(
        volume_id: String,
        total: u64,
        free: u64,
        available: u64,
        thresholds: Thresholds,
    ) -> io::Result<Self> {
        if total == 0 || free > total || available > free {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid filesystem capacity",
            ));
        }
        thresholds.validate()?;
        let physical_used = total - free;
        let usable = physical_used + available;
        let used_percent = if usable == 0 {
            100.0 // Valid counters with no usable capacity: conservative pressure.
        } else {
            physical_used as f64 / usable as f64 * 100.0
        };
        let mut sample = Self {
            scope: "cccc_home".into(),
            volume_id,
            checked_at: cccc_contracts::utc_now(),
            total_bytes: total,
            free_bytes: free,
            available_bytes: available,
            used_percent,
            severity: Severity::Normal,
            thresholds,
        };
        sample.severity = sample.classify();
        Ok(sample)
    }
    fn classify(&self) -> Severity {
        if self.used_percent >= self.thresholds.critical_percent {
            Severity::Critical
        } else if self.used_percent >= self.thresholds.warning_percent
            || self.available_bytes < self.thresholds.minimum_available_bytes
        {
            Severity::Warning
        } else {
            Severity::Normal
        }
    }
    // Only event re-arming is hysteretic. Health always reports current measured pressure.
    fn event_severity(&self, previous: Severity) -> Severity {
        if self.severity >= previous {
            return self.severity;
        }
        if previous == Severity::Critical
            && self.used_percent
                >= self.thresholds.critical_percent - self.thresholds.recovery_percent
        {
            return Severity::Critical;
        }
        if self.used_percent >= self.thresholds.warning_percent - self.thresholds.recovery_percent
            || (self.thresholds.minimum_available_bytes > 0
                && self.available_bytes
                    < self
                        .thresholds
                        .minimum_available_bytes
                        .saturating_add(self.thresholds.recovery_available_bytes))
        {
            Severity::Warning
        } else {
            Severity::Normal
        }
    }
}

pub(crate) fn sample(home: &HomeLayout) -> io::Result<Snapshot> {
    let stats = fs2::statvfs(home.root())?;
    let canonical = std::fs::canonicalize(home.root())?;
    let mut identity = Sha256::new();
    identity.update(canonical.as_os_str().as_encoded_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        identity.update(std::fs::metadata(home.root())?.dev().to_le_bytes());
    }
    let volume_id = format!("{:x}", identity.finalize());
    Snapshot::from_capacity(
        volume_id,
        stats.total_space(),
        stats.free_space(),
        stats.available_space(),
        Thresholds::load(home)?,
    )
}

pub(crate) fn health(home: &HomeLayout) -> Value {
    match sample(home) {
        Ok(snapshot) => json!(snapshot),
        Err(error) => json!({"scope":"cccc_home","severity":"unknown",
            "checked_at":cccc_contracts::utc_now(), "error_kind":format!("{:?}",error.kind())}),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DiskEvent {
    pub v: u8,
    pub id: String,
    pub ts: String,
    pub kind: String,
    pub scope: String,
    pub episode_id: String,
    pub previous_severity: Severity,
    pub severity: Severity,
    pub direction: String,
    pub disk: Snapshot,
}

#[derive(Default)]
pub(crate) struct Publisher {
    previous: Option<DiskEvent>,
}
impl Publisher {
    pub(crate) fn tick(&mut self, home: &HomeLayout) -> io::Result<()> {
        self.publish(home, sample(home)?)
    }
    fn publish(&mut self, home: &HomeLayout, snapshot: Snapshot) -> io::Result<()> {
        if self.previous.is_none() {
            self.previous = journal::latest(home)?;
        }
        let prior = self
            .previous
            .as_ref()
            .filter(|event| event.disk.volume_id == snapshot.volume_id);
        let previous_severity = prior.map(|event| event.severity).unwrap_or_default();
        let severity = snapshot.event_severity(previous_severity);
        if severity == previous_severity {
            return Ok(());
        }
        let episode_id = if previous_severity == Severity::Normal {
            uuid::Uuid::new_v4().to_string()
        } else {
            prior.expect("non-normal previous event").episode_id.clone()
        };
        let event = DiskEvent {
            v: 1,
            id: uuid::Uuid::new_v4().to_string(),
            ts: snapshot.checked_at.clone(),
            kind: "disk.threshold_crossed".into(),
            scope: "cccc_home".into(),
            episode_id,
            previous_severity,
            severity,
            direction: if severity > previous_severity {
                "up"
            } else {
                "down"
            }
            .into(),
            disk: snapshot,
        };
        if let Err(error) = journal::append(home, &event) {
            // Re-read durable history next tick: an ambiguous write may have committed.
            self.previous = None;
            return Err(error);
        }
        self.previous = Some(event);
        Ok(())
    }
}
