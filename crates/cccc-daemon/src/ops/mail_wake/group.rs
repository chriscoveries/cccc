//! Per-group bounded Mail wake state. Callers hold the existing group permit.
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cccc_core::{GroupStore, HomeLayout};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::budget::Attempt;

const VERSION: u8 = 1;
const ACTOR_WINDOW: Duration = Duration::from_secs(600);
const GROUP_WINDOW: Duration = Duration::from_secs(60);
const ACTOR_SPACING: Duration = Duration::from_secs(60);
const LIMIT: usize = 2;

/// The source is a scheduling hint, never proof that Mail remains awaited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub source_event_id: String,
    pub source_ts: String,
    pub generation: String,
    pub policy_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Watermark {
    source_event_id: String,
    source_ts: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActorState {
    generation: String,
    debits: Vec<String>,
    in_flight: Option<Attempt>,
    settled_attempt: Option<Attempt>,
    pending: Option<Pending>,
    cue_pending: bool,
    settled_source: Option<Watermark>,
    failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    group_id: String,
    group_generation: String,
    debits: Vec<String>,
    actors: BTreeMap<String, ActorState>,
}

impl State {
    fn empty(group_id: &str, group_generation: &str) -> Self {
        Self {
            version: VERSION,
            group_id: group_id.to_owned(),
            group_generation: group_generation.to_owned(),
            debits: Vec::new(),
            actors: BTreeMap::new(),
        }
    }

    fn validate(&self, group_id: &str, group_generation: &str) -> io::Result<()> {
        let valid_times = |debits: &[String]| {
            debits.len() <= LIMIT
                && debits
                    .iter()
                    .all(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
        };
        if self.version != VERSION
            || self.group_id != group_id
            || self.group_generation != group_generation
            || group_generation.is_empty()
            || !valid_times(&self.debits)
            || self.actors.len() > 1024
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid group wake state",
            ));
        }
        for (actor, state) in &self.actors {
            if actor.is_empty()
                || actor.len() > 256
                || state.generation.is_empty()
                || state.generation.len() > 256
                || !valid_times(&state.debits)
                || state.settled_source.as_ref().is_some_and(|watermark| {
                    watermark.source_event_id.is_empty()
                        || watermark.source_event_id.len() > 256
                        || chrono::DateTime::parse_from_rfc3339(&watermark.source_ts).is_err()
                })
                || (state.cue_pending && (state.pending.is_none() || state.in_flight.is_some()))
                || state.settled_attempt.as_ref().is_some_and(|attempt| {
                    attempt.validate().is_err()
                        || attempt.group_id != group_id
                        || attempt.actor_id != *actor
                        || attempt.generation != state.generation
                        || !state.cue_pending
                        || !state.pending.as_ref().is_some_and(|pending| {
                            pending.source_event_id == attempt.source_event_id
                                && pending.policy_fingerprint == attempt.policy_fingerprint
                        })
                })
                || (state.cue_pending && state.settled_attempt.is_none())
                || state.pending.as_ref().is_some_and(|pending| {
                    pending.generation != state.generation
                        || pending.source_event_id.is_empty()
                        || pending.source_event_id.len() > 256
                        || pending.policy_fingerprint.len() != 64
                        || !pending
                            .policy_fingerprint
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                        || chrono::DateTime::parse_from_rfc3339(&pending.source_ts).is_err()
                })
                || state.in_flight.as_ref().is_some_and(|attempt| {
                    attempt.validate().is_err()
                        || attempt.group_id != group_id
                        || attempt.actor_id != *actor
                        || attempt.generation != state.generation
                        || state.debits.is_empty()
                        || self.debits.is_empty()
                        || !state.pending.as_ref().is_some_and(|pending| {
                            pending.source_event_id == attempt.source_event_id
                                && pending.policy_fingerprint == attempt.policy_fingerprint
                        })
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid actor wake state",
                ));
            }
        }
        Ok(())
    }
}

