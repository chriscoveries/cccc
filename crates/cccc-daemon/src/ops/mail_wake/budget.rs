//! Durable cross-group admission for automatic Mail starts.
//!
//! The caller must hold the home-scoped budget mutex while using this type.
//! It is not a lock for the provider launch; the persisted attempt is that
//! fence. Missing or malformed state never creates a fresh allowance.
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const VERSION: u8 = 1;
const WINDOW: Duration = Duration::from_secs(60);
const LIMIT: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Attempt {
    pub id: String,
    pub group_id: String,
    pub actor_id: String,
    pub generation: String,
    pub policy_fingerprint: String,
    pub source_event_id: String,
}

impl Attempt {
    pub(super) fn validate(&self) -> io::Result<()> {
        if [
            &self.id,
            &self.group_id,
            &self.actor_id,
            &self.generation,
            &self.policy_fingerprint,
            &self.source_event_id,
        ]
        .into_iter()
        .any(|value| value.is_empty() || value.len() > 256)
            || self.policy_fingerprint.len() != 64
            || !self
                .policy_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid mail wake attempt",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    activation_id: String,
    policy_key: [u8; 32],
    last_observed_at: String,
    /// Wall-clock strings are informational only and never refill a budget.
    debits: Vec<String>,
    in_flight: Option<Attempt>,
}

impl Record {
    fn validate(&self) -> io::Result<()> {
        if self.version != VERSION
            || self.activation_id.is_empty()
            || self.activation_id.len() > 128
            || self.policy_key.iter().all(|byte| *byte == 0)
            || chrono::DateTime::parse_from_rfc3339(&self.last_observed_at).is_err()
            || self.debits.len() > LIMIT
            || self
                .debits
                .iter()
                .any(|entry| chrono::DateTime::parse_from_rfc3339(entry).is_err())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid mail wake budget",
            ));
        }
        if let Some(attempt) = &self.in_flight {
            attempt.validate()?;
            if self.debits.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "in-flight attempt without debit",
                ));
            }
        }
        Ok(())
    }
}

pub(super) struct Budget {
    path: PathBuf,
    record: Record,
    /// Reconstructed on every load: old debits retain a full fresh window.
    clocks: VecDeque<Instant>,
    last_wall: chrono::DateTime<chrono::FixedOffset>,
    /// A write error can mean an ambiguous rename; do not trust a local retry.
    unknown: bool,
}

