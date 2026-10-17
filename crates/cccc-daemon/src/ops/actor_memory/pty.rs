//! cf959bff PTY policy, applied to every runtime outside native ACP maintenance.
//! Quiet is an output/input heuristic. It is not a provider idle/ownership fence.
use super::*;
use cccc_contracts::{Actor, ActorRole};
use cccc_runtime::ActivitySnapshot;
use std::time::Duration;

pub(super) const PTY_QUIET_SECS: u64 = 600;
const DETECTION_FILE: &str = "actor_memory_detection.json";

pub(crate) fn is_native_acp(actor: &Actor) -> bool {
    super::super::codex_voice_analyst::AnalystSession::supports_memory_restart(
        actor.runtime,
        actor.runtime_mode,
    )
}

/// Literal cf959bff predicate: silence must be observed, with no newer input.
pub(super) fn pty_quiet_for(snapshot: &ActivitySnapshot, quiet: Duration) -> bool {
    let Some(output) = &snapshot.last_output else {
        return false;
    };
    if output.age < quiet {
        return false;
    }
    match &snapshot.last_input {
        None => true,
        Some(input) => input.age >= output.age,
    }
}

pub(crate) fn quiet_live(group_id: &str, actor_id: &str) -> bool {
    cccc_runtime::activity(group_id, actor_id)
        .map(|snapshot| {
            #[cfg(all(test, target_os = "linux"))]
            let snapshot = tests::aged_for_test(group_id, actor_id, snapshot);
            pty_quiet_for(&snapshot, Duration::from_secs(PTY_QUIET_SECS))
        })
        .unwrap_or(false)
}

/// Role is configuration, not liveness. A disabled/down foreman still owns its
/// durable inbox; producing this record never starts or broadcasts to an actor.
pub(super) fn configured_foreman(group: &GroupDoc) -> io::Result<&Actor> {
    let mut actors = cccc_core::actors::visible(group)
        .filter(|a| cccc_core::actors::effective_role(group, &a.id) == Some(ActorRole::Foreman));
    let first = actors
        .next()
        .ok_or_else(|| io::Error::other("Memory maintenance has no configured visible foreman"))?;
    if actors.next().is_some() {
        return Err(io::Error::other(
            "Memory maintenance configured foreman is ambiguous",
        ));
    }
    Ok(first)
}

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct DetectionMark {
    at: u64,
    pending: Option<Event>,
}

/// Last emitted UNAVAILABLE notice per actor, keyed by the stable
/// (outcome, detail) pair — never the rendered message (which embeds the
/// volatile limit). Stored in its own file so the over-class window stamp
/// (`DetectionMark.at`) is never touched by unavailable traffic and vice
/// versa: a repeat of the recorded pair stays silent forever with no clock
/// refresh, while any change re-notifies. Recovery clears entries; nothing
/// else writes them.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct UnavailableState {
    outcome: String,
    detail: String,
}
const UNAVAILABLE_FILE: &str = "actor_memory_unavailable.json";

fn read_unavailable_states(
    path: &std::path::Path,
) -> io::Result<HashMap<String, UnavailableState>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(error),
    }
}

/// Clear a recorded unavailable state after genuinely healthy observation
/// (attributable AND under limit). Silent: no event. Disabled and limit-zero
/// actors are deliberately NOT cleared here — without an observation there is
/// no recovery evidence, and a re-enabled actor failing identically is the
/// same state (silence is correct, not a miss).
fn clear_unavailable_state(home: &HomeLayout, group_id: &str, actor_id: &str) -> io::Result<()> {
    let store = cccc_core::GroupStore::new(home.clone())?;
    let path = store.state_dir(group_id)?.join(UNAVAILABLE_FILE);
    // Fast path: no history file, nothing to clear — and no lock file or
    // parent dirs are created for the common healthy case.
    if !path.exists() {
        return Ok(());
    }
    cccc_core::fs::with_exclusive_lock(&path.with_extension("lock"), || {
        let mut states = read_unavailable_states(&path)?;
        if states.remove(actor_id).is_some() {
            cccc_core::fs::write_json_committed(&path, &states)?;
        }
        Ok(())
    })
}