fn age_out(
    clocks: &mut VecDeque<Instant>,
    debits: &mut Vec<String>,
    now: Instant,
    window: Duration,
) -> io::Result<()> {
    while let Some(oldest) = clocks.front().copied() {
        let age = now
            .checked_duration_since(oldest)
            .ok_or_else(|| io::Error::other("wake clock moved backwards"))?;
        if age < window {
            break;
        }
        clocks.pop_front();
        debits.remove(0);
    }
    Ok(())
}

pub(super) struct GroupBudget {
    path: PathBuf,
    group_id: String,
    group_generation: String,
    initializing: bool,
    state: State,
    group_clocks: VecDeque<Instant>,
    actor_clocks: BTreeMap<(String, String), VecDeque<Instant>>,
    unknown: bool,
}

pub(super) struct Prepared {
    next: State,
    key: (String, String),
    group_clocks: VecDeque<Instant>,
    actor_clocks: VecDeque<Instant>,
}

impl GroupBudget {
    /// A global record that has never registered this group cannot adopt an
    /// existing wake namespace. The caller holds the group permit throughout.
    #[cfg(test)]
    pub fn check_unregistered(home: &HomeLayout, group_id: &str) -> io::Result<()> {
        let store = GroupStore::new(home.clone())?;
        let path = store.state_dir(group_id)?.join("automation.json");
        if !path.try_exists()? {
            return Ok(());
        }
        let doc: Value = cccc_core::fs::read_json(&path)?;
        let root = doc
            .as_object()
            .ok_or_else(|| io::Error::other("invalid automation state"))?;
        if root.contains_key("mail_wake_v1") {
            return Err(io::Error::other(
                "unregistered Mail wake group state exists",
            ));
        }
        Ok(())
    }

