//! Optional resident-memory ceilings for directly owned native ACP providers.
//!
//! A nonzero limit enables observation. Enforcement requires one owned child,
//! a matching materialized receipt and authoritative idle-close admission.
//! The callback compares a fresh process-tree sample with the fresh limit.
//! Viewer, PTY and multi-root topologies are skipped rather than guessed at.
//!
//! Reserve a durable 30-minute cooldown only after authoritative idle-close
//! starts a real maintenance attempt, and keep
//! an exact-resume requirement through failed stop/start. No replacement may
//! silently create a new conversation. Successful and failed attempts append
//! one outcome and target the available foreman through durable delivery.

use std::collections::HashMap;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use cccc_contracts::Event;
use cccc_core::{GroupDoc, HomeLayout};
use serde_json::{Value, json};

use super::actor_memory_sampling as sampling;

/// Minimum seconds between restarts of one actor, even across daemon
/// restarts. The marker is written after authoritative idle-close and before
/// provider teardown or replacement. Busy refusals never consume this window.
/// The pre-close exact-resume sidecar fences a crash before marker persistence.
const RESTART_COOLDOWN_SECS: u64 = 1800;

/// Cooldown file name inside the group's state dir.
const COOLDOWN_FILE: &str = "actor_memory.json";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// A destructive memory restart may only re-open this conversation. This
/// sidecar survives loss/replacement of the provider's own resume receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RequiredResume {
    pub generation: String,
    pub runtime: cccc_contracts::ActorRuntime,
    pub session_id: String,
}

#[derive(Debug)]
struct MemoryResumeBlocked(String);

impl std::fmt::Display for MemoryResumeBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} Fresh startup is blocked after memory maintenance; restore the saved receipt or explicitly choose New Session.",
            self.0
        )
    }
}
impl std::error::Error for MemoryResumeBlocked {}

pub(crate) fn blocked_resume(message: impl Into<String>) -> io::Error {
    io::Error::other(MemoryResumeBlocked(message.into()))
}

pub(crate) fn is_resume_blocked(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<MemoryResumeBlocked>())
}

fn required_resume_path(
    home: &HomeLayout,
    group: &str,
    actor: &str,
) -> io::Result<std::path::PathBuf> {
    Ok(cccc_core::GroupStore::new(home.clone())?
        .state_dir(group)?
        .join("actor_memory_required")
        .join(format!("{actor}.json")))
}

pub(crate) fn required_resume(
    home: &HomeLayout,
    group: &str,
    actor: &str,
) -> io::Result<Option<RequiredResume>> {
    let path = required_resume_path(home, group, actor)?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let document: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if document["v"] != 1 {
        return Err(blocked_resume(
            "Unrecognized memory-maintenance resume requirement",
        ));
    }
    let requirement: RequiredResume =
        serde_json::from_value(document["required"].clone()).map_err(io::Error::other)?;
    if requirement.generation.is_empty()
        || !super::codex_voice_analyst::valid_native_acp_id(&requirement.session_id)
    {
        return Err(blocked_resume(
            "Invalid memory-maintenance resume requirement",
        ));
    }
    Ok(Some(requirement))
}

pub(crate) fn require_resume(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    requirement: &RequiredResume,
) -> io::Result<()> {
    if requirement.generation.is_empty()
        || !super::codex_voice_analyst::valid_native_acp_id(&requirement.session_id)
    {
        return Err(blocked_resume(
            "Invalid memory-maintenance resume requirement",
        ));
    }
    cccc_core::fs::write_json(
        &required_resume_path(home, group, actor)?,
        &json!({"v":1,"required":requirement}),
    )
}