/// This file is deliberately separate from real restart-attempt cooldown.
/// Pending event ID is persisted before append, then reconciled via ledger
/// lookup after failure/crash. A committed append never gets a second event ID.
fn emit_detection(home: &HomeLayout, group: &GroupDoc, event: Event) -> io::Result<Option<Event>> {
    let store = cccc_core::GroupStore::new(home.clone())?;
    let path = store.state_dir(&group.group_id)?.join(DETECTION_FILE);
    let ledger = store.ledger_path(&group.group_id)?;
    let actor = event.data["context"]["actor_id"]
        .as_str()
        .ok_or_else(|| io::Error::other("Memory notice actor is missing"))?
        .to_owned();
    cccc_core::fs::with_exclusive_lock(&path.with_extension("lock"), || {
        let mut marks: HashMap<String, DetectionMark> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e),
        };
        let unavailable_path = store.state_dir(&group.group_id)?.join(UNAVAILABLE_FILE);
        let mut states = read_unavailable_states(&unavailable_path)?;
        let now = now_secs();
        // State-change key from the event's own context: a stable
        // (outcome, detail) pair. The `detail` is a `&'static str` reason at
        // every report() call site — never the rendered message.
        let incoming_outcome = event.data["context"]["outcome"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let incoming_detail = event.data["context"]["detail"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let incoming_unavailable = incoming_outcome == "detection_unavailable";
        // Pending-event recovery first (existing semantics for both classes):
        // a crash between persist and append re-offers the recorded event.
        if let Some(record) = marks.get(&actor).and_then(|mark| mark.pending.clone()) {
            if event.data["target_actor_id"]
                .as_str()
                .is_none_or(str::is_empty)
            {
                return Err(io::Error::other(
                    "Memory detection notice has no concrete durable foreman target",
                ));
            }
            let at = marks.get(&actor).map(|mark| mark.at).unwrap_or(now);
            let appended = cccc_core::ledger::find_event(&ledger, &record.id)?.is_none();
            if appended {
                cccc_core::ledger::append(&ledger, &record)?;
            }
            // Only the pending flag clears; the over-class window stamp and
            // any recorded unavailable state survive the rewrite untouched.
            if let Some(mark) = marks.get_mut(&actor) {
                mark.pending = None;
            }
            cccc_core::fs::write_json_committed(&path, &marks)?;
            return Ok(appended.then_some(record));
        }
        if incoming_unavailable {
            let same = states.get(&actor).is_some_and(|state| {
                state.outcome == incoming_outcome && state.detail == incoming_detail
            });
            if same {
                // Repeat of the recorded unavailable state: silent forever,
                // with no clock refresh (refreshing `at` would re-arm spam)
                // and no state write.
                return Ok(None);
            }
        } else if let Some(mark) = marks.get(&actor) {
            // Over-class (and any other non-unavailable outcome): the
            // pre-existing per-actor window on `at`, untouched.
            if now < mark.at.saturating_add(RESTART_COOLDOWN_SECS) {
                return Ok(None);
            }
        }
        if event.data["target_actor_id"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(io::Error::other(
                "Memory detection notice has no concrete durable foreman target",
            ));
        }
        // Mint through the crash-safe pending flow. The over-class window
        // stamp advances ONLY on over-class emits: unavailable traffic
        // preserves the existing stamp (or zero when there never was one),
        // so an unavailable streak can never consume the over-limit window.
        let at = if incoming_unavailable {
            marks.get(&actor).map(|mark| mark.at).unwrap_or(0)
        } else {
            now
        };
        marks.insert(
            actor.clone(),
            DetectionMark {
                at,
                pending: Some(event.clone()),
            },
        );
        cccc_core::fs::write_json_committed(&path, &marks)?;
        let appended = cccc_core::ledger::find_event(&ledger, &event.id)?.is_none();
        if appended {
            cccc_core::ledger::append(&ledger, &event)?;
        }
        marks.insert(actor.clone(), DetectionMark { at, pending: None });
        cccc_core::fs::write_json_committed(&path, &marks)?;
        if incoming_unavailable {
            states.insert(
                actor,
                UnavailableState {
                    outcome: incoming_outcome,
                    detail: incoming_detail,
                },
            );
            cccc_core::fs::write_json_committed(&unavailable_path, &states)?;
        }
        Ok(appended.then_some(event))
    })
}