impl Budget {
    /// Exclusive creation never replaces a prior debit or unresolved attempt.
    pub(super) fn create_for_activation(path: &Path, activation_id: &str) -> io::Result<()> {
        use std::io::Write;
        if activation_id.is_empty() || activation_id.len() > 128 {
            return Err(io::Error::other("invalid Mail wake activation ID"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("budget path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let mut policy_key = [0; 32];
        policy_key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        policy_key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let record = Record {
            version: VERSION,
            activation_id: activation_id.to_owned(),
            policy_key,
            last_observed_at: cccc_contracts::utc_now(),
            debits: Vec::new(),
            in_flight: None,
        };
        let mut bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn initialize(path: &Path) -> io::Result<()> {
        Self::create_for_activation(path, "activation-control")
    }

    pub fn load(path: &Path, now: Instant) -> io::Result<Self> {
        Self::load_at(path, now, chrono::Utc::now().fixed_offset())
    }

    fn load_at(
        path: &Path,
        now: Instant,
        last_wall: chrono::DateTime<chrono::FixedOffset>,
    ) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::other("Mail wake budget is not private"));
            }
        }
        let record: Record = cccc_core::fs::read_json(path)?;
        record.validate()?;
        let recorded = chrono::DateTime::parse_from_rfc3339(&record.last_observed_at)
            .map_err(io::Error::other)?;
        if last_wall < recorded {
            return Err(io::Error::other("Mail wake wall clock moved backwards"));
        }
        let clocks = std::iter::repeat_n(now, record.debits.len()).collect();
        Ok(Self {
            path: path.to_owned(),
            record,
            clocks,
            last_wall,
            unknown: false,
        })
    }

    pub fn activation_id(&self) -> &str {
        &self.record.activation_id
    }

    pub fn policy_key(&self) -> &[u8; 32] {
        &self.record.policy_key
    }

    pub fn is_pristine(&self) -> bool {
        self.record.debits.is_empty() && self.record.in_flight.is_none()
    }

    pub fn in_flight(&self) -> Option<Attempt> {
        self.record.in_flight.clone()
    }

    pub fn verify_unchanged(&mut self) -> io::Result<()> {
        if self.unknown {
            return Err(io::Error::other("Mail wake budget unknown"));
        }
        self.check_disk()
    }

    fn check_disk(&mut self) -> io::Result<()> {
        self.check_disk_at(chrono::Utc::now().fixed_offset())
    }

    fn check_disk_at(&mut self, wall: chrono::DateTime<chrono::FixedOffset>) -> io::Result<()> {
        if wall < self.last_wall {
            self.unknown = true;
            return Err(io::Error::other("Mail wake wall clock moved backwards"));
        }
        self.last_wall = wall;
        let current = cccc_core::fs::read_json::<Record>(&self.path);
        match current {
            Ok(record) if record == self.record && record.validate().is_ok() => Ok(()),
            _ => {
                self.unknown = true;
                Err(io::Error::other(
                    "global Mail wake state changed or disappeared",
                ))
            }
        }
    }

    /// Reserve durably before any group debit or launch. False means held.
    pub fn reserve(&mut self, attempt: Attempt, now: Instant) -> io::Result<bool> {
        attempt.validate()?;
        if self.unknown || self.record.in_flight.is_some() {
            return Ok(false);
        }
        self.check_disk()?;
        let mut next = self.record.clone();
        let mut clocks = self.clocks.clone();
        while let Some(oldest) = clocks.front().copied() {
            let Some(age) = now.checked_duration_since(oldest) else {
                self.unknown = true;
                return Err(io::Error::other(
                    "mail wake monotonic clock moved backwards",
                ));
            };
            if age < WINDOW {
                break;
            }
            clocks.pop_front();
            next.debits.remove(0);
        }
        if clocks.len() >= LIMIT {
            return Ok(false);
        }
        next.debits.push(self.last_wall.to_rfc3339());
        next.last_observed_at = self.last_wall.to_rfc3339();
        next.in_flight = Some(attempt);
        self.unknown = true;
        cccc_core::fs::write_json(&self.path, &next)?;
        self.record = next;
        clocks.push_back(now);
        self.clocks = clocks;
        self.unknown = false;
        Ok(true)
    }

    /// Call only after the matching settled group result was durably recorded.
    pub fn settle(&mut self, attempt: &Attempt) -> io::Result<()> {
        if self.unknown || self.record.in_flight.as_ref() != Some(attempt) {
            return Err(io::Error::other("mail wake attempt join is unknown"));
        }
        self.check_disk()?;
        let mut next = self.record.clone();
        next.in_flight = None;
        next.last_observed_at = self.last_wall.to_rfc3339();
        self.unknown = true;
        cccc_core::fs::write_json(&self.path, &next)?;
        self.record = next;
        self.unknown = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_contracts::Event;

    // Redacted capture. The private raw-to-redacted manifest binds its provenance.
    fn captured_attempt(id: &str) -> Attempt {
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(event.kind, "chat.message");
        Attempt {
            id: id.to_owned(),
            group_id: event.group_id,
            actor_id: "receiver".into(),
            generation: "generation-one".into(),
            policy_fingerprint: "a".repeat(64),
            source_event_id: event.id,
        }
    }

    fn activated(path: &Path, now: Instant) -> Budget {
        // Labeled control state; the event above is the retained real input.
        let record = Record {
            version: VERSION,
            activation_id: "activation-control".into(),
            policy_key: [1; 32],
            last_observed_at: cccc_contracts::utc_now(),
            debits: Vec::new(),
            in_flight: None,
        };
        cccc_core::fs::write_json(path, &record).expect("activation control");
        Budget::load(path, now).expect("budget")
    }

    #[test]
    fn missing_or_invalid_policy_key_never_creates_a_new_budget() {
        let _source = captured_attempt("attempt-one");
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("budget.json");
        let _budget = activated(&path, Instant::now());
        let mut value: serde_json::Value = cccc_core::fs::read_json(&path).expect("budget");
        value.as_object_mut().expect("object").remove("policy_key");
        cccc_core::fs::write_json(&path, &value).expect("labeled missing key");
        assert!(Budget::load(&path, Instant::now()).is_err());
        value["policy_key"] = serde_json::json!(vec![0u8; 32]);
        cccc_core::fs::write_json(&path, &value).expect("labeled invalid key");
        assert!(Budget::load(&path, Instant::now()).is_err());
        assert_eq!(
            cccc_core::fs::read_json::<serde_json::Value>(&path).expect("unchanged"),
            value
        );
    }

    #[test]
    fn global_budget_is_shared_and_recovers_conservatively() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("budget.json");
        let start = Instant::now();
        let mut budget = activated(&path, start);
        let first = captured_attempt("attempt-one");
        let mut second = captured_attempt("attempt-two");
        second.group_id = "another-group".into(); // coordination control
        assert!(budget.reserve(first.clone(), start).expect("first"));
        assert!(
            !budget
                .reserve(second.clone(), start)
                .expect("in-flight holds")
        );
        budget.settle(&first).expect("settled group result");
        assert!(budget.reserve(second.clone(), start).expect("second group"));
        budget.settle(&second).expect("settled group result");
        assert!(!budget.reserve(first.clone(), start).expect("window holds"));
        let after = start + Duration::from_secs(61);
        let mut restarted = Budget::load(&path, after).expect("reload");
        assert!(
            !restarted
                .reserve(first.clone(), after)
                .expect("fresh window holds")
        );
        assert!(
            restarted
                .reserve(first, after + WINDOW)
                .expect("window expires")
        );
    }

    #[test]
    fn unknown_global_record_and_unsettled_attempt_hold() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("budget.json");
        let now = Instant::now();
        assert!(Budget::load(&path, now).is_err());
        let mut budget = activated(&path, now);
        let first = captured_attempt("attempt-one");
        assert!(budget.reserve(first, now).expect("reserve"));
        let mut restarted = Budget::load(&path, now + WINDOW).expect("load");
        assert!(
            !restarted
                .reserve(captured_attempt("attempt-two"), now + WINDOW)
                .expect("unresolved held")
        );
        assert!(restarted.settle(&captured_attempt("attempt-two")).is_err());
    }

