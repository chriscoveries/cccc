//! Rust-side DeepSeek process lifecycle registry.
mod delivery;
mod delivery_projection;
mod launch_command;
mod lifecycle;
mod recovery;
mod turn_failure;
mod turn_timeout;

pub use delivery::{TurnDelivery, deliver};
pub use lifecycle::apply;
pub(super) use lifecycle::apply_wake;

use cccc_contracts::Actor;
use cccc_core::{GroupDoc, HomeLayout};
use cccc_runtime::deepseek_supervisor::DeepSeekSupervisor;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::Duration;

type Key = (String, String);

pub(super) struct RuntimeEntry {
    pub(super) supervisor: std::sync::Mutex<DeepSeekSupervisor>,
    pub(super) running: AtomicBool,
    pub(super) manual_restart_required: AtomicBool,
    pub(super) generation: String,
    wake_origin: Option<cccc_core::deepseek_restart_gate::WakeOrigin>,
    workspace: super::actor_runtime::LaunchWorkspace,
}

fn lifecycle_gate(key: &Key) -> std::io::Result<Arc<Mutex<()>>> {
    static GATES: OnceLock<Mutex<HashMap<Key, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| std::io::Error::other("deepseek lifecycle gate poisoned"))?;
    gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = gates.get(key).and_then(Weak::upgrade) {
        return Ok(gate);
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(key.clone(), Arc::downgrade(&gate));
    Ok(gate)
}

pub(super) fn sessions() -> &'static RwLock<HashMap<Key, Arc<RuntimeEntry>>> {
    static SESSIONS: OnceLock<RwLock<HashMap<Key, Arc<RuntimeEntry>>>> = OnceLock::new();
    SESSIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn cancel_flags() -> &'static RwLock<HashMap<Key, Arc<AtomicBool>>> {
    static FLAGS: OnceLock<RwLock<HashMap<Key, Arc<AtomicBool>>>> = OnceLock::new();
    FLAGS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn start(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    cwd: &Path,
) -> std::io::Result<()> {
    start_owned(home, group, actor, cwd, None)
}

fn start_for_wake(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    cwd: &Path,
    origin: &cccc_core::deepseek_restart_gate::WakeOrigin,
) -> std::io::Result<()> {
    if origin.actor_generation != cccc_core::actors::generation_identity(actor) {
        return Err(std::io::Error::other(
            "deepseek wake actor generation changed",
        ));
    }
    start_owned(home, group, actor, cwd, Some(origin))
}

fn start_owned(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    cwd: &Path,
    wake_origin: Option<&cccc_core::deepseek_restart_gate::WakeOrigin>,
) -> std::io::Result<()> {
    let cwd = cwd.canonicalize()?;
    let key = (group.group_id.clone(), actor.id.clone());
    let gate = lifecycle_gate(&key)?;
    let _guard = gate
        .lock()
        .map_err(|_| std::io::Error::other("deepseek lifecycle gate poisoned"))?;
    if wake_origin.is_some() {
        let occupied = sessions()
            .read()
            .map_err(|_| std::io::Error::other("deepseek session lock poisoned"))?
            .contains_key(&key)
            || cancel_flags()
                .read()
                .map_err(|_| std::io::Error::other("deepseek cancel lock poisoned"))?
                .contains_key(&key);
        if occupied {
            return Err(std::io::Error::other(
                "deepseek launch owner already exists",
            ));
        }
    } else {
        stop_locked(&group.group_id, &actor.id);
    }
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let generation = uuid::Uuid::new_v4().simple().to_string();
    let mut supervisor = DeepSeekSupervisor::default();
    let mut env = actor.env.clone();
    env.insert(
        "CCCC_HOME".into(),
        home.root().to_string_lossy().into_owned(),
    );
    env.insert("CCCC_GROUP_ID".into(), group.group_id.clone());
    env.insert("CCCC_ACTOR_ID".into(), actor.id.clone());
    let session_root = home
        .root()
        .join("groups")
        .join(&group.group_id)
        .join("state/deepseek")
        .join(&actor.id)
        .join("sessions");
    std::fs::create_dir_all(&session_root)?;
    env.insert(
        "CCCC_DEEPSEEK_SESSION_ROOT".into(),
        session_root.to_string_lossy().into_owned(),
    );
    let command = launch_command::resolve(actor, &env)?;
    let env = env.into_iter().collect::<Vec<_>>();
    supervisor
        .start(&command, &cwd, &env)
        .map_err(std::io::Error::other)?;
    if let Err(error) = supervisor.handshake(&cwd, Duration::from_secs(5)) {
        let _ = supervisor.stop();
        return Err(std::io::Error::other(format!(
            "deepseek ACP handshake failed: {error}"
        )));
    }
    sessions()
        .write()
        .map_err(|_| std::io::Error::other("deepseek session lock poisoned"))?
        .insert(
            key,
            Arc::new(RuntimeEntry {
                supervisor: std::sync::Mutex::new(supervisor),
                running: AtomicBool::new(true),
                manual_restart_required: AtomicBool::new(false),
                generation: generation.clone(),
                wake_origin: wake_origin.cloned(),
                workspace: super::actor_runtime::LaunchWorkspace::capture(group, actor, &cwd),
            }),
        );
    cancel_flags()
        .write()
        .map_err(|_| std::io::Error::other("deepseek cancel lock poisoned"))?
        .insert((group.group_id.clone(), actor.id.clone()), cancel_flag);
    if let Err(error) = cccc_core::deepseek_restart_gate::record_running_generation_with_wake(
        home,
        &group.group_id,
        &actor.id,
        &actor.created_at,
        &generation,
        wake_origin,
    ) {
        stop_locked(&group.group_id, &actor.id);
        return Err(error);
    }
    Ok(())
}