/// Recover a pending notice even if the actor subsequently stopped, went below
/// the limit, or opted out. Otherwise a crash before append could strand the
/// observed event indefinitely behind an unrelated future sampling condition.
fn recover_pending(home: &HomeLayout, group: &GroupDoc) -> io::Result<Vec<Event>> {
    let store = cccc_core::GroupStore::new(home.clone())?;
    let path = store.state_dir(&group.group_id)?.join(DETECTION_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let ledger = store.ledger_path(&group.group_id)?;
    cccc_core::fs::with_exclusive_lock(&path.with_extension("lock"), || {
        let mut marks: HashMap<String, DetectionMark> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut appended = Vec::new();
        let mut changed = false;
        for mark in marks.values_mut() {
            let Some(record) = mark.pending.as_ref() else {
                continue;
            };
            if record.data["target_actor_id"]
                .as_str()
                .is_none_or(str::is_empty)
            {
                return Err(io::Error::other(
                    "Pending memory detection has no concrete durable target",
                ));
            }
            if cccc_core::ledger::find_event(&ledger, &record.id)?.is_none() {
                cccc_core::ledger::append(&ledger, record)?;
            }
            // The previous append may have committed before its caller could
            // receive the event. Re-offer the same identity once; runtime delivery
            // claims, rather than allocating a new notice, arbitrate submission.
            appended.push(record.clone());
            mark.pending = None;
            changed = true;
        }
        if changed {
            cccc_core::fs::write_json_committed(&path, &marks)?;
        }
        Ok(appended)
    })
}

fn reserve_pty_attempt(home: &HomeLayout, group: &str, actor: &str, at: u64) -> io::Result<()> {
    let mut marks = read_cooldowns(home, group)?;
    marks.insert(actor.to_owned(), at);
    // Atomic rename may commit before a directory-sync error is returned.
    // Readback equivalence makes that committed reservation unambiguous.
    cccc_core::fs::write_json_committed(&cooldown_path(home, group)?, &json!({"restarts":marks}))
}

fn report(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    sample: Option<&observation::Observation>,
    outcome: &str,
    detail: &str,
    action: &str,
) -> io::Result<Option<Event>> {
    let foreman = configured_foreman(group)?;
    let mut event = Event::new("system.notify", &group.group_id);
    event.by = "system".into();
    let memory_mb = sample.map(|s| s.total_kib.div_ceil(1024));
    let message = match memory_mb {
        Some(memory) => format!(
            "actor {} accounted memory {memory} MiB over limit {} MiB: {detail}",
            actor.id, actor.memory_limit_mb
        ),
        None => format!(
            "actor {} memory detection unavailable (limit {} MiB): {detail}",
            actor.id, actor.memory_limit_mb
        ),
    };
    event.data = json!({"kind":"memory_restart","priority":"normal","title":"Actor memory ceiling",
        "message":message,"to":["@foreman"],"target_actor_id":foreman.id,"im_visibility":"internal",
        "context":{"actor_id":actor.id,"limit_mb":actor.memory_limit_mb,"memory_mb":memory_mb,
            "outcome":outcome,"detail":detail,"action":action,"measurement_roots":sample.map(|s|s.basis),
            "sampled_processes":sample.map(|s|s.counted),
            "sampling_model":"non_atomic_observed_process_tree","sampling_budget_ms":Value::Null}})
    .as_object()
    .cloned()
    .unwrap_or_default();
    // Inherit the platform basis/budget from the native notice constructor so
    // the independent Darwin sampler patch has a single accounting authority.
    let accounting = notice(
        &group.group_id,
        &actor.id,
        u64::from(actor.memory_limit_mb),
        memory_mb.unwrap_or(0),
        outcome,
        detail,
    );
    if let Some(context) = event.data.get_mut("context").and_then(Value::as_object_mut) {
        for key in ["sampling_basis", "sampling_model", "sampling_budget_ms"] {
            if let Some(value) = accounting.data["context"].get(key) {
                context.insert(key.into(), value.clone());
            }
        }
    }
    if action == "detection_only" {
        emit_detection(home, group, event)
    } else {
        let store = cccc_core::GroupStore::new(home.clone())?;
        cccc_core::ledger::append(&store.ledger_path(&group.group_id)?, &event)?;
        Ok(Some(event))
    }
}

pub(super) fn tick_detection(home: &HomeLayout, group: &GroupDoc) -> io::Result<Vec<Event>> {
    let mut events = recover_pending(home, group)?;
    for stored in group
        .actors
        .iter()
        .filter(|a| a.enabled && a.memory_limit_mb != 0)
    {
        // A real attempt already reported this actor in this window. Keep this
        // check ahead of profile/sample diagnostics as well as over-limit reports.
        // Previously persisted pending events were recovered before this loop.
        if read_cooldowns(home, &group.group_id)?
            .get(&stored.id)
            .is_some_and(|at| now_secs() < at.saturating_add(RESTART_COOLDOWN_SECS))
        {
            continue;
        }
        let actor = match super::super::actor_profile_runtime::resolve(home, stored) {
            Ok(actor) => actor,
            Err(_) => {
                if let Some(e) = report(
                    home,
                    group,
                    stored,
                    None,
                    "detection_unavailable",
                    "Current runtime profile could not be resolved; no automatic restart.",
                    "detection_only",
                )? {
                    events.push(e);
                }
                continue;
            }
        };
        let sampled = observation::observe(home, group, &actor);
        let sample = match sampled {
            Ok(Some(sample)) => sample,
            Ok(None) => continue,
            Err(reason) => {
                if let Some(e) = report(
                    home,
                    group,
                    &actor,
                    None,
                    "detection_unavailable",
                    reason,
                    "detection_only",
                )? {
                    events.push(e);
                }
                continue;
            }
        };
        if sample.total_kib <= u64::from(actor.memory_limit_mb) * 1024 {
            // Observed healthy (attributable AND under limit): silently clear
            // any recorded unavailable state. The over-class window stamp is
            // untouched, and disabled/limit-zero actors are deliberately not
            // cleared (no observation, no recovery evidence).
            clear_unavailable_state(home, &group.group_id, &actor.id)?;
            continue;
        }
        let required_block = if is_native_acp(&actor) {
            None
        } else {
            match required_resume(home, &group.group_id, &actor.id) {
                Ok(Some(_)) => Some(
                    "A durable native exact-resume obligation blocks PTY fresh restart; explicit New Session is required to abandon it.",
                ),
                Err(_) => Some(
                    "The durable native exact-resume obligation is unreadable; no automatic fresh restart.",
                ),
                Ok(None) => None,
            }
        };
        let held = if is_native_acp(&actor) {
            Some("Native ACP exact-session idle admission was unavailable; no automatic restart.")
        } else if !group.running || group.state != cccc_contracts::GroupState::Active {
            Some("Group is not active/running; no automatic restart.")
        } else if !actor.memory_limit_restart_pty {
            Some(
                "PTY fresh restart is not opted in (memory_limit_restart_pty=false); no automatic restart.",
            )
        } else if required_block.is_some() {
            required_block
        } else if !quiet_live(&group.group_id, &actor.id) {
            Some(
                "PTY has not observed ten minutes of output silence without newer input; no automatic restart.",
            )
        } else {
            None
        };
        if let Some(reason) = held {
            if let Some(e) = report(
                home,
                group,
                &actor,
                Some(&sample),
                "over_limit",
                reason,
                "detection_only",
            )? {
                events.push(e);
            }
            continue;
        }
        // Require a concrete durable recipient before any destructive attempt.
        configured_foreman(group)?;
        let attempted = std::cell::Cell::new(false);
        let mut admitted = None;
        let result = super::super::actor_runtime::restart_for_memory_pty(
            home,
            &group.group_id,
            &actor.id,
            &sample,
            |fresh, fresh_actor| {
                if fresh_actor.memory_limit_mb == 0
                    || read_cooldowns(home, &fresh.group_id)?
                        .get(&fresh_actor.id)
                        .is_some_and(|at| now_secs() < at.saturating_add(RESTART_COOLDOWN_SECS))
                {
                    return Ok(false);
                }
                configured_foreman(fresh)?;
                let Some(current) = observation::observe(home, fresh, fresh_actor)
                    .ok()
                    .flatten()
                else {
                    return Ok(false);
                };
                if current.identity != sample.identity
                    || current.total_kib <= u64::from(fresh_actor.memory_limit_mb) * 1024
                {
                    return Ok(false);
                }
                admitted = Some((fresh.clone(), fresh_actor.clone(), current));
                Ok(true)
            },
            || {
                reserve_pty_attempt(home, &group.group_id, &actor.id, now_secs())?;
                attempted.set(true);
                Ok(())
            },
        );
        match result {
            Ok(super::super::actor_runtime::FreshOutcome::Restarted) => {
                let (fresh, actor, sample) = admitted.ok_or_else(|| {
                    io::Error::other("Fresh PTY restart lacked admission evidence")
                })?;
                if let Some(e) = report(
                    home,
                    &fresh,
                    &actor,
                    Some(&sample),
                    "restarted",
                    "fresh restart (no resume)",
                    "pty_fresh_restart",
                )? {
                    events.push(e);
                }
                super::super::actor_delivery::dispatch_unread(home, &fresh, &actor.id);
            }
            result => {
                let detail = if attempted.get() {
                    "PTY fresh restart (no resume) attempt failed; restart cooldown holds when persisted. No provider-idle claim is made."
                } else {
                    "PTY fresh restart admission changed or was refused; no automatic restart."
                };
                let outcome = if attempted.get() {
                    "restart_failed"
                } else {
                    "over_limit"
                };
                let action = if attempted.get() {
                    "pty_fresh_restart"
                } else {
                    "detection_only"
                };
                if let Err(ref error) = result {
                    tracing::warn!(kind=?error.kind(),actor_id=%actor.id,"PTY memory maintenance failed");
                }
                // Once admission selected fresh settings and a recipient, both
                // success and failed-attempt notices must retain that binding.
                // An early refusal still reports the original observation.
                let (report_group, report_actor, report_sample) = admitted
                    .as_ref()
                    .map(|(fresh, actor, sample)| (fresh, actor, sample))
                    .unwrap_or((group, &actor, &sample));
                if let Some(e) = report(
                    home,
                    report_group,
                    report_actor,
                    Some(report_sample),
                    outcome,
                    detail,
                    action,
                )? {
                    events.push(e);
                }
            }
        }
    }
    Ok(events)
}

#[cfg(all(test, target_os = "linux"))]
#[path = "pty_tests.rs"]
mod tests;

// Tests-only owner MUST1/MUST2 policy regressions; no production change here.
#[cfg(all(test, target_os = "linux"))]
#[path = "memory_musts_notice_tests.rs"]
mod memory_musts_notice;