    #[test]
    fn three_groups_share_two_attempt_window() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("budget.json");
        let now = Instant::now();
        let mut budget = activated(&path, now);
        let mut attempts = [
            captured_attempt("one"),
            captured_attempt("two"),
            captured_attempt("three"),
        ];
        // Group names and elapsed time are labeled coordination controls.
        for (index, attempt) in attempts.iter_mut().enumerate() {
            attempt.group_id = format!("group-{index}");
        }
        for attempt in &attempts[..2] {
            assert!(budget.reserve(attempt.clone(), now).expect("admit"));
            budget.settle(attempt).expect("settle");
        }
        assert!(
            !budget
                .reserve(attempts[2].clone(), now)
                .expect("third held")
        );
        assert!(
            budget
                .reserve(attempts[2].clone(), now + WINDOW)
                .expect("later admitted")
        );
    }

    #[test]
    fn observed_wall_clock_rollback_holds_after_capture_and_reload() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("budget.json");
        let now = Instant::now();
        let mut budget = activated(&path, now);
        let attempt = captured_attempt("attempt-one");
        assert!(budget.reserve(attempt, now).expect("reserve"));
        let earlier = budget.last_wall - chrono::Duration::seconds(1); // labeled clock mutation
        assert!(budget.check_disk_at(earlier).is_err());
        assert!(budget.unknown);
        assert!(Budget::load_at(&path, now, earlier).is_err());
    }
}