pub fn stop(group_id: &str, actor_id: &str) {
    let key = (group_id.to_owned(), actor_id.to_owned());
    let Ok(gate) = lifecycle_gate(&key) else {
        return;
    };
    let Ok(_guard) = gate.lock() else { return };
    stop_locked(group_id, actor_id);
}

fn stop_locked(group_id: &str, actor_id: &str) {
    let key = (group_id.to_owned(), actor_id.to_owned());
    if let Ok(flags) = cancel_flags().read() {
        if let Some(flag) = flags.get(&key) {
            flag.store(true, Ordering::Release);
        }
    }
    if let Some(holder) = sessions().write().ok().and_then(|mut map| map.remove(&key)) {
        holder.running.store(false, Ordering::Release);
        if let Ok(mut supervisor) = holder.supervisor.lock() {
            let _ = supervisor.stop();
        }
    }
    if let Ok(mut flags) = cancel_flags().write() {
        flags.remove(&key);
    }
}

pub fn stop_group(group_id: &str) {
    let actor_ids = sessions()
        .read()
        .map(|map| {
            map.keys()
                .filter(|(candidate, _)| candidate == group_id)
                .map(|(_, actor_id)| actor_id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for actor_id in actor_ids {
        stop(group_id, &actor_id);
    }
}

pub fn stop_all() {
    let keys = sessions()
        .read()
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for (group_id, actor_id) in keys {
        stop(&group_id, &actor_id);
    }
}

pub(super) fn cancellation_requested(group_id: &str, actor_id: &str) -> bool {
    let key = (group_id.to_owned(), actor_id.to_owned());
    cancel_flags()
        .read()
        .ok()
        .and_then(|flags| flags.get(&key).cloned())
        .is_some_and(|flag| flag.load(Ordering::Acquire))
}

/// A registered owner, even one awaiting cleanup, blocks automatic wake.
/// A poisoned registry is unknown, never interpreted as no owner.
pub(super) fn has_registered_owner(group_id: &str, actor_id: &str) -> std::io::Result<bool> {
    sessions()
        .read()
        .map(|map| map.contains_key(&(group_id.to_owned(), actor_id.to_owned())))
        .map_err(|_| std::io::Error::other("DeepSeek owner registry is poisoned"))
}

pub(super) fn confirmed_running(group_id: &str, actor_id: &str) -> std::io::Result<bool> {
    let key = (group_id.to_owned(), actor_id.to_owned());
    let gate = lifecycle_gate(&key)?;
    let _guard = gate
        .lock()
        .map_err(|_| std::io::Error::other("deepseek lifecycle gate poisoned"))?;
    let holder = sessions()
        .read()
        .map_err(|_| std::io::Error::other("deepseek session lock poisoned"))?
        .get(&key)
        .cloned();
    let Some(holder) = holder else {
        return Ok(false);
    };
    if !holder.running.load(Ordering::Acquire) {
        return Ok(false);
    }
    let mut supervisor = holder
        .supervisor
        .try_lock()
        .map_err(|_| std::io::Error::other("deepseek owner cannot be observed"))?;
    supervisor.confirm_running()
}

pub fn running(group_id: &str, actor_id: &str) -> bool {
    let key = (group_id.to_owned(), actor_id.to_owned());
    sessions()
        .read()
        .ok()
        .and_then(|map| map.get(&key).cloned())
        .is_some_and(|holder| entry_running(&holder))
}

pub(super) fn registered_workspace(
    group_id: &str,
    actor_id: &str,
) -> Option<(bool, super::actor_runtime::LaunchWorkspace)> {
    let holder = sessions()
        .read()
        .ok()?
        .get(&(group_id.to_owned(), actor_id.to_owned()))
        .cloned()?;
    Some((entry_running(&holder), holder.workspace.clone()))
}

fn entry_running(holder: &RuntimeEntry) -> bool {
    if !holder.running.load(Ordering::Acquire) {
        return false;
    }
    match holder.supervisor.try_lock() {
        Ok(mut supervisor) => {
            let running = supervisor.is_running();
            holder.running.store(running, Ordering::Release);
            running
        }
        Err(std::sync::TryLockError::WouldBlock) => true,
        Err(std::sync::TryLockError::Poisoned(_)) => false,
    }
}

#[cfg(all(test, unix))]
mod wake_owner_tests {
    use super::*;
    use cccc_contracts::{ActorRuntime, Event};
    use cccc_core::GroupStore;

    fn confirmed(
        home: &HomeLayout,
        group: &GroupDoc,
        actor: &Actor,
        origin: &cccc_core::deepseek_restart_gate::WakeOrigin,
    ) -> std::io::Result<bool> {
        with_confirmed_wake(home, group, actor, origin, || Ok(())).map(|value| value.is_some())
    }

    struct Cleanup(String, String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            stop(&self.0, &self.1);
        }
    }

    #[test]
    fn actual_deepseek_owner_binds_one_attempt_and_replacement_does_not_inherit_it() {
        let event: Event = serde_json::from_str(include_str!("mail_wake/fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("wake-owner-control", "").expect("group");
        let mut actor = Actor::new("receiver");
        actor.runtime = ActorRuntime::Deepseek;
        actor.generation = "captured-generation-control".into();
        // Labeled launcher control: the real supervisor owns this small local
        // ACP process; the source Mail envelope above is the retained capture.
        actor.command = vec!["sh".into(), "-c".into(), r#"while IFS= read -r line; do
if printf '%s' "$line" | grep -q '"method":"initialize"'; then
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"control"}}}'
elif printf '%s' "$line" | grep -q '"method":"session/new"'; then
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"control-session"}}'
fi
done"#.into()];
        group.actors.push(actor.clone());
        store.save(&group).expect("save");
        let _cleanup = Cleanup(group.group_id.clone(), actor.id.clone());
        let origin = cccc_core::deepseek_restart_gate::WakeOrigin {
            attempt_id: "attempt-one".into(),
            actor_generation: actor.generation.clone(),
            policy_fingerprint: "a".repeat(64),
            source_event_id: event.id,
        };
        start_for_wake(&home, &group, &actor, temp.path(), &origin).expect("launch");
        assert!(confirmed(&home, &group, &actor, &origin).expect("owned"));
        let key = (group.group_id.clone(), actor.id.clone());
        let incarnation = sessions()
            .read()
            .expect("sessions")
            .get(&key)
            .expect("owner")
            .generation
            .clone();
        // Labeled persistence-failure control; no second launcher is called.
        let failed: std::io::Result<Option<()>> =
            with_confirmed_wake(&home, &group, &actor, &origin, || {
                Err(std::io::Error::other("result write failed"))
            });
        assert!(failed.is_err());
        assert_eq!(
            sessions()
                .read()
                .expect("sessions")
                .get(&key)
                .expect("same owner")
                .generation,
            incarnation
        );
        assert!(confirmed(&home, &group, &actor, &origin).expect("rejoin"));
        let owner_path = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("deepseek/receiver/runtime-state.json");
        let saved = std::fs::read(&owner_path).expect("owner record");
        std::fs::write(&owner_path, b"{").expect("corrupt owner control");
        assert!(confirmed(&home, &group, &actor, &origin).is_err());
        std::fs::write(&owner_path, saved).expect("restore owner control");
        assert!(confirmed(&home, &group, &actor, &origin).expect("rejoin restored"));
        let mut other = origin.clone();
        other.attempt_id = "attempt-two".into();
        assert!(!confirmed(&home, &group, &actor, &other).expect("not inherited"));
        assert!(start_for_wake(&home, &group, &actor, temp.path(), &other).is_err());
        assert!(confirmed(&home, &group, &actor, &origin).expect("original still owned"));
        stop(&group.group_id, &actor.id);
        assert!(!confirmed(&home, &group, &actor, &origin).expect("absent-registry control"));
        start(&home, &group, &actor, temp.path()).expect("operator replacement");
        assert!(!confirmed(&home, &group, &actor, &origin).expect("replacement excluded"));
    }
}