    /// Caller must already have verified an activated global record.
    pub fn load(
        home: &HomeLayout,
        group_id: &str,
        now: Instant,
        initialize: bool,
    ) -> io::Result<Self> {
        let store = GroupStore::new(home.clone())?;
        let group = store.load(group_id)?;
        let group_generation = if group.generation.is_empty() {
            if group.created_at.is_empty() {
                return Err(io::Error::other("group generation unknown"));
            }
            format!("legacy:{}", group.created_at)
        } else {
            group.generation
        };
        let path = store.state_dir(group_id)?.join("automation.json");
        let doc: Value = if path.try_exists()? {
            cccc_core::fs::read_json(&path)?
        } else {
            json!({})
        };
        let object = doc.as_object().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid automation state")
        })?;
        let state = match object.get("mail_wake_v1") {
            Some(_) if initialize => {
                return Err(io::Error::other(
                    "unregistered Mail wake group state exists",
                ));
            }
            Some(value) => {
                serde_json::from_value::<State>(value.clone()).map_err(io::Error::other)?
            }
            None if initialize => State::empty(group_id, &group_generation),
            None => return Err(io::Error::other("registered Mail wake group state missing")),
        };
        state.validate(group_id, &group_generation)?;
        let group_clocks = std::iter::repeat_n(now, state.debits.len()).collect();
        let actor_clocks = state
            .actors
            .iter()
            .map(|(id, actor)| {
                (
                    (id.clone(), actor.generation.clone()),
                    std::iter::repeat_n(now, actor.debits.len()).collect(),
                )
            })
            .collect();
        let mut budget = Self {
            path,
            group_id: group_id.to_owned(),
            group_generation,
            initializing: initialize,
            state,
            group_clocks,
            actor_clocks,
            unknown: false,
        };
        if initialize {
            budget.save(budget.state.clone())?;
        }
        Ok(budget)
    }

    fn save(&mut self, next: State) -> io::Result<()> {
        if self.unknown {
            return Err(io::Error::other("group wake state unknown"));
        }
        // Read, validation, and replacement failures are all uncertain. Do
        // not let a later in-process retry infer that the state was clean.
        self.unknown = true;
        next.validate(&self.group_id, &self.group_generation)?;
        let mut doc: Value = if self.path.try_exists()? {
            cccc_core::fs::read_json(&self.path)?
        } else {
            json!({})
        };
        let root = doc.as_object_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid automation state")
        })?;
        if let Some(current) = root.get("mail_wake_v1") {
            let current: State =
                serde_json::from_value(current.clone()).map_err(io::Error::other)?;
            if current != self.state {
                return Err(io::Error::other("group wake state changed"));
            }
        } else if !self.initializing {
            return Err(io::Error::other("group wake state disappeared"));
        }
        root.insert(
            "mail_wake_v1".into(),
            serde_json::to_value(&next).map_err(io::Error::other)?,
        );
        cccc_core::fs::write_json(&self.path, &doc)?;
        self.state = next;
        self.initializing = false;
        self.unknown = false;
        Ok(())
    }

    /// Check the actual group generation even when its budget is cached.
    pub fn check_group(&mut self, home: &HomeLayout) -> io::Result<()> {
        if self.unknown {
            return Err(io::Error::other("group wake state unknown"));
        }
        let checked = self.check_group_inner(home);
        if checked.is_err() {
            self.unknown = true;
        }
        checked
    }

    fn check_group_inner(&self, home: &HomeLayout) -> io::Result<()> {
        let group = GroupStore::new(home.clone())?.load(&self.group_id)?;
        let generation = if group.generation.is_empty() {
            if group.created_at.is_empty() {
                return Err(io::Error::other("group generation unknown"));
            }
            format!("legacy:{}", group.created_at)
        } else {
            group.generation
        };
        if generation != self.group_generation {
            return Err(io::Error::other("group generation changed"));
        }
        let doc: Value = cccc_core::fs::read_json(&self.path)?;
        let current: State = serde_json::from_value(
            doc.get("mail_wake_v1")
                .ok_or_else(|| io::Error::other("group wake state missing"))?
                .clone(),
        )
        .map_err(io::Error::other)?;
        current.validate(&self.group_id, &self.group_generation)?;
        if current != self.state {
            return Err(io::Error::other("group wake state changed"));
        }
        Ok(())
    }

    /// Retire obsolete registrations without deleting an unresolved owner.
    pub fn retain_current(&mut self, current: &BTreeMap<String, String>) -> io::Result<()> {
        let mut next = self.state.clone();
        for (id, actor) in &next.actors {
            if current.get(id) != Some(&actor.generation) && actor.in_flight.is_some() {
                return Err(io::Error::other(
                    "obsolete actor generation remains unresolved",
                ));
            }
        }
        next.actors
            .retain(|id, actor| current.get(id) == Some(&actor.generation));
        if next != self.state {
            self.save(next)?;
        }
        self.actor_clocks
            .retain(|(id, generation), _| current.get(id) == Some(generation));
        Ok(())
    }

    /// Coalesce a committed source while holding the same group permit as
    /// lifecycle operations. Only the oldest source for a generation is kept.
    pub fn note(&mut self, actor_id: &str, pending: Pending) -> io::Result<()> {
        if self.unknown
            || actor_id.is_empty()
            || actor_id.len() > 256
            || pending.generation.is_empty()
            || pending.source_event_id.is_empty()
            || pending.source_event_id.len() > 256
            || pending.policy_fingerprint.len() != 64
            || !pending
                .policy_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || chrono::DateTime::parse_from_rfc3339(&pending.source_ts).is_err()
        {
            return Err(io::Error::other("invalid or unknown pending Mail wake"));
        }
        let mut next = self.state.clone();
        if let Some(previous) = next.actors.get(actor_id)
            && previous.generation != pending.generation
        {
            if previous.in_flight.is_some() {
                return Err(io::Error::other(
                    "prior actor generation remains unresolved",
                ));
            }
            next.actors.remove(actor_id);
        }
        let actor = next
            .actors
            .entry(actor_id.to_owned())
            .or_insert_with(|| ActorState {
                generation: pending.generation.clone(),
                debits: Vec::new(),
                in_flight: None,
                settled_attempt: None,
                pending: None,
                cue_pending: false,
                settled_source: None,
                failed: false,
            });
        if actor.failed
            || actor.in_flight.is_some()
            || actor.cue_pending
            || actor.settled_source.as_ref().is_some_and(|settled| {
                let old = chrono::DateTime::parse_from_rfc3339(&settled.source_ts).ok();
                let new = chrono::DateTime::parse_from_rfc3339(&pending.source_ts).ok();
                (new, &pending.source_event_id) <= (old, &settled.source_event_id)
            })
        {
            return Ok(());
        }
        if actor.pending.as_ref().is_some_and(|old| {
            let old_time = chrono::DateTime::parse_from_rfc3339(&old.source_ts).ok();
            let new_time = chrono::DateTime::parse_from_rfc3339(&pending.source_ts).ok();
            (old_time, &old.source_event_id) <= (new_time, &pending.source_event_id)
                && old.policy_fingerprint == pending.policy_fingerprint
        }) {
            return Ok(());
        }
        actor.pending = Some(pending);
        self.save(next)
    }

    pub fn pending(&self, actor_id: &str) -> Option<&Pending> {
        self.state
            .actors
            .get(actor_id)
            .and_then(|actor| actor.pending.as_ref())
    }

    /// Cancel only the queued hint. Existing debits and unresolved starts stay.
    pub fn clear_pending(&mut self, actor_id: &str) -> io::Result<()> {
        let mut next = self.state.clone();
        let Some(actor) = next.actors.get_mut(actor_id) else {
            return Ok(());
        };
        if actor.pending.is_none() {
            return Ok(());
        }
        actor.pending = None;
        actor.cue_pending = false;
        actor.settled_attempt = None;
        self.save(next)
    }

    pub fn unresolved(&self) -> impl Iterator<Item = &Attempt> {
        self.state
            .actors
            .values()
            .filter_map(|actor| actor.in_flight.as_ref())
    }

    pub fn attempt_matches(&self, attempt: &Attempt) -> bool {
        self.state
            .actors
            .get(&attempt.actor_id)
            .is_some_and(|actor| actor.in_flight.as_ref() == Some(attempt))
    }

    pub fn settled_matches(&self, attempt: &Attempt) -> bool {
        self.state
            .actors
            .get(&attempt.actor_id)
            .is_some_and(|actor| {
                actor.cue_pending && actor.settled_attempt.as_ref() == Some(attempt)
            })
    }

    pub fn already_settled(
        &self,
        actor_id: &str,
        source_ts: &str,
        source_id: &str,
    ) -> io::Result<bool> {
        let source_time =
            chrono::DateTime::parse_from_rfc3339(source_ts).map_err(io::Error::other)?;
        let Some(watermark) = self
            .state
            .actors
            .get(actor_id)
            .and_then(|actor| actor.settled_source.as_ref())
        else {
            return Ok(false);
        };
        let settled_time =
            chrono::DateTime::parse_from_rfc3339(&watermark.source_ts).map_err(io::Error::other)?;
        Ok((source_time, source_id) <= (settled_time, watermark.source_event_id.as_str()))
    }

    pub fn cue_pending(&self, actor_id: &str) -> bool {
        self.state
            .actors
            .get(actor_id)
            .is_some_and(|actor| actor.cue_pending)
    }

    /// Explicit successful operator start or off-to-on opt-in may rearm a
    /// failed generation. This does not clear an unresolved start or debits.
    pub fn rearm(&mut self, actor_id: &str, generation: &str) -> io::Result<()> {
        let mut next = self.state.clone();
        let Some(actor) = next.actors.get_mut(actor_id) else {
            return Ok(());
        };
        if actor.generation != generation || actor.in_flight.is_some() || !actor.failed {
            return Ok(());
        }
        actor.failed = false;
        self.save(next)
    }

    /// Check rate and generation without spending a global debit. The group
    /// permit stays held until commit, so the prepared state cannot race a Stop.
    pub fn prepare(&self, attempt: &Attempt, now: Instant) -> io::Result<Option<Prepared>> {
        attempt.validate()?;
        if self.unknown || attempt.group_id != self.group_id {
            return Ok(None);
        }
        let key = (attempt.actor_id.clone(), attempt.generation.clone());
        let mut next = self.state.clone();
        if let Some(previous) = next.actors.get(&attempt.actor_id)
            && previous.generation != attempt.generation
        {
            if previous.in_flight.is_some() {
                return Ok(None);
            }
            // A new registration does not inherit an old actor's failure latch.
            next.actors.remove(&attempt.actor_id);
        }
        let actor = next
            .actors
            .entry(attempt.actor_id.clone())
            .or_insert_with(|| ActorState {
                generation: attempt.generation.clone(),
                debits: Vec::new(),
                in_flight: None,
                settled_attempt: None,
                pending: None,
                cue_pending: false,
                settled_source: None,
                failed: false,
            });
        if actor.failed
            || actor.in_flight.is_some()
            || actor.cue_pending
            || !actor.pending.as_ref().is_some_and(|pending| {
                pending.generation == attempt.generation
                    && pending.source_event_id == attempt.source_event_id
                    && pending.policy_fingerprint == attempt.policy_fingerprint
            })
        {
            return Ok(None);
        }
        let mut group_clocks = self.group_clocks.clone();
        let mut actor_clocks = self.actor_clocks.get(&key).cloned().unwrap_or_default();
        age_out(&mut group_clocks, &mut next.debits, now, GROUP_WINDOW)?;
        age_out(&mut actor_clocks, &mut actor.debits, now, ACTOR_WINDOW)?;
        if group_clocks.len() >= LIMIT || actor_clocks.len() >= LIMIT {
            return Ok(None);
        }
        if let Some(last) = actor_clocks.back()
            && now
                .checked_duration_since(*last)
                .ok_or_else(|| io::Error::other("wake clock moved backwards"))?
                < ACTOR_SPACING
        {
            return Ok(None);
        }
        let stamp = cccc_contracts::utc_now();
        actor.debits.push(stamp.clone());
        actor.in_flight = Some(attempt.clone());
        next.debits.push(stamp);
        group_clocks.push_back(now);
        actor_clocks.push_back(now);
        Ok(Some(Prepared {
            next,
            key,
            group_clocks,
            actor_clocks,
        }))
    }

    /// Commit after the global debit. On any error, retain its in-flight latch
    /// and never launch or refund the attempt.
    pub fn commit(&mut self, prepared: Prepared) -> io::Result<()> {
        self.save(prepared.next)?;
        self.group_clocks = prepared.group_clocks;
        self.actor_clocks
            .insert(prepared.key, prepared.actor_clocks);
        Ok(())
    }

    /// A negative result latches. Unknown launch outcomes must not call this.
    pub fn settle(&mut self, attempt: &Attempt, succeeded: bool) -> io::Result<()> {
        if self.unknown {
            return Err(io::Error::other("group wake state unknown"));
        }
        let mut next = self.state.clone();
        let actor = next
            .actors
            .get_mut(&attempt.actor_id)
            .ok_or_else(|| io::Error::other("actor wake state missing"))?;
        if actor.in_flight.as_ref() != Some(attempt) {
            return Err(io::Error::other("group wake attempt mismatch"));
        }
        actor.in_flight = None;
        actor.cue_pending = succeeded;
        actor.settled_attempt = succeeded.then(|| attempt.clone());
        if succeeded {
            let pending = actor
                .pending
                .as_ref()
                .filter(|pending| {
                    pending.source_event_id == attempt.source_event_id
                        && pending.generation == attempt.generation
                        && pending.policy_fingerprint == attempt.policy_fingerprint
                })
                .ok_or_else(|| io::Error::other("settled wake source missing"))?;
            actor.settled_source = Some(Watermark {
                source_event_id: pending.source_event_id.clone(),
                source_ts: pending.source_ts.clone(),
            });
        }
        if !succeeded {
            actor.pending = None;
        }
        actor.failed = !succeeded;
        self.save(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_contracts::Event;

    fn captured() -> (Pending, Attempt) {
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let pending = Pending {
            source_event_id: event.id.clone(),
            source_ts: event.ts,
            generation: "generation-one".into(),
            policy_fingerprint: "a".repeat(64),
        };
        let attempt = Attempt {
            id: "attempt-one".into(),
            group_id: event.group_id,
            actor_id: "receiver".into(),
            generation: pending.generation.clone(),
            policy_fingerprint: pending.policy_fingerprint.clone(),
            source_event_id: event.id,
        };
        (pending, attempt)
    }

    #[test]
    fn actor_limits_and_failure_latch_survive_reload() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("budget-control", "").expect("group");
        let start = Instant::now();
        let mut budget = GroupBudget::load(&home, &group.group_id, start, true).expect("load");
        let (pending, mut attempt) = captured();
        attempt.group_id = group.group_id.clone();
        budget
            .note(&attempt.actor_id, pending.clone())
            .expect("note");
        let mut changed_policy = attempt.clone(); // labeled configuration-race control
        changed_policy.policy_fingerprint = "b".repeat(64);
        assert!(
            budget
                .prepare(&changed_policy, start)
                .expect("check")
                .is_none()
        );
        let prepared = budget
            .prepare(&attempt, start)
            .expect("check")
            .expect("admitted");
        budget.commit(prepared).expect("group debit");
        budget.settle(&attempt, false).expect("known failed launch");
        budget
            .note(&attempt.actor_id, pending.clone())
            .expect("failure held");
        assert!(
            budget
                .prepare(&attempt, start + ACTOR_WINDOW)
                .expect("check")
                .is_none()
        );
        budget
            .rearm(&attempt.actor_id, &attempt.generation)
            .expect("explicit rearm");
        budget
            .note(&attempt.actor_id, pending)
            .expect("new nomination");
        assert!(budget.prepare(&attempt, start).expect("spacing").is_none());
        let restarted =
            GroupBudget::load(&home, &group.group_id, start + ACTOR_WINDOW, false).expect("reload");
        assert!(
            restarted
                .prepare(&attempt, start + ACTOR_WINDOW)
                .expect("fresh window")
                .is_none()
        );
    }

    #[test]
    fn captured_mail_replay_cannot_reserve_again_after_settlement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("replay-control", "").expect("group");
        let now = Instant::now();
        let mut budget = GroupBudget::load(&home, &group.group_id, now, true).expect("setup");
        let (pending, mut attempt) = captured();
        attempt.group_id = group.group_id;
        budget
            .note(&attempt.actor_id, pending.clone())
            .expect("nomination");
        budget
            .note(&attempt.actor_id, pending.clone())
            .expect("labeled duplicate callback");
        let prepared = budget
            .prepare(&attempt, now)
            .expect("check")
            .expect("admit");
        budget.commit(prepared).expect("debit");
        budget.settle(&attempt, true).expect("settled");
        budget
            .clear_pending(&attempt.actor_id)
            .expect("cue complete");
        budget
            .note(&attempt.actor_id, pending.clone())
            .expect("labeled replay after settlement");
        assert!(budget.pending(&attempt.actor_id).is_none());
        assert!(
            budget
                .already_settled(
                    &attempt.actor_id,
                    &pending.source_ts,
                    &pending.source_event_id
                )
                .expect("watermark")
        );
    }

    #[test]
    fn unrelated_automation_namespaces_survive_group_debits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("preservation-control", "").expect("group");
        let path = store
            .state_dir(&group.group_id)
            .expect("state dir")
            .join("automation.json");
        // Labeled state mutation; the source Mail is retained captured input.
        cccc_core::fs::write_json(
            &path,
            &json!({"rules":{"existing":{"last_fired_at":"2026-01-01T00:00:00Z"}}}),
        )
        .expect("seed");
        let now = Instant::now();
        let mut budget = GroupBudget::load(&home, &group.group_id, now, true).expect("load");
        let (pending, mut attempt) = captured();
        attempt.group_id = group.group_id;
        budget.note(&attempt.actor_id, pending).expect("note");
        let prepared = budget
            .prepare(&attempt, now)
            .expect("check")
            .expect("admitted");
        budget.commit(prepared).expect("commit");
        let doc: Value = cccc_core::fs::read_json(&path).expect("read");
        assert_eq!(
            doc["rules"]["existing"]["last_fired_at"],
            "2026-01-01T00:00:00Z"
        );
    }

    #[test]
    fn empty_namespace_loss_and_group_generation_change_are_held() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("identity-control", "").expect("group");
        let now = Instant::now();
        let mut budget = GroupBudget::load(&home, &group.group_id, now, true).expect("initialize");
        let path = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("automation.json");
        // Labeled loss of an otherwise valid empty namespace.
        cccc_core::fs::write_json(&path, &json!({})).expect("loss");
        let (pending, _) = captured();
        assert!(budget.note("receiver", pending).is_err());
        assert!(GroupBudget::load(&home, &group.group_id, now, false).is_err());
        assert!(
            cccc_core::fs::read_json::<Value>(&path)
                .expect("read")
                .get("mail_wake_v1")
                .is_none()
        );

        let other = store.create("generation-control", "").expect("group");
        let mut budget = GroupBudget::load(&home, &other.group_id, now, true).expect("initialize");
        group = other;
        group.generation = "replacement-generation-control".into(); // labeled import/replacement
        store.save(&group).expect("replace generation");
        assert!(budget.check_group(&home).is_err());
        assert!(GroupBudget::load(&home, &group.group_id, now, false).is_err());
    }

    #[test]
    fn missing_registered_group_state_is_not_reinitialized() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("budget-control", "").expect("group");
        let global_path = temp.path().join("global.json");
        super::super::budget::Budget::initialize(&global_path).expect("initialize");
        let now = Instant::now();
        GroupBudget::check_unregistered(&home, &group.group_id).expect("new group");
        let mut budget = GroupBudget::load(&home, &group.group_id, now, true).expect("group");
        assert!(GroupBudget::check_unregistered(&home, &group.group_id).is_err());
        let (pending, mut attempt) = captured();
        attempt.group_id = group.group_id.clone();
        budget.note(&attempt.actor_id, pending).expect("note");
        let prepared = budget
            .prepare(&attempt, now)
            .expect("check")
            .expect("admit");
        budget.commit(prepared).expect("debit");

        // Labeled loss of this namespace after a real captured Mail input.
        let path = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("automation.json");
        cccc_core::fs::write_json(&path, &json!({})).expect("simulate loss");
        let reloaded = super::super::budget::Budget::load(&global_path, now).expect("global");
        assert!(reloaded.in_flight().is_none());
        assert!(GroupBudget::load(&home, &group.group_id, now, false).is_err());
    }

    #[test]
    fn group_commit_uncertainty_keeps_global_debit_and_blocks_other_work() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("ordered-write-control", "").expect("group");
        let path = temp.path().join("global.json");
        super::super::budget::Budget::initialize(&path).expect("initialize");
        let now = Instant::now();
        let mut global = super::super::budget::Budget::load(&path, now).expect("global");
        let mut group_budget = GroupBudget::load(&home, &group.group_id, now, true).expect("group");
        let (pending, mut attempt) = captured();
        attempt.group_id = group.group_id.clone();
        group_budget.note(&attempt.actor_id, pending).expect("note");
        let prepared = group_budget
            .prepare(&attempt, now)
            .expect("prepare")
            .expect("allow");
        assert!(global.reserve(attempt.clone(), now).expect("global debit"));
        // Labeled lost-state mutation after the durable global debit; no
        // launch is attempted and a restarted budget must remain held.
        let state_path = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("automation.json");
        cccc_core::fs::write_json(&state_path, &json!({})).expect("simulate loss");
        assert!(group_budget.commit(prepared).is_err());
        let mut restarted = super::super::budget::Budget::load(&path, now).expect("reload");
        assert_eq!(restarted.in_flight(), Some(attempt.clone()));
        assert!(
            !restarted
                .reserve(attempt, now + GROUP_WINDOW)
                .expect("held")
        );
    }
}