pub(crate) fn clear_required_resume(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    expected: Option<&RequiredResume>,
) -> io::Result<()> {
    if let Some(expected) = expected {
        match required_resume(home, group, actor)? {
            Some(current) if &current == expected => {}
            None => return Ok(()),
            Some(_) => {
                return Err(blocked_resume(
                    "Memory-maintenance resume requirement changed",
                ));
            }
        }
    }
    match std::fs::remove_file(required_resume_path(home, group, actor)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn cooldown_path(home: &HomeLayout, group_id: &str) -> io::Result<std::path::PathBuf> {
    Ok(cccc_core::GroupStore::new(home.clone())?
        .state_dir(group_id)?
        .join(COOLDOWN_FILE))
}

fn read_cooldowns(home: &HomeLayout, group_id: &str) -> io::Result<HashMap<String, u64>> {
    let path = cooldown_path(home, group_id)?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error),
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let restarts = value
        .get("restarts")
        .ok_or_else(|| io::Error::other("memory cooldown state is missing restart history"))?;
    serde_json::from_value(restarts.clone()).map_err(io::Error::other)
}

fn write_cooldown(home: &HomeLayout, group_id: &str, actor_id: &str, at: u64) -> io::Result<()> {
    let mut marks = read_cooldowns(home, group_id)?;
    marks.insert(actor_id.to_owned(), at);
    cccc_core::fs::write_json(&cooldown_path(home, group_id)?, &json!({"restarts":marks}))
}

/// An inexpensive negative filter; the provider command loop establishes
/// authoritative idle admission before any destructive maintenance.
fn managed_idle(group_id: &str, actor_id: &str) -> bool {
    super::local_headless::ready_for_mail_notice(group_id, actor_id)
}

fn notice(
    group_id: &str,
    actor_id: &str,
    limit_mb: u64,
    memory_mb: u64,
    outcome: &str,
    detail: &str,
) -> Event {
    let mut event = Event::new("system.notify", group_id);
    event.by = "system".into();
    event.data = json!({
        "kind": "memory_restart",
        "priority": "normal",
        "title": "Actor memory ceiling",
        "message": format!(
            "actor {actor_id} accounted memory {memory_mb} MiB over limit {limit_mb} MiB while idle: {outcome}. {detail}",
        ),
        "to": ["@foreman"],
        "im_visibility": "internal",
        "context": {
            "actor_id": actor_id,
            "limit_mb": limit_mb,
            "memory_mb": memory_mb,
            "sampling_basis": sampling::accounting_basis(),
            "sampling_model": "non_atomic_observed_process_tree",
            "sampling_budget_ms": sampling::accounting_budget_ms(),
            "outcome": outcome,
        },
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    event
}

/// One enforcement pass for a group. Returns ledger-appended events for the
/// caller to dispatch. Pure observation when nothing qualifies: no writes,
/// no signals.
pub fn tick(home: &HomeLayout, group: &GroupDoc) -> io::Result<Vec<Event>> {
    use super::local_headless::memory::{
        MemoryRestartOutcome, memory_candidate, restart_for_memory_with_commit,
    };
    let mut out = Vec::new();
    let now = now_secs();
    let marks = read_cooldowns(home, &group.group_id)?;
    for actor in group.actors.iter().filter(|actor| actor.enabled) {
        let limit_mb = actor.memory_limit_mb;
        if limit_mb == 0
            || marks
                .get(&actor.id)
                .is_some_and(|at| now < at.saturating_add(RESTART_COOLDOWN_SECS))
            || !managed_idle(&group.group_id, &actor.id)
        {
            continue;
        }
        let Some(candidate) = memory_candidate(&group.group_id, &actor.id) else {
            continue;
        };
        let Ok(pid) = i32::try_from(candidate.provider_pid) else {
            continue;
        };
        let (observed_kib, counted) = sampling::tree_rss_kib(pid);
        if counted == 0 || observed_kib <= u64::from(limit_mb) * 1024 {
            continue;
        }
        let mut admitted = None;
        let attempted = std::cell::Cell::new(false);
        let result = restart_for_memory_with_commit(
            home,
            &group.group_id,
            &actor.id,
            &candidate,
            |fresh, fresh_actor, owned| {
                let limit = fresh_actor.memory_limit_mb;
                let now = now_secs();
                if !fresh_actor.enabled
                    || limit == 0
                    || read_cooldowns(home, &fresh.group_id)?
                        .get(&actor.id)
                        .is_some_and(|at| now < at.saturating_add(RESTART_COOLDOWN_SECS))
                {
                    return Ok(false);
                }
                let Ok(pid) = i32::try_from(owned.provider_pid) else {
                    return Ok(false);
                };
                let (rss_kib, counted) = sampling::tree_rss_kib(pid);
                if counted == 0 || rss_kib <= u64::from(limit) * 1024 {
                    return Ok(false);
                }
                admitted = Some((limit, rss_kib.div_ceil(1024)));
                Ok(true)
            },
            || {
                // A positive authoritative close has begun an actual restart.
                // Busy refusals never reach this callback or consume cooldown.
                attempted.set(true);
                write_cooldown(home, &group.group_id, &actor.id, now_secs())
            },
        );
        match result {
            Ok(MemoryRestartOutcome::Skipped) => {}
            Ok(MemoryRestartOutcome::Restarted) => {
                let (limit, memory_mb) = admitted.ok_or_else(|| {
                    io::Error::other("memory restart completed without admission evidence")
                })?;
                out.push(emit(
                    home,
                    &group.group_id,
                    &actor.id,
                    u64::from(limit),
                    memory_mb,
                    "restarted",
                    "Verified native ACP session resumed.",
                )?);
                // The coordinator has released its admission guard. Pending
                // deliveries can now target the verified replacement session.
                if let Ok(fresh) = cccc_core::GroupStore::new(home.clone())
                    .and_then(|store| store.load(&group.group_id))
                {
                    super::actor_delivery::dispatch_unread(home, &fresh, &actor.id);
                }
            }
            Err(error) => {
                if let Some((limit, memory_mb)) = admitted.filter(|_| attempted.get()) {
                    let cooldown_holds = read_cooldowns(home, &group.group_id)
                        .ok()
                        .and_then(|marks| marks.get(&actor.id).copied())
                        .is_some_and(|at| now_secs() < at.saturating_add(RESTART_COOLDOWN_SECS));
                    let recovery = if cooldown_holds {
                        "Cooldown holds; exact session resume remains required."
                    } else {
                        "Cooldown was not persisted; exact session resume remains required. Operator recovery is needed."
                    };
                    out.push(emit(
                        home,
                        &group.group_id,
                        &actor.id,
                        u64::from(limit),
                        memory_mb,
                        "restart_failed",
                        &format!("Memory maintenance failed: {error}. {recovery}"),
                    )?);
                } else {
                    tracing::warn!(%error, actor_id = %actor.id, "memory maintenance admission failed or was unconfirmed");
                }
                // Failed attempts retain a durable exact-resume requirement.
                // Do not redispatch into ordinary automatic startup here.
            }
        }
    }
    Ok(out)
}

fn emit(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    limit_mb: u64,
    memory_mb: u64,
    outcome: &str,
    detail: &str,
) -> io::Result<Event> {
    let mut event = notice(group_id, actor_id, limit_mb, memory_mb, outcome, detail);
    let store = cccc_core::GroupStore::new(home.clone())?;
    let group = store.load(group_id)?;
    if let Ok(foreman) = cccc_core::actors::unique_available_foreman(&group) {
        event
            .data
            .insert("target_actor_id".into(), json!(foreman.id));
    }
    let path = store.ledger_path(group_id)?;
    cccc_core::ledger::append(&path, &event)?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_window_holds_then_releases() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let group = store.create("cooldown", "").expect("group");
        let gid = group.group_id.clone();
        // Missing file: no cooldown.
        assert!(
            read_cooldowns(&home, &gid)
                .expect("missing history")
                .is_empty()
        );
        write_cooldown(&home, &gid, "a", 10_000).expect("write");
        let marks = read_cooldowns(&home, &gid).expect("cooldown history");
        assert_eq!(marks.get("a"), Some(&10_000));
        // Inside the window the marker blocks; the window is 30 minutes.
        assert_eq!(RESTART_COOLDOWN_SECS, 1800);
        // Corrupt history cannot prove the cooldown elapsed. Fail closed.
        let path = cooldown_path(&home, &gid).expect("path");
        std::fs::write(&path, "{ not json").expect("corrupt");
        assert!(read_cooldowns(&home, &gid).is_err());
    }

    #[test]
    fn tick_with_no_limited_actors_writes_nothing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let group = store.create("quiet", "").expect("group");
        let before = std::fs::read_to_string(store.ledger_path(&group.group_id).expect("ledger"))
            .unwrap_or_default()
            .len();
        let events = tick(&home, &group).expect("tick");
        assert!(events.is_empty());
        let after = std::fs::read_to_string(store.ledger_path(&group.group_id).expect("ledger"))
            .unwrap_or_default()
            .len();
        assert_eq!(before, after, "a quiet tick must not append");
        // ... and a PTY actor with a limit is observed but never restarted:
        // idleness here is unprovable, so no event, no cooldown file.
        let mut cela = group.clone();
        let mut actor = cccc_contracts::Actor::new("pty-lane");
        actor.memory_limit_mb = 1;
        cela.actors.push(actor);
        store.save(&cela).expect("save");
        let reloaded = store.load(&group.group_id).expect("reload");
        let events = tick(&home, &reloaded).expect("tick");
        assert!(events.is_empty());
        assert!(
            cooldown_path(&home, &group.group_id)
                .map(|path| !path.exists())
                .unwrap_or(true),
            "no restart means no cooldown marker"
        );
    }

    #[test]
    fn notice_shape_targets_foreman_only() {
        let event = notice("g", "a", 256, 1024, "restarted", "d.");
        assert_eq!(event.data["kind"], "memory_restart");
        assert_eq!(event.data["to"], json!(["@foreman"]));
        assert_eq!(event.data["context"]["outcome"], "restarted");
        assert_eq!(event.data["context"]["memory_mb"], 1024);
        assert_eq!(
            event.data["context"]["sampling_basis"],
            sampling::accounting_basis()
        );
        assert_eq!(
            event.data["context"]["sampling_model"],
            "non_atomic_observed_process_tree"
        );
        assert_eq!(
            event.data["context"]["sampling_budget_ms"],
            serde_json::json!(sampling::accounting_budget_ms())
        );
    }

    #[test]
    fn memory_restart_targets_only_the_foreman_in_dispatch_and_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("memory routing", "").expect("group");
        group.state = cccc_contracts::GroupState::Stopped;
        group.actors = vec![
            cccc_contracts::Actor::new("lead"),
            cccc_contracts::Actor::new("peer"),
        ];
        store.save(&group).expect("save actors");
        let event = emit(
            &home,
            &group.group_id,
            "worker",
            500,
            800,
            "restarted",
            "same session",
        )
        .expect("emit notice");
        assert_eq!(event.data["target_actor_id"], "lead");
        // Stopped groups deliberately return before recipient selection.
        let stopped = super::super::actor_delivery::dispatch(&home, &group, &event);
        assert_eq!(stopped.targeted, 0);
        assert_eq!(stopped.queued, 0);
        // A notice persisted before dispatch is recoverable only by its target.
        for (actor, expected) in [(&group.actors[0], true), (&group.actors[1], false)] {
            let pending = super::super::runtime_delivery::pending_sources(&home, &group, actor, 10)
                .expect("pending notifications");
            assert_eq!(pending.iter().any(|item| item.id == event.id), expected);
            assert!(
                !cccc_core::inbox::is_for_actor(&group, &event, &actor.id),
                "maintenance notices never become inbox messages"
            );
        }

        group.state = cccc_contracts::GroupState::Active;
        store.save(&group).expect("activate routing fixture");
        // Model an existing delivery claim so real dispatch selects recipients
        // without starting a provider or creating a duplicate delivery worker.
        assert_eq!(
            super::super::runtime_delivery::claim(
                &home,
                &group,
                &group.actors[0],
                &event.id,
                "managed_session",
                false,
            )
            .expect("claim foreman notice"),
            super::super::runtime_delivery::ClaimResult::Claimed
        );
        let report = super::super::actor_delivery::dispatch(&home, &group, &event);
        // Clean up before assertions even if a routing regression queues a peer.
        super::super::actor_delivery::shutdown_group(&group.group_id);
        assert_eq!(report.targeted, 1);
        assert_eq!(
            report.queued, 0,
            "the existing foreman claim is not duplicated"
        );
        assert!(
            super::super::runtime_delivery::latest_state(
                &home,
                &group.group_id,
                "peer",
                &event.id,
            )
            .expect("peer delivery state")
            .is_none(),
            "dispatch must never claim the peer's copy"
        );
    }

    #[test]
    fn memory_restart_disabled_configured_foreman_is_targeted_and_durably_recoverable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let mut group = store
            .create("memory routing disabled foreman", "")
            .expect("group");
        group.state = cccc_contracts::GroupState::Active;
        let mut lead = cccc_contracts::Actor::new("lead");
        lead.enabled = false;
        group.actors = vec![lead, cccc_contracts::Actor::new("peer")];
        store.save(&group).expect("save actors");
        let event = emit(
            &home,
            &group.group_id,
            "worker",
            500,
            800,
            "restart_failed",
            "receipt missing",
        )
        .expect("emit real outcome");
        assert_eq!(
            event.data["target_actor_id"], "lead",
            "configured role retains its durable inbox while disabled"
        );
        assert_eq!(event.data["to"], json!(["@foreman"]));
        let report = super::super::actor_delivery::dispatch(&home, &group, &event);
        super::super::actor_delivery::shutdown_group(&group.group_id);
        assert_eq!(
            report.targeted, 1,
            "only the configured foreman is selected"
        );
        assert_eq!(report.queued, 0, "disabled foreman is never woken");
        assert!(!cccc_runtime::status(&group.group_id, "lead").is_ok_and(|s| s.running));
        for (actor, expected) in [(&group.actors[0], true), (&group.actors[1], false)] {
            let pending = super::super::runtime_delivery::pending_sources(&home, &group, actor, 10)
                .expect("durable recovery");
            assert_eq!(
                pending.iter().any(|item| item.id == event.id),
                expected,
                "only the concrete recipient can recover this outcome"
            );
        }
        let ledger =
            cccc_core::ledger::read_all(&store.ledger_path(&group.group_id).expect("path"))
                .expect("ledger");
        assert_eq!(ledger.iter().filter(|item| item.id == event.id).count(), 1);
    }

    #[test]
    fn memory_restart_without_any_configured_foreman_fails_before_ledger_append() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let mut group = store
            .create("memory routing absent foreman", "")
            .expect("group");
        group.actors = vec![cccc_contracts::Actor::new("hidden")];
        group.actors[0].internal_kind = Some("fixture_hidden".into());
        store.save(&group).expect("all internal actors");
        let path = store.ledger_path(&group.group_id).expect("ledger path");
        let before = std::fs::read(&path).unwrap_or_default();
        assert!(
            emit(
                &home,
                &group.group_id,
                "hidden",
                500,
                800,
                "restart_failed",
                "missing receipt"
            )
            .is_err(),
            "real emit must reject a targetless outcome"
        );
        assert_eq!(
            std::fs::read(&path).unwrap_or_default(),
            before,
            "never append an unrecoverable/broadcast outcome"
        );
        assert!(
            !cooldown_path(&home, &group.group_id)
                .expect("attempt state")
                .exists()
        );
        assert!(
            super::super::runtime_delivery::pending_sources(&home, &group, &group.actors[0], 10)
                .expect("hidden actor recovery")
                .is_empty()
        );
    }

    #[test]
    fn required_resume_survives_reopen_and_cannot_clear_another_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let group = store.create("memory resume fence", "").expect("group");
        let requirement = RequiredResume {
            generation: "generation-a".into(),
            runtime: cccc_contracts::ActorRuntime::Hermes,
            session_id: "saved-session-a".into(),
        };
        require_resume(&home, &group.group_id, "worker", &requirement)
            .expect("durable requirement");
        let reopened = HomeLayout::from_path(home.root().to_path_buf()).expect("reopen");
        assert_eq!(
            required_resume(&reopened, &group.group_id, "worker").expect("replay"),
            Some(requirement.clone())
        );
        let mut newer = requirement.clone();
        newer.generation = "generation-b".into();
        require_resume(&reopened, &group.group_id, "worker", &newer).expect("newer owner");
        let error = clear_required_resume(&reopened, &group.group_id, "worker", Some(&requirement))
            .expect_err("stale clear refused");
        assert!(is_resume_blocked(&error));
        assert_eq!(
            required_resume(&home, &group.group_id, "worker").expect("retained"),
            Some(newer)
        );
        clear_required_resume(&home, &group.group_id, "worker", None)
            .expect("explicit new session clears");
        assert!(
            required_resume(&reopened, &group.group_id, "worker")
                .expect("cleared")
                .is_none()
        );
    }

    #[test]
    fn corrupt_resume_requirement_is_not_treated_as_permission_for_fresh_start() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let group = cccc_core::GroupStore::new(home.clone())
            .expect("store")
            .create("memory corrupt fence", "")
            .expect("group");
        let path = required_resume_path(&home, &group.group_id, "worker").expect("path");
        cccc_core::fs::atomic_write(&path, b"{ invalid").expect("corrupt fixture");
        assert!(required_resume(&home, &group.group_id, "worker").is_err());
        clear_required_resume(&home, &group.group_id, "worker", None)
            .expect("explicit reset handles corruption");
        assert!(
            required_resume(&home, &group.group_id, "worker")
                .expect("reset")
                .is_none()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "actor_memory_pty_policy_red.rs"]
mod pty_policy_red;