/// Verify both the actual registered incarnation and the durable owner record.
/// A busy or poisoned supervisor is unknown, not a successful observation.
pub(super) fn with_confirmed_wake<T>(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    origin: &cccc_core::deepseek_restart_gate::WakeOrigin,
    confirmed: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<Option<T>> {
    if origin.actor_generation != cccc_core::actors::generation_identity(actor) {
        return Ok(None);
    }
    let key = (group.group_id.clone(), actor.id.clone());
    let gate = lifecycle_gate(&key)?;
    let _guard = gate
        .lock()
        .map_err(|_| std::io::Error::other("deepseek lifecycle gate poisoned"))?;
    let holder = sessions()
        .read()
        .map_err(|_| std::io::Error::other("deepseek session lock poisoned"))?
        .get(&key)
        .cloned();
    let Some(holder) = holder else {
        return Ok(None);
    };
    if holder.wake_origin.as_ref() != Some(origin) || !holder.running.load(Ordering::Acquire) {
        return Ok(None);
    }
    let mut supervisor = holder
        .supervisor
        .try_lock()
        .map_err(|_| std::io::Error::other("deepseek owner cannot be observed"))?;
    if !supervisor.confirm_running()? {
        holder.running.store(false, Ordering::Release);
        return Ok(None);
    }
    if !cccc_core::deepseek_restart_gate::wake_origin_matches(
        home,
        &group.group_id,
        &actor.id,
        &actor.created_at,
        &holder.generation,
        origin,
    )? {
        return Ok(None);
    }
    // Keep the lifecycle and supervisor permits through the caller's durable
    // result join so a replacement cannot inherit this observation.
    confirmed().map(Some)
}

pub(super) fn manual_restart_required(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> bool {
    let key = (group.group_id.clone(), actor.id.clone());
    if sessions()
        .read()
        .ok()
        .and_then(|map| map.get(&key).cloned())
        .is_some_and(|holder| holder.manual_restart_required.load(Ordering::Acquire))
    {
        return true;
    }
    match cccc_core::deepseek_restart_gate::manual_restart_required(
        home,
        &group.group_id,
        &actor.id,
        &actor.created_at,
    ) {
        Ok(required) => required,
        Err(error) => {
            tracing::error!(
                %error,
                group_id = %group.group_id,
                actor_id = %actor.id,
                "failed to read DeepSeek manual restart gate"
            );
            true
        }
    }
}

#[cfg(test)]
#[path = "deepseek_runtime/dedupe_tests.rs"]
mod dedupe_tests;
#[cfg(test)]
#[path = "deepseek_runtime/delivery_tests.rs"]
mod delivery_tests;
#[cfg(test)]
#[path = "deepseek_runtime/launch_command_tests.rs"]
mod launch_command_tests;
#[cfg(test)]
#[path = "deepseek_runtime/timeout_tests.rs"]
mod timeout_tests;
