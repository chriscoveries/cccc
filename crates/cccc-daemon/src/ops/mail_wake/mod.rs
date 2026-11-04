//! Automatic Mail wake admission. The Mail ledger stays authoritative.
mod budget;
mod group;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use cccc_contracts::{Actor, DaemonRequest, Event, GroupState};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, actors, mail_pending};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use self::budget::Budget;
use self::group::{GroupBudget, Pending};

use super::operation::{Operation, Policy};
use crate::dispatch::{OpError, OpResult};
use serde_json::json;

pub(super) fn resolve_operation(request: &DaemonRequest) -> Option<Operation> {
    match request.op.as_str() {
        "mail_wake_initialize" => Some(Operation::new(Policy::GlobalWrite, initialize)),
        _ => None,
    }
}

fn initialize(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    if request.args.get("by").and_then(serde_json::Value::as_str) != Some("user")
        || request.args.contains_key("_cccc_native_binding")
        || request.args.contains_key("_cccc_native_source_binding")
        || request.args.contains_key("_cccc_web_binding")
    {
        return Err(OpError::new(
            "permission_denied",
            "explicit user recovery required",
        ));
    }
    // This is recovery of already-ACTIVE, complete state only. Request fields
    // never authorize first use or a legacy-home migration.
    recover_existing(home).map_err(OpError::io)?;
    crate::dispatch::object(json!({"status":"recovered"}))
}

struct HomeState {
    activation: cccc_core::registry::MailWakeActivation,
    global: Budget,
    groups: HashMap<String, GroupBudget>,
    signaled: bool,
}

type Homes = HashMap<PathBuf, HomeState>;

fn homes() -> &'static Mutex<Homes> {
    static HOMES: OnceLock<Mutex<Homes>> = OnceLock::new();
    HOMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn failed_publications() -> &'static Mutex<HashSet<PathBuf>> {
    static FAILED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    FAILED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn global_path(home: &HomeLayout) -> PathBuf {
    home.root().join("state").join("mail-wake.json")
}

const ACTIVATION_SCHEMA: u8 = 1;

fn home_identity(home: &HomeLayout) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(home.root())?;
        if !metadata.is_dir() {
            return Err(io::Error::other("home is not a directory"));
        }
        Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = home;
        Err(io::Error::other("home identity unavailable"))
    }
}

/// Use the native source identity compiled into the running process. It binds
/// the implementation source set, not an executable or artifact checksum.
fn implementation_digest() -> io::Result<String> {
    let info = cccc_core::build_info::current();
    let identity = info
        .get("source_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| io::Error::other("compiled source identity unavailable"))?;
    if identity.len() != 64 || !identity.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other("invalid compiled source identity"));
    }
    Ok(identity.to_owned())
}

fn fresh_proof_digest(identity: &str, implementation: &str, activation_id: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            format!("fresh-home-v1:{identity}:{implementation}:{activation_id}").as_bytes()
        )
    )
}

fn active_header(home: &HomeLayout) -> io::Result<cccc_core::registry::MailWakeActivation> {
    use cccc_core::registry::MailWakePhase;
    let registry = cccc_core::Registry::load_existing(home)?;
    let header = registry
        .mail_wake_activation
        .ok_or_else(|| io::Error::other("Mail wake activation missing"))?;
    if header.version != ACTIVATION_SCHEMA
        || header.phase != MailWakePhase::Active
        || header.activation_id.is_empty()
        || header.activation_id.len() > 128
        || header.proof_digest.len() != 64
        || !header
            .proof_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || header.home_identity != home_identity(home)?
        || header.implementation != implementation_digest()?
        || header.proof_digest
            != fresh_proof_digest(
                &header.home_identity,
                &header.implementation,
                &header.activation_id,
            )
    {
        return Err(io::Error::other("Mail wake activation unverified"));
    }
    Ok(header)
}

/// The private creation capability is held from exclusive directory creation
/// through PREPARING, limiter creation, census and the ACTIVE publication.
/// An interrupted attempt is not silently resumed from missing state.
fn activate_fresh(
    home: &HomeLayout,
    fresh: &cccc_core::home::FreshHome,
    fault: &mut impl FnMut(&std::path::Path, PublishStage) -> io::Result<()>,
) -> io::Result<()> {
    use cccc_core::registry::{MailWakeActivation, MailWakePhase};
    let identity = fresh.verify_identity(home)?;
    let implementation = implementation_digest()?;
    let registry = cccc_core::Registry::load_existing(home)?;
    if registry.mail_wake_activation.is_some()
        || !registry.groups.is_empty()
        || !registry.defaults.is_empty()
        || std::fs::read_dir(home.groups_dir())?
            .next()
            .transpose()?
            .is_some()
        || global_path(home).try_exists()?
    {
        return Err(io::Error::other("fresh-home baseline changed"));
    }
    let activation_id = uuid::Uuid::new_v4().simple().to_string();
    let proof_digest = fresh_proof_digest(&identity, &implementation, &activation_id);
    let mut header = MailWakeActivation {
        version: ACTIVATION_SCHEMA,
        activation_id: activation_id.clone(),
        proof_digest,
        implementation,
        home_identity: identity,
        phase: MailWakePhase::Preparing,
    };
    fresh.verify_identity(home)?;
    cccc_core::Registry::mutate_existing(home, |current| {
        if current != &registry {
            return Err(io::Error::other("fresh-home registry changed"));
        }
        current.mail_wake_activation = Some(header.clone());
        Ok(())
    })?;
    fresh.verify_identity(home)?;
    Budget::create_for_activation(&global_path(home), &activation_id)?;
    sync_ancestors(home, &global_path(home), &mut |_, _| Ok(()))?;
    let limiter = Budget::load(&global_path(home), Instant::now())?;
    if limiter.activation_id() != activation_id || !limiter.is_pristine() {
        return Err(io::Error::other("fresh limiter mismatch"));
    }
    let preparing = cccc_core::Registry::load_existing(home)?;
    if preparing.mail_wake_activation.as_ref() != Some(&header)
        || !preparing.groups.is_empty()
        || !preparing.defaults.is_empty()
        || std::fs::read_dir(home.groups_dir())?
            .next()
            .transpose()?
            .is_some()
    {
        return Err(io::Error::other("fresh-home census changed"));
    }
    fresh.verify_identity(home)?;
    header.phase = MailWakePhase::Active;
    cccc_core::fs::with_exclusive_lock(&home.root().join("registry.json.lock"), || {
        let mut current = cccc_core::Registry::load_existing(home)?;
        if current != preparing {
            return Err(io::Error::other("fresh-home census changed"));
        }
        current.mail_wake_activation = Some(header);
        current.updated_at = cccc_contracts::utc_now();
        current.seal_mail_wake()?;
        let mut bytes = serde_json::to_vec_pretty(&current).map_err(io::Error::other)?;
        bytes.push(b'\n');
        strict_publish(&home.registry_path(), &bytes, fault)
    })?;
    fresh.verify_identity(home)?;
    Ok(())
}

/// All daemon startup routes take the same creation lock. Other home access
/// remains valid, but does not itself mint first-use authority.
fn bootstrap_lock_path(home: &HomeLayout) -> io::Result<PathBuf> {
    let parent = home
        .root()
        .parent()
        .ok_or_else(|| io::Error::other("home has no parent"))?;
    let mut name = home
        .root()
        .file_name()
        .ok_or_else(|| io::Error::other("home has no name"))?
        .to_os_string();
    name.push(".cccc-bootstrap.lock");
    Ok(parent.join(name))
}

pub(crate) fn bootstrap(home: &HomeLayout) -> io::Result<()> {
    bootstrap_with(home, &mut |_, _| Ok(()))
}

fn bootstrap_with(
    home: &HomeLayout,
    fault: &mut impl FnMut(&std::path::Path, PublishStage) -> io::Result<()>,
) -> io::Result<()> {
    cccc_core::fs::with_exclusive_lock(&bootstrap_lock_path(home)?, || {
        let fresh = home.try_create_fresh()?;
        home.initialize().map_err(io::Error::other)?;
        if let Some(fresh) = fresh {
            // This is the only case in which Registry::load may create the
            // baseline; ordinary admission uses load_existing exclusively.
            cccc_core::Registry::load(home)?;
            if let Err(error) = activate_fresh(home, &fresh, fault) {
                failed_publications()
                    .lock()
                    .map_err(|_| io::Error::other("publication latch poisoned"))?
                    .insert(home.root().to_path_buf());
                tracing::warn!(%error, "Mail wake fresh-home activation held");
            }
        }
        Ok(())
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublishStage {
    Write,
    DataSync,
    Rename,
    DirectorySync,
    AncestorSync,
}

fn strict_publish(
    path: &std::path::Path,
    bytes: &[u8],
    fault: &mut impl FnMut(&std::path::Path, PublishStage) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("publication parent missing"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    fault(path, PublishStage::Write)?;
    temp.write_all(bytes)?;
    fault(path, PublishStage::DataSync)?;
    temp.as_file().sync_all()?;
    fault(path, PublishStage::Rename)?;
    temp.persist(path).map_err(|error| error.error)?;
    fault(path, PublishStage::DirectorySync)
        .and_then(|()| std::fs::File::open(parent)?.sync_all())
        .map_err(|error| io::Error::other(format!("published; durability unknown: {error}")))
}

fn sync_ancestors(
    home: &HomeLayout,
    path: &std::path::Path,
    fault: &mut impl FnMut(&std::path::Path, PublishStage) -> io::Result<()>,
) -> io::Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (home, path, fault);
        Err(io::Error::other("directory durability unavailable"))
    }
    #[cfg(unix)]
    {
        let outer = home
            .root()
            .parent()
            .ok_or_else(|| io::Error::other("home parent missing"))?;
        let mut dir = path
            .parent()
            .ok_or_else(|| io::Error::other("state parent missing"))?;
        for _ in 0..8 {
            fault(dir, PublishStage::AncestorSync)?;
            std::fs::File::open(dir)?.sync_all()?;
            if dir == outer {
                return Ok(());
            }
            if !dir.starts_with(home.root()) {
                return Err(io::Error::other("state escaped home"));
            }
            dir = dir
                .parent()
                .ok_or_else(|| io::Error::other("state ancestor missing"))?;
        }
        Err(io::Error::other("state ancestor bound exceeded"))
    }
}

fn bounded_read(path: &std::path::Path, total: &mut u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    const MEMBER: u64 = 8 * 1024 * 1024;
    const TOTAL: u64 = 32 * 1024 * 1024;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MEMBER + 1)
        .read_to_end(&mut bytes)?;
    *total = total
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| io::Error::other("recovery size overflow"))?;
    if bytes.len() as u64 > MEMBER || *total > TOTAL {
        return Err(io::Error::other("recovery size bound exceeded"));
    }
    Ok(bytes)
}

fn same_bytes(path: &std::path::Path, expected: &[u8]) -> io::Result<bool> {
    use std::io::Read;
    let mut actual = Vec::new();
    std::fs::File::open(path)?
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut actual)?;
    Ok(actual.as_slice() == expected)
}

/// Called under daemon-wide creation/writer exclusion, before IPC startup or
/// via an explicit globally serialized recovery request. No missing file is
/// initialized here and the same validated bytes are republished.
fn recover_with(
    home: &HomeLayout,
    fault: &mut impl FnMut(&std::path::Path, PublishStage) -> io::Result<()>,
) -> io::Result<()> {
    homes()
        .lock()
        .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?
        .remove(home.root());
    cccc_core::fs::with_exclusive_lock(&bootstrap_lock_path(home)?, || {
        let lock = home.root().join("registry.json.lock");
        cccc_core::fs::with_exclusive_lock(&lock, || {
            let mut total = 0;
            let registry_path = home.registry_path();
            let registry_bytes = bounded_read(&registry_path, &mut total)?;
            let activation = active_header(home)?;
            let registry = cccc_core::Registry::load_existing(home)?;
            if registry.groups.len() > 1024 {
                return Err(io::Error::other("Mail wake census limit reached"));
            }
            // Group mutations can take their group lock before the registry
            // lock. Never wait for them while holding the registry lock: an
            // unavailable lock holds recovery instead of deadlocking. Keep
            // every acquired lock until the validated snapshots are published.
            let store = GroupStore::new(home.clone())?;
            let mut group_locks = Vec::with_capacity(registry.groups.len());
            for group_id in registry.groups.keys() {
                let path = store.group_dir(group_id)?.join("group.yaml.lock");
                let lock = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)?;
                fs2::FileExt::try_lock_exclusive(&lock)?;
                group_locks.push(lock);
            }
            let global_path = global_path(home);
            let global_bytes = bounded_read(&global_path, &mut total)?;
            let global = Budget::load(&global_path, Instant::now())?;
            if global.activation_id() != activation.activation_id {
                return Err(io::Error::other("Mail wake activation mismatch"));
            }
            let expected_attempt = global.in_flight();
            let mut matched_attempt = false;
            let mut groups = Vec::with_capacity(registry.groups.len());
            for group_id in registry.groups.keys() {
                let group_path = store.group_dir(group_id)?.join("group.yaml");
                let group_bytes = bounded_read(&group_path, &mut total)?;
                let path = store.state_dir(group_id)?.join("automation.json");
                let bytes = bounded_read(&path, &mut total)?;
                let mut state = GroupBudget::load(home, group_id, Instant::now(), false)?;
                state.check_group(home)?;
                for attempt in state.unresolved() {
                    if expected_attempt.as_ref() != Some(attempt) || matched_attempt {
                        return Err(io::Error::other("Mail wake attempt join mismatch"));
                    }
                    matched_attempt = true;
                }
                if let Some(attempt) = expected_attempt.as_ref()
                    && attempt.group_id.as_str() == group_id.as_str()
                    && state.settled_matches(attempt)
                {
                    if matched_attempt {
                        return Err(io::Error::other("duplicate Mail wake attempt join"));
                    }
                    matched_attempt = true;
                }
                groups.push((group_id.clone(), path, bytes, group_path, group_bytes));
            }
            if expected_attempt.is_some() && !matched_attempt {
                return Err(io::Error::other("Mail wake attempt join incomplete"));
            }
            if !same_bytes(&global_path, &global_bytes)? {
                return Err(io::Error::other(
                    "global wake state changed during recovery",
                ));
            }
            strict_publish(&global_path, &global_bytes, fault)?;
            sync_ancestors(home, &global_path, fault)?;
            for (_, path, bytes, group_path, group_bytes) in &groups {
                if !same_bytes(path, bytes)? || !same_bytes(group_path, group_bytes)? {
                    return Err(io::Error::other("group wake state changed during recovery"));
                }
                strict_publish(group_path, group_bytes, fault)?;
                sync_ancestors(home, group_path, fault)?;
                strict_publish(path, bytes, fault)?;
                sync_ancestors(home, path, fault)?;
            }
            if !same_bytes(&registry_path, &registry_bytes)? {
                return Err(io::Error::other("registry changed during recovery"));
            }
            strict_publish(&registry_path, &registry_bytes, fault)?;
            sync_ancestors(home, &registry_path, fault)?;
            if active_header(home)? != activation
                || cccc_core::Registry::load_existing(home)? != registry
                || !same_bytes(&registry_path, &registry_bytes)?
                || !same_bytes(&global_path, &global_bytes)?
            {
                return Err(io::Error::other("Mail wake recovery join changed"));
            }
            for (group_id, path, bytes, group_path, group_bytes) in &groups {
                if !same_bytes(path, bytes)? || !same_bytes(group_path, group_bytes)? {
                    return Err(io::Error::other("group wake state changed during recovery"));
                }
                let mut state = GroupBudget::load(home, group_id, Instant::now(), false)?;
                state.check_group(home)?;
            }
            let global = Budget::load(&global_path, Instant::now())?;
            if global.activation_id() != activation.activation_id {
                return Err(io::Error::other("Mail wake activation mismatch"));
            }
            homes()
                .lock()
                .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?
                .insert(
                    home.root().to_path_buf(),
                    HomeState {
                        activation,
                        global,
                        groups: HashMap::new(),
                        signaled: false,
                    },
                );
            Ok(())
        })
    })
}

fn attempt_recovery(
    home: &HomeLayout,
    explicit: bool,
    fault: &mut impl FnMut(&std::path::Path, PublishStage) -> io::Result<()>,
) -> io::Result<()> {
    if !explicit
        && failed_publications()
            .lock()
            .map_err(|_| io::Error::other("publication latch poisoned"))?
            .contains(home.root())
    {
        return Err(io::Error::other("explicit publication recovery required"));
    }
    let result = recover_with(home, fault);
    let mut failed = failed_publications()
        .lock()
        .map_err(|_| io::Error::other("publication latch poisoned"))?;
    if result.is_ok() {
        failed.remove(home.root());
    } else {
        failed.insert(home.root().to_path_buf());
    }
    result
}

pub(crate) fn recover_existing(home: &HomeLayout) -> io::Result<()> {
    attempt_recovery(home, true, &mut |_, _| Ok(()))
}

pub(crate) fn recover_on_start(home: &HomeLayout) -> io::Result<()> {
    attempt_recovery(home, false, &mut |_, _| Ok(()))
}

fn group_budget<'a>(
    home: &HomeLayout,
    state: &'a mut HomeState,
    group_id: &str,
) -> io::Result<&'a mut GroupBudget> {
    if !state.groups.contains_key(group_id) {
        if state.groups.len() >= 1024 {
            return Err(io::Error::other("Mail wake group cache limit reached"));
        }
        // Ordinary admission never initializes a missing namespace.
        let budget = GroupBudget::load(home, group_id, Instant::now(), false)?;
        state.groups.insert(group_id.to_owned(), budget);
    }
    let budget = state
        .groups
        .get_mut(group_id)
        .ok_or_else(|| io::Error::other("Mail wake group missing"))?;
    budget.check_group(home)?;
    Ok(budget)
}

fn home_state<'a>(home: &HomeLayout, homes: &'a mut Homes) -> io::Result<&'a mut HomeState> {
    let activation = active_header(home)?;
    let state = homes
        .get_mut(home.root())
        .ok_or_else(|| io::Error::other("Mail wake readiness unverified"))?;
    if state.activation != activation || state.global.activation_id() != activation.activation_id {
        return Err(io::Error::other("Mail wake activation mismatch"));
    }
    state.global.verify_unchanged()?;
    Ok(state)
}

/// Called only for a Group returned by the new-Group creator while its global
/// write permit is held. Restores and imports never call this function.
pub(super) fn initialize_created_group(home: &HomeLayout, group: &GroupDoc) -> io::Result<()> {
    let result = (|| {
        let path = global_path(home);
        if !path.try_exists()? {
            // An untouched legacy home needs no namespace. Missing state in
            // an activated home is instead a recovery-required error.
            if cccc_core::Registry::load_existing(home)?
                .mail_wake_activation
                .is_some()
            {
                return Err(io::Error::other("Mail wake limiter missing"));
            }
            return Ok(());
        }
        let header = active_header(home)?;
        let active = Budget::load(&path, Instant::now())?;
        if active.activation_id() != header.activation_id {
            return Err(io::Error::other("Mail wake activation mismatch"));
        }
        if group.group_id.is_empty() || group.generation.is_empty() {
            return Err(io::Error::other("new group identity unavailable"));
        }
        GroupBudget::load(home, &group.group_id, Instant::now(), true)?;
        let path = GroupStore::new(home.clone())?
            .state_dir(&group.group_id)?
            .join("automation.json");
        sync_ancestors(home, &path, &mut |_, _| Ok(()))
    })();
    if result.is_err() {
        homes()
            .lock()
            .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?
            .remove(home.root());
        failed_publications()
            .lock()
            .map_err(|_| io::Error::other("publication latch poisoned"))?
            .insert(home.root().to_path_buf());
    }
    result
}

pub(super) fn require_activation(home: &HomeLayout) -> Result<(), OpError> {
    let mut guard = homes()
        .lock()
        .map_err(|_| OpError::new("mail_wake_unknown", "Mail wake state lock poisoned"))?;
    home_state(home, &mut guard).map_err(|_| {
        OpError::new(
            "mail_wake_uninitialized",
            "Initialize the Mail wake budget after reconciling prior attempts",
        )
    })?;
    Ok(())
}

/// An automatic boot restore must not create a replacement for an unresolved
/// Mail-wake attempt. Explicit operator starts retain their own authority.
pub(super) fn restore_blocked(home: &HomeLayout, group_id: &str, actor: &Actor) -> bool {
    let registry = match cccc_core::Registry::load_existing(home) {
        Ok(registry) => registry,
        Err(_) => return true,
    };
    let path = global_path(home);
    let present = match path.try_exists() {
        Ok(value) => value,
        Err(_) => return true,
    };
    let header = registry.mail_wake_activation.as_ref();
    if !present {
        // Untouched pre-feature homes preserve the old restore path. An
        // activated or opted-in home with missing evidence is unknown.
        return header.is_some() || actor.wake_on_mail;
    }
    let Ok(budget) = Budget::load(&path, Instant::now()) else {
        return true;
    };
    budget
        .in_flight()
        .is_some_and(|attempt| attempt.group_id == group_id && attempt.actor_id == actor.id)
}

pub(super) fn rearm(home: &HomeLayout, group_id: &str, actor: &Actor) -> io::Result<()> {
    let mut guard = homes()
        .lock()
        .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
    let state = home_state(home, &mut guard)?;
    group_budget(home, state, group_id)?.rearm(&actor.id, &actors::generation_identity(actor))
}

/// The periodic unread tick is the repair path when this in-memory hint is lost.
pub(crate) fn take_signal(home: &HomeLayout) -> bool {
    let Ok(mut guard) = homes().lock() else {
        return false;
    };
    guard
        .get_mut(home.root())
        .is_some_and(|state| std::mem::take(&mut state.signaled))
}

/// Only literal-addressed committed Mail can nominate an automatic start.
fn direct_mail_for(event: &Event, actor: &Actor) -> bool {
    if event.kind != "chat.message"
        || event.by == actor.id
        || event
            .data
            .get("message_mode")
            .and_then(serde_json::Value::as_str)
            != Some("mail")
    {
        return false;
    }
    let Some(targets) = event.data.get("to").and_then(serde_json::Value::as_array) else {
        return false;
    };
    if targets.is_empty()
        || targets
            .iter()
            .any(|target| target.as_str().is_none_or(|target| target.starts_with('@')))
    {
        return false;
    }
    targets
        .iter()
        .any(|target| target.as_str() == Some(actor.id.as_str()))
}

fn policy_fingerprint(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> io::Result<String> {
    // The private key is generated once with the activation and is never
    // reconstructed when the budget is missing or malformed.
    let activation = active_header(home)?;
    let budget = Budget::load(&global_path(home), Instant::now())?;
    if budget.activation_id() != activation.activation_id {
        return Err(io::Error::other("Mail wake activation mismatch"));
    }
    let environment = super::actor_secrets::effective_values(home, &group.group_id, actor)
        .map_err(|error| io::Error::other(error.message))?;
    struct HashWriter(Hmac<Sha256>);
    impl io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Mac::update(&mut self.0, bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mac = <Hmac<Sha256> as Mac>::new_from_slice(budget.policy_key())
        .map_err(|_| io::Error::other("invalid Mail wake policy key"))?;
    let mut writer = HashWriter(mac);
    serde_json::to_writer(&mut writer, &(group, actor, environment)).map_err(io::Error::other)?;
    Ok(format!("{:x}", writer.0.finalize().into_bytes()))
}

fn eligible_group(group: &GroupDoc) -> bool {
    group.running && matches!(group.state, GroupState::Active | GroupState::Idle)
}

fn eligible_actor(actor: &Actor) -> bool {
    actor.enabled
        && actor.wake_on_mail
        && (!actor.generation.is_empty() || !actor.created_at.is_empty())
        && actor.internal_kind.is_none()
        && actor.external_session_id.is_empty()
        && !actor.runtime.is_web_model()
}

/// Called after the original message commits, under the send's group permit.
/// A failure is diagnostic only; the unread projection is the repair path.
pub(super) fn note_committed(home: &HomeLayout, group: &GroupDoc, event: &Event) -> io::Result<()> {
    if !eligible_group(group)
        || !actors::visible(group)
            .any(|actor| eligible_actor(actor) && direct_mail_for(event, actor))
    {
        return Ok(());
    }
    let mut guard = homes()
        .lock()
        .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
    let state = home_state(home, &mut guard)?;
    let budget = group_budget(home, state, &group.group_id)?;
    let mut noted = false;
    for stored in actors::visible(group)
        .filter(|actor| eligible_actor(actor) && direct_mail_for(event, actor))
    {
        let actor = super::actor_profile_runtime::resolve(home, stored)
            .map_err(|error| io::Error::other(error.message))?;
        let awaited =
            mail_pending::still_awaited(home, group, &actor.id, std::slice::from_ref(&event.id))?;
        if !awaited.contains(&event.id) {
            continue;
        }
        budget.note(
            &actor.id,
            Pending {
                source_event_id: event.id.clone(),
                source_ts: event.ts.clone(),
                generation: actors::generation_identity(&actor),
                policy_fingerprint: policy_fingerprint(home, group, &actor)?,
            },
        )?;
        noted = true;
    }
    state.signaled |= noted;
    Ok(())
}

#[derive(Clone)]
struct Candidate {
    group_id: String,
    actor_id: String,
    generation: String,
    source_id: String,
    source_time: chrono::DateTime<chrono::FixedOffset>,
    fingerprint: String,
}

/// Build nominations from the indexed unread projection, not a ledger walk.
/// Called with the target Group permit; any error aborts the entire census.
fn group_candidates(home: &HomeLayout, group_id: &str) -> io::Result<Vec<Candidate>> {
    let store = GroupStore::new(home.clone())?;
    let group = store.load(group_id)?;
    // A missing or changed namespace invalidates the complete census, even
    // when this group currently has no eligible actor.
    {
        let mut guard = homes()
            .lock()
            .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
        let state = home_state(home, &mut guard)?;
        group_budget(home, state, group_id)?;
    }
    if !eligible_group(&group) {
        return Ok(Vec::new());
    }
    let actors = actors::visible(&group)
        .filter(|actor| eligible_actor(actor))
        .collect::<Vec<_>>();
    if actors.is_empty() {
        return Ok(Vec::new());
    }
    {
        let current = group
            .actors
            .iter()
            .map(|actor| (actor.id.clone(), actors::generation_identity(actor)))
            .collect();
        let mut guard = homes()
            .lock()
            .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
        let state = home_state(home, &mut guard)?;
        group_budget(home, state, group_id)?.retain_current(&current)?;
    }
    let mut result = Vec::new();
    for stored in actors {
        let actor = super::actor_profile_runtime::resolve(home, stored)
            .map_err(|error| io::Error::other(error.message))?;
        let fingerprint = policy_fingerprint(home, &group, &actor)?;
        let mail = cccc_core::inbox::list_unread(home, &group, &actor.id, 1000)?;
        if mail.len() >= 1000 {
            return Err(io::Error::other(
                "Mail wake census reached the unread page bound",
            ));
        }
        let addressed = mail
            .into_iter()
            .filter(|event| direct_mail_for(event, &actor))
            .collect::<Vec<_>>();
        let ids = addressed
            .iter()
            .map(|event| event.id.clone())
            .collect::<Vec<_>>();
        let awaited = mail_pending::still_awaited(home, &group, &actor.id, &ids)?;
        let mut available = Vec::new();
        for event in addressed
            .into_iter()
            .filter(|event| awaited.contains(&event.id))
        {
            let time = chrono::DateTime::parse_from_rfc3339(&event.ts)
                .map_err(|_| io::Error::other("invalid Mail wake source timestamp"))?;
            available.push((time, event));
        }
        available.sort_by(|a, b| (&a.0, &a.1.id).cmp(&(&b.0, &b.1.id)));
        let mut guard = homes()
            .lock()
            .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
        let state = home_state(home, &mut guard)?;
        let budget = group_budget(home, state, group_id)?;
        if !budget.cue_pending(&actor.id) {
            let mut retained = Vec::new();
            for (time, event) in available {
                if !budget.already_settled(&actor.id, &event.ts, &event.id)? {
                    retained.push((time, event));
                }
            }
            available = retained;
        }
        let old = budget.pending(&actor.id).cloned();
        if old.as_ref().is_some_and(|pending| {
            pending.generation != actors::generation_identity(&actor)
                || pending.policy_fingerprint != fingerprint
                || !available
                    .iter()
                    .any(|(_, event)| event.id == pending.source_event_id)
        }) {
            // A settled cue is a durable owner-bound obligation. A changed
            // policy or missing source holds it instead of erasing it.
            if budget.cue_pending(&actor.id) {
                continue;
            }
            budget.clear_pending(&actor.id)?;
        }
        if let Some((time, event)) = available.first() {
            budget.note(
                &actor.id,
                Pending {
                    source_event_id: event.id.clone(),
                    source_ts: event.ts.clone(),
                    generation: actors::generation_identity(&actor),
                    policy_fingerprint: fingerprint.clone(),
                },
            )?;
            if !budget.cue_pending(&actor.id)
                && budget.pending(&actor.id).is_some_and(|pending| {
                    pending.source_event_id == event.id && pending.policy_fingerprint == fingerprint
                })
            {
                result.push(Candidate {
                    group_id: group_id.to_owned(),
                    actor_id: actor.id.clone(),
                    generation: actors::generation_identity(&actor),
                    source_id: event.id.clone(),
                    source_time: *time,
                    fingerprint,
                });
            }
        }
    }
    Ok(result)
}

/// Reconcile committed wake cues using the existing notice reducer. This does
/// not launch an actor or consume the original Mail.
fn repair_cues(home: &HomeLayout, group_id: &str) -> io::Result<()> {
    let store = GroupStore::new(home.clone())?;
    let group = store.load(group_id)?;
    if !eligible_group(&group) {
        return Ok(());
    }
    for stored in actors::visible(&group).filter(|actor| eligible_actor(actor)) {
        let actor = super::actor_profile_runtime::resolve(home, stored)
            .map_err(|error| io::Error::other(error.message))?;
        let (pending, attempt) = {
            let mut guard = homes()
                .lock()
                .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
            let state = home_state(home, &mut guard)?;
            let budget = group_budget(home, state, group_id)?;
            if !budget.cue_pending(&actor.id) {
                continue;
            }
            (
                budget.pending(&actor.id).cloned(),
                budget.settled_attempt(&actor.id).cloned(),
            )
        };
        let Some(pending) = pending else {
            return Err(io::Error::other("wake cue lacks source"));
        };
        let Some(attempt) = attempt else {
            return Err(io::Error::other("wake cue lacks settled owner binding"));
        };
        if pending.generation != actors::generation_identity(&actor)
            || pending.policy_fingerprint != policy_fingerprint(home, &group, &actor)?
        {
            continue;
        }
        let awaited = mail_pending::still_awaited(
            home,
            &group,
            &actor.id,
            std::slice::from_ref(&pending.source_event_id),
        )?;
        if !awaited.contains(&pending.source_event_id) {
            continue;
        }
        if with_confirmed_owner(home, &group, &actor, &attempt, || Ok(()))?.is_none() {
            continue;
        }
        if !super::automation_runtime::notify_confirmed_mail_wake(
            home,
            group_id,
            &actor.id,
            &pending.source_event_id,
        )? {
            continue;
        }
        let mut guard = homes()
            .lock()
            .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
        let state = home_state(home, &mut guard)?;
        group_budget(home, state, group_id)?.clear_pending(&actor.id)?;
    }
    Ok(())
}

/// The global read permit freezes group registration while each group is
/// collected under its own permit. An error holds the whole pass.
fn census(
    home: &HomeLayout,
    locks: &crate::dispatch_concurrency::DispatchLocks,
) -> io::Result<Vec<Candidate>> {
    locks.with_stable_census_blocking(|locks| {
        let ids = cccc_core::automation::group_ids(home)?;
        let mut candidates = Vec::new();
        for group_id in ids {
            candidates.extend(locks.with_census_group_blocking(&group_id, || {
                let candidates = group_candidates(home, &group_id)?;
                repair_cues(home, &group_id)?;
                Ok::<_, io::Error>(candidates)
            })?);
        }
        order_candidates(&mut candidates);
        Ok(candidates)
    })
}

fn order_candidates(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        (&a.source_time, &a.group_id, &a.actor_id, &a.source_id).cmp(&(
            &b.source_time,
            &b.group_id,
            &b.actor_id,
            &b.source_id,
        ))
    });
}

fn stopped_owner(group: &GroupDoc, actor: &Actor) -> io::Result<bool> {
    if super::local_headless::waiting_for_workspace_trust(&group.group_id, &actor.id)?
        || super::local_headless::confirmation_pending_active(&group.group_id, &actor.id)?
        || super::deepseek_runtime::has_registered_owner(&group.group_id, &actor.id)?
    {
        return Ok(false);
    }
    if super::local_headless::owner_or_starting(&group.group_id, &actor.id)?
        && !super::local_headless::retire_confirmed_stopped(&group.group_id, &actor.id)?
    {
        return Ok(false);
    }
    if super::actor_runtime::is_structured(actor)
        && actor.runtime != cccc_contracts::ActorRuntime::Deepseek
    {
        return Ok(false);
    }
    match cccc_runtime::status(&group.group_id, &actor.id) {
        // A registered PTY remains an owner even if its status says exited.
        // Let its lifecycle remove the entry before admitting a wake.
        Ok(_) => Ok(false),
        Err(cccc_runtime::RuntimeError::NotFound(_, _)) => Ok(true),
        Err(error) => Err(io::Error::other(error)),
    }
}

pub(super) fn confirmed_running(group: &GroupDoc, actor: &Actor) -> bool {
    if super::local_headless::supports(actor) {
        super::local_headless::confirmed_running(&group.group_id, &actor.id).unwrap_or(false)
    } else if actor.runtime == cccc_contracts::ActorRuntime::Deepseek {
        super::deepseek_runtime::confirmed_running(&group.group_id, &actor.id).unwrap_or(false)
    } else {
        cccc_runtime::confirmed_running(&group.group_id, &actor.id).unwrap_or(false)
    }
}

fn record_start(home: &HomeLayout, group: &GroupDoc, attempt: &budget::Attempt) -> io::Result<()> {
    let store = GroupStore::new(home.clone())?;
    let path = store.ledger_path(&group.group_id)?;
    let mut event = Event::new("actor.start", &group.group_id);
    event.id = attempt.id.clone();
    event.by = "system".into();
    event.data = json!({"actor_id":attempt.actor_id, "reason":"mail_wake",
        "source_event_id":attempt.source_event_id, "actor_generation":attempt.generation})
    .as_object()
    .cloned()
    .ok_or_else(|| io::Error::other("invalid wake event"))?;
    match cccc_core::ledger::find_event(&path, &event.id)? {
        Some(existing)
            if existing.kind == event.kind
                && existing.by == event.by
                && existing.data == event.data =>
        {
            Ok(())
        }
        Some(_) => Err(io::Error::other("Mail wake event identity collision")),
        None => cccc_core::ledger::append(&path, &event),
    }
}

fn wake_origin(attempt: &budget::Attempt) -> cccc_core::wake_origin::WakeOrigin {
    cccc_core::wake_origin::WakeOrigin {
        attempt_id: attempt.id.clone(),
        actor_generation: attempt.generation.clone(),
        policy_fingerprint: attempt.policy_fingerprint.clone(),
        source_event_id: attempt.source_event_id.clone(),
    }
}

fn with_confirmed_owner<T>(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    attempt: &budget::Attempt,
    confirmed: impl FnOnce() -> io::Result<T>,
) -> io::Result<Option<T>> {
    let origin = wake_origin(attempt);
    if actor.runtime == cccc_contracts::ActorRuntime::Deepseek {
        super::deepseek_runtime::with_confirmed_wake(home, group, actor, &origin, confirmed)
    } else if super::local_headless::supports(actor) {
        super::local_headless::with_confirmed_wake(home, group, actor, &origin, confirmed)
    } else if !super::actor_runtime::is_structured(actor) {
        super::actor_runtime::with_confirmed_pty_wake(home, group, actor, &origin, confirmed)
    } else {
        Ok(None)
    }
}

/// Record a proven pre-launch refusal; uncertain outcomes never use this.
fn settle_known_failure(home: &HomeLayout, attempt: &budget::Attempt) -> io::Result<()> {
    let mut guard = homes()
        .lock()
        .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
    let state = home_state(home, &mut guard)?;
    group_budget(home, state, &attempt.group_id)?.settle(attempt, false)?;
    state.global.settle(attempt)
}

/// Recheck the nominated source and policy while holding its Group permit.
/// Global and group debits commit in that order before invoking an adapter.
enum Admission {
    Stale,
    Held,
    Started,
}

fn admit(home: &HomeLayout, candidate: &Candidate) -> io::Result<Admission> {
    let candidates = group_candidates(home, &candidate.group_id)?;
    if !candidates.iter().any(|current| {
        current.actor_id == candidate.actor_id
            && current.generation == candidate.generation
            && current.source_id == candidate.source_id
            && current.fingerprint == candidate.fingerprint
    }) {
        return Ok(Admission::Stale);
    }
    let store = GroupStore::new(home.clone())?;
    let group = store.load(&candidate.group_id)?;
    let stored = actors::visible(&group)
        .find(|actor| actor.id == candidate.actor_id)
        .ok_or_else(|| io::Error::other("Mail wake actor no longer exists"))?;
    let actor = super::actor_profile_runtime::resolve(home, stored)
        .map_err(|error| io::Error::other(error.message))?;
    if !eligible_group(&group)
        || !eligible_actor(&actor)
        || actors::generation_identity(&actor) != candidate.generation
        || policy_fingerprint(home, &group, &actor)? != candidate.fingerprint
        || !stopped_owner(&group, &actor)?
        || super::runtime_restore::deepseek_restore_blocked(home, &group, &actor)
    {
        return Ok(Admission::Stale);
    }
    if actor.runtime != cccc_contracts::ActorRuntime::Deepseek
        && !super::local_headless::supports(&actor)
        && super::actor_runtime::is_structured(&actor)
    {
        // Other owner paths stay held until their own durable binding exists.
        return Ok(Admission::Held);
    }
    let awaited = mail_pending::still_awaited(
        home,
        &group,
        &actor.id,
        std::slice::from_ref(&candidate.source_id),
    )?;
    if !awaited.contains(&candidate.source_id) {
        return Ok(Admission::Stale);
    }
    let attempt = budget::Attempt {
        id: uuid::Uuid::new_v4().simple().to_string(),
        group_id: candidate.group_id.clone(),
        actor_id: candidate.actor_id.clone(),
        generation: candidate.generation.clone(),
        policy_fingerprint: candidate.fingerprint.clone(),
        source_event_id: candidate.source_id.clone(),
    };
    {
        let mut guard = homes()
            .lock()
            .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
        let state = home_state(home, &mut guard)?;
        let Some(prepared) =
            group_budget(home, state, &candidate.group_id)?.prepare(&attempt, Instant::now())?
        else {
            return Ok(Admission::Held);
        };
        if !state.global.reserve(attempt.clone(), Instant::now())? {
            return Ok(Admission::Held);
        }
        // A failure after the global debit is unknown and holds all wake work.
        group_budget(home, state, &candidate.group_id)?.commit(prepared)?;
    }
    // DeepSeek's existing launch path does not take the daemon start permit.
    // Other adapters take it internally; holding it twice would deadlock.
    let deepseek_permit = if actor.runtime == cccc_contracts::ActorRuntime::Deepseek {
        match crate::runtime_start_gate::permit(home) {
            Ok(permit) => Some(permit),
            Err(_) => {
                settle_known_failure(home, &attempt)?;
                return Ok(Admission::Started);
            }
        }
    } else {
        None
    };
    let origin = wake_origin(&attempt);
    let launch = if actor.runtime == cccc_contracts::ActorRuntime::Deepseek {
        super::deepseek_runtime::apply_wake(home, &group, &actor, &origin)
    } else if super::local_headless::supports(&actor) {
        super::actor_runtime::start_managed_wake(home, &group, &actor, &origin)
    } else {
        super::actor_runtime::start_pty_wake(home, &group, &actor, &origin).map(|_| ())
    };
    drop(deepseek_permit);
    if let Err(error) = launch {
        if matches!(
            error.code.as_str(),
            "runtime_shutting_down" | "setup_required"
        ) {
            // Both refusals occur before their adapter creates a process.
            settle_known_failure(home, &attempt)?;
            return Ok(Admission::Started);
        }
        tracing::warn!(code = %error.code, group_id = %group.group_id, actor_id = %actor.id,
                       "Mail wake launch outcome held for reconciliation");
        return Err(io::Error::other("Mail wake launch outcome unknown"));
    }
    let settled = with_confirmed_owner(home, &group, &actor, &attempt, || {
        record_start(home, &group, &attempt)?;
        let mut guard = homes()
            .lock()
            .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
        let state = home_state(home, &mut guard)?;
        group_budget(home, state, &candidate.group_id)?.settle(&attempt, true)?;
        state.global.settle(&attempt)
    })?;
    if settled.is_none() {
        return Err(io::Error::other(
            "Mail wake owner not confirmed after launch",
        ));
    }
    // This can remain pending if the existing notice path reports uncertainty.
    match super::automation_runtime::notify_confirmed_mail_wake(
        home,
        &group.group_id,
        &actor.id,
        &attempt.source_event_id,
    ) {
        Ok(true) => {}
        Ok(false) => return Ok(Admission::Started),
        Err(error) => {
            tracing::warn!(%error, "Mail wake cue remains pending");
            return Ok(Admission::Started);
        }
    }
    let mut guard = homes()
        .lock()
        .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
    let state = home_state(home, &mut guard)?;
    group_budget(home, state, &candidate.group_id)?.clear_pending(&actor.id)?;
    Ok(Admission::Started)
}

/// An unresolved launch can settle only against its own actual owner and the
/// matching durable group result. Unsupported or unobservable owners hold.
fn reconcile(
    home: &HomeLayout,
    locks: &crate::dispatch_concurrency::DispatchLocks,
) -> io::Result<()> {
    let attempt = {
        let mut guard = homes()
            .lock()
            .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
        let global = &home_state(home, &mut guard)?.global;
        global.in_flight()
    };
    let Some(attempt) = attempt else {
        return Ok(());
    };
    locks.with_group_write_blocking(&attempt.group_id, || {
        let store = GroupStore::new(home.clone())?;
        let group = store.load(&attempt.group_id)?;
        let stored = actors::visible(&group)
            .find(|actor| actor.id == attempt.actor_id)
            .ok_or_else(|| io::Error::other("unresolved wake owner missing"))?;
        let actor = super::actor_profile_runtime::resolve(home, stored)
            .map_err(|error| io::Error::other(error.message))?;
        if actors::generation_identity(&actor) != attempt.generation
            || policy_fingerprint(home, &group, &actor)? != attempt.policy_fingerprint
        {
            return Err(io::Error::other("unresolved Mail wake owner attribution"));
        }
        with_confirmed_owner(home, &group, &actor, &attempt, || {
            let (in_flight, settled) = {
                let mut guard = homes()
                    .lock()
                    .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
                let state = home_state(home, &mut guard)?;
                let group = group_budget(home, state, &attempt.group_id)?;
                (
                    group.attempt_matches(&attempt),
                    group.settled_matches(&attempt),
                )
            };
            if !in_flight && !settled {
                return Err(io::Error::other("unresolved Mail wake group result"));
            }
            record_start(home, &group, &attempt)?;
            let mut guard = homes()
                .lock()
                .map_err(|_| io::Error::other("Mail wake state lock poisoned"))?;
            let state = home_state(home, &mut guard)?;
            if in_flight {
                group_budget(home, state, &attempt.group_id)?.settle(&attempt, true)?;
            }
            state.global.settle(&attempt)
        })?
        .ok_or_else(|| io::Error::other("unresolved Mail wake owner attribution"))
    })
}

/// Called by the existing scheduler from a blocking worker. At most one
/// candidate is admitted per pass; uncertainty holds every automatic wake.
pub(crate) fn run(
    home: &HomeLayout,
    locks: &crate::dispatch_concurrency::DispatchLocks,
) -> io::Result<()> {
    if cccc_runtime::owned_process_recovery_pending() {
        return Err(io::Error::other("owned process recovery is incomplete"));
    }
    reconcile(home, locks)?;
    let candidates = census(home, locks)?;
    for candidate in candidates {
        match locks.with_group_write_blocking(&candidate.group_id, || admit(home, &candidate))? {
            Admission::Stale => continue,
            Admission::Held | Admission::Started => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn restart_cue_control(
        confirmed: bool,
        notice_before_restart: bool,
        ledger_fault: Option<&str>,
        test_name: &str,
    ) {
        const CHILD: &str = "CCCC_MAIL_WAKE_RESTART_CONTROL";
        if std::env::var_os(CHILD).is_some() {
            let home = HomeLayout::from_path(
                std::env::var_os("CCCC_MAIL_WAKE_CONTROL_HOME").expect("control home"),
            )
            .expect("home");
            let group_id = std::env::var("CCCC_MAIL_WAKE_CONTROL_GROUP").expect("group");
            recover_on_start(&home).expect("restart recovery");
            let store = GroupStore::new(home.clone()).expect("store");
            let group = store.load(&group_id).expect("group");
            let actor = group.actors[0].clone();
            let attempt = {
                let mut guard = homes().lock().expect("homes");
                let state = home_state(&home, &mut guard).expect("state");
                let budget = group_budget(&home, state, &group_id).expect("budget");
                assert!(budget.cue_pending(&actor.id));
                budget.settled_attempt(&actor.id).expect("settled").clone()
            };
            assert_eq!(
                policy_fingerprint(&home, &group, &actor).expect("restart fingerprint"),
                attempt.policy_fingerprint
            );
            // Rebuild the unread census in a genuinely separate process. A
            // process-local key would erase the persisted cue here.
            assert!(
                group_candidates(&home, &group_id)
                    .expect("census")
                    .is_empty()
            );
            if confirmed {
                // Labeled local control: install an actual matching owner in
                // this process. This is not a claim of live owner reattachment.
                struct Cleanup(String, String);
                impl Drop for Cleanup {
                    fn drop(&mut self) {
                        let _ = cccc_runtime::stop(&self.0, &self.1);
                    }
                }
                let _cleanup = Cleanup(group_id.clone(), actor.id.clone());
                super::super::actor_runtime::start_pty_wake(
                    &home,
                    &group,
                    &actor,
                    &wake_origin(&attempt),
                )
                .expect("matching owner control");
                let ledger = store.ledger_path(&group_id).expect("ledger");
                let original = std::fs::read(&ledger).expect("saved ledger bytes");
                let backup = ledger.with_extension("held");
                let expected = match ledger_fault {
                    Some("malformed") => {
                        // Labeled truncation fault derived from retained Mail.
                        // A complete matching notice remains before this tail.
                        let captured = include_bytes!("fixtures/mail.json");
                        let mut file = std::fs::OpenOptions::new()
                            .append(true)
                            .open(&ledger)
                            .expect("fault writer");
                        std::io::Write::write_all(&mut file, &captured[..captured.len() / 2])
                            .expect("truncated captured record");
                        std::io::Write::write_all(&mut file, b"\n").expect("fault newline");
                        file.sync_all().expect("sync fault");
                        Some(std::fs::read(&ledger).expect("fault bytes"))
                    }
                    Some("unreadable") => {
                        // Owned scratch path becomes a directory, so even a
                        // privileged CI runner gets a real source-read error.
                        std::fs::rename(&ledger, &backup).expect("retain ledger");
                        std::fs::create_dir(&ledger).expect("unreadable source");
                        None
                    }
                    None => None,
                    Some(_) => panic!("unknown ledger fault"),
                };
                let repair = repair_cues(&home, &group_id);
                if ledger_fault.is_some() {
                    assert!(
                        cccc_core::ledger::contains_validated(&ledger, |_| true).is_err(),
                        "even an earlier match must not hide a later read/parse failure",
                    );
                    assert!(repair.is_err(), "read/parse uncertainty must hold");
                    if let Some(expected) = expected {
                        assert_eq!(std::fs::read(&ledger).expect("fault retained"), expected);
                    } else {
                        assert!(ledger.is_dir());
                        assert_eq!(std::fs::read(&backup).expect("ledger retained"), original);
                    }
                } else {
                    repair.expect("repair cue");
                    let events =
                        cccc_core::ledger::read_all(&store.ledger_path(&group_id).expect("ledger"))
                            .expect("events");
                    let matching: Vec<_> = events
                        .iter()
                        .filter(|event| {
                            event.kind == "system.notify"
                                && event.data.get("kind") == Some(&json!("mail_notice"))
                                && event.data.get("target_actor_id") == Some(&json!(actor.id))
                                && event.data["context"]["source_event_ids"]
                                    .as_array()
                                    .is_some_and(|ids| {
                                        ids.contains(&json!(attempt.source_event_id))
                                    })
                        })
                        .collect();
                    assert_eq!(matching.len(), 1, "notice must not be duplicated");
                }
            } else {
                // No owner registry is carried into this process; neither a
                // missing owner nor a replacement is authority to relaunch.
                let _ = repair_cues(&home, &group_id);
                let _ = run(
                    &home,
                    &crate::dispatch_concurrency::DispatchLocks::default(),
                );
                assert!(cccc_runtime::status(&group_id, &actor.id).is_err());
            }
            if notice_before_restart && ledger_fault.is_none() {
                let events =
                    cccc_core::ledger::read_all(&store.ledger_path(&group_id).expect("ledger"))
                        .expect("retained notices");
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| {
                            event.kind == "system.notify"
                                && event.data["kind"] == "mail_notice"
                                && event.data["target_actor_id"] == actor.id
                                && event.data["context"]["source_event_ids"]
                                    .as_array()
                                    .is_some_and(|ids| {
                                        ids.contains(&json!(attempt.source_event_id))
                                    })
                        })
                        .count(),
                    1,
                    "the pre-crash notice stays unique even without a proven owner",
                );
            }
            let mut guard = homes().lock().expect("homes");
            let state = home_state(&home, &mut guard).expect("state");
            let budget = group_budget(&home, state, &group_id).expect("budget");
            let held = !confirmed || ledger_fault.is_some();
            assert_eq!(budget.cue_pending(&actor.id), held);
            if held {
                assert_eq!(budget.settled_attempt(&actor.id), Some(&attempt));
            }
            return;
        }

        let mut event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let created = store.create("restart-control", "").expect("group");
        let mut group = cccc_core::group_scope::attach(
            &store,
            &created.group_id,
            cccc_core::scope::detect(temp.path()).expect("scope"),
        )
        .expect("attach");
        let mut actor = Actor::new("receiver");
        actor.runtime = cccc_contracts::ActorRuntime::Custom;
        actor.command = vec!["sh".into(), "-c".into(), "exec sleep 30".into()];
        actor.generation = "generation-control".into();
        actor.wake_on_mail = true;
        group.actors.push(actor);
        group.running = true;
        group.state = GroupState::Active;
        store.save(&group).expect("group");
        let group = store.load(&group.group_id).expect("reload");
        let actor = group.actors[0].clone();
        event.group_id = group.group_id.clone(); // labeled captured-event routing
        cccc_core::ledger::append(&store.ledger_path(&group.group_id).expect("ledger"), &event)
            .expect("captured Mail");
        activate_control(&home);
        GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
        recover_existing(&home).expect("readiness");
        let fingerprint = policy_fingerprint(&home, &group, &actor).expect("fingerprint");
        let attempt = budget::Attempt {
            id: uuid::Uuid::new_v4().simple().to_string(),
            group_id: group.group_id.clone(),
            actor_id: actor.id.clone(),
            generation: actors::generation_identity(&actor),
            policy_fingerprint: fingerprint.clone(),
            source_event_id: event.id.clone(),
        };
        {
            let now = Instant::now();
            let mut guard = homes().lock().expect("homes");
            let state = home_state(&home, &mut guard).expect("state");
            let budget = group_budget(&home, state, &group.group_id).expect("group budget");
            budget
                .note(
                    &actor.id,
                    Pending {
                        source_event_id: event.id.clone(),
                        source_ts: event.ts.clone(),
                        generation: attempt.generation.clone(),
                        policy_fingerprint: fingerprint,
                    },
                )
                .expect("pending");
            let prepared = budget
                .prepare(&attempt, now)
                .expect("prepare")
                .expect("admitted");
            assert!(
                state
                    .global
                    .reserve(attempt.clone(), now)
                    .expect("global debit")
            );
            let budget = group_budget(&home, state, &group.group_id).expect("group budget");
            budget.commit(prepared).expect("group debit");
            // Labeled durable settled-cue control, before notice delivery.
            budget.settle(&attempt, true).expect("settle group");
            state.global.settle(&attempt).expect("settle global");
        }
        if notice_before_restart {
            // Labeled crash window: durably append a notice and leave the cue
            // uncleared before checking recovery in a fresh process.
            let result = cccc_core::automation::notify_wake_mail(&home, &group.group_id, &actor.id)
                .expect("append notice before restart");
            assert!(result.unread_failure.is_none());
            assert_eq!(result.notifications.len(), 1);
            assert_eq!(result.notifications[0].data["target_actor_id"], actor.id);
            assert!(
                result.notifications[0].data["context"]["source_event_ids"]
                    .as_array()
                    .is_some_and(|ids| ids.contains(&json!(event.id)))
            );
        }
        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, "1")
            .env("CCCC_MAIL_WAKE_CONTROL_HOME", home.root())
            .env("CCCC_MAIL_WAKE_CONTROL_GROUP", &group.group_id)
            .status()
            .expect("fresh process");
        assert!(child.success(), "fresh-process control failed: {child}");
    }

    #[cfg(unix)]
    #[test]
    fn restart_cue_repairs_only_with_confirmed_owner() {
        restart_cue_control(
            true,
            false,
            None,
            "ops::mail_wake::tests::restart_cue_repairs_only_with_confirmed_owner",
        );
    }

    #[cfg(unix)]
    #[test]
    fn restart_cue_holds_when_owner_unproven() {
        restart_cue_control(
            false,
            false,
            None,
            "ops::mail_wake::tests::restart_cue_holds_when_owner_unproven",
        );
    }

    #[cfg(unix)]
    #[test]
    fn restart_cue_accepts_the_notice_already_appended_before_crash() {
        restart_cue_control(
            true,
            true,
            None,
            "ops::mail_wake::tests::restart_cue_accepts_the_notice_already_appended_before_crash",
        );
    }

    #[cfg(unix)]
    #[test]
    fn restart_cue_retains_notice_when_owner_unproven() {
        restart_cue_control(
            false,
            true,
            None,
            "ops::mail_wake::tests::restart_cue_retains_notice_when_owner_unproven",
        );
    }

    #[cfg(unix)]
    #[test]
    fn restart_cue_holds_on_malformed_tail_after_matching_notice() {
        restart_cue_control(
            true,
            true,
            Some("malformed"),
            "ops::mail_wake::tests::restart_cue_holds_on_malformed_tail_after_matching_notice",
        );
    }

    #[cfg(unix)]
    #[test]
    fn restart_cue_holds_on_ledger_read_failure() {
        restart_cue_control(
            true,
            true,
            Some("unreadable"),
            "ops::mail_wake::tests::restart_cue_holds_on_ledger_read_failure",
        );
    }

    #[cfg(unix)]
    fn activate_control(home: &HomeLayout) {
        use cccc_core::registry::{MailWakeActivation, MailWakePhase};
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(event.kind, "chat.message");
        // Labeled control, never a production first-use authorization.
        cccc_core::Registry::load(home).expect("registry setup");
        cccc_core::Registry::mutate_existing(home, |registry| {
            let identity = home_identity(home)?;
            let implementation = implementation_digest()?;
            registry.mail_wake_activation = Some(MailWakeActivation {
                version: ACTIVATION_SCHEMA,
                activation_id: "activation-control".into(),
                proof_digest: fresh_proof_digest(&identity, &implementation, "activation-control"),
                implementation,
                home_identity: identity,
                phase: MailWakePhase::Active,
            });
            Ok(())
        })
        .expect("activation control");
        Budget::initialize(&global_path(home)).expect("limiter setup");
    }

    #[cfg(unix)]
    #[test]
    fn fresh_creation_activates_once_and_existing_empty_home_holds() {
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("fresh")).expect("home");
        bootstrap(&home).expect("bootstrap");
        let header = active_header(&home).expect("active");
        {
            let mut state = homes().lock().expect("homes");
            assert!(home_state(&home, &mut state).is_err());
        }
        recover_existing(&home).expect("strict recovery");
        {
            let mut state = homes().lock().expect("homes");
            assert!(home_state(&home, &mut state).is_ok());
        }
        let mut global = Budget::load(&global_path(&home), Instant::now()).expect("limiter");
        assert_eq!(global.activation_id(), header.activation_id);
        let attempt = budget::Attempt {
            id: "attempt-control".into(),
            group_id: event.group_id,
            actor_id: "receiver".into(),
            generation: "generation-control".into(),
            policy_fingerprint: "a".repeat(64),
            source_event_id: event.id,
        };
        assert!(
            global
                .reserve(attempt.clone(), Instant::now())
                .expect("debit")
        );
        bootstrap(&home).expect("reopen");
        let reopened = Budget::load(&global_path(&home), Instant::now()).expect("limiter");
        assert_eq!(reopened.in_flight(), Some(attempt));
        assert_eq!(active_header(&home).expect("active"), header);

        let existing_path = temp.path().join("existing-empty");
        std::fs::create_dir(&existing_path).expect("existing directory control");
        let existing = HomeLayout::from_path(&existing_path).expect("home");
        bootstrap(&existing).expect("bootstrap");
        assert!(active_header(&existing).is_err());
        assert!(!global_path(&existing).exists());
    }

    #[cfg(unix)]
    #[test]
    fn fresh_active_rename_error_requires_new_strict_recovery() {
        let capture: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(capture.kind, "chat.message");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("fresh")).expect("home");
        let registry_path = home.registry_path();
        let mut injected = false;
        let mut fault = |path: &std::path::Path, stage: PublishStage| {
            if path == registry_path && stage == PublishStage::DirectorySync {
                injected = true;
                return Err(io::Error::other("labeled post-rename failure"));
            }
            Ok(())
        };
        bootstrap_with(&home, &mut fault).expect("home bootstrap remains available");
        assert!(injected);
        // ACTIVE bytes are visible, but the failed operation did not create
        // readiness and ordinary startup cannot silently retry its latch.
        let visible = std::fs::read(&registry_path).expect("visible registry");
        let header = active_header(&home).expect("visible active candidate");
        assert_eq!(std::fs::read(&registry_path).expect("readback"), visible);
        assert!(home_state(&home, &mut homes().lock().expect("homes")).is_err());
        assert!(recover_on_start(&home).is_err());
        assert!(
            run(
                &home,
                &crate::dispatch_concurrency::DispatchLocks::default()
            )
            .is_err()
        );
        recover_existing(&home).expect("explicit strict recovery now");
        assert_eq!(active_header(&home).expect("same active candidate"), header);
        assert_eq!(std::fs::read(&registry_path).expect("same bytes"), visible);
        assert!(home_state(&home, &mut homes().lock().expect("homes")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn fresh_active_pre_rename_failure_is_unpublished_and_removes_temp() {
        let capture: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(capture.kind, "chat.message");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("fresh")).expect("home");
        let registry_path = home.registry_path();
        let mut before_error = None;
        let mut fault = |path: &std::path::Path, stage: PublishStage| {
            if path == registry_path && stage == PublishStage::Rename {
                before_error = Some(
                    std::fs::read_dir(home.root())?
                        .map(|entry| entry.map(|entry| entry.file_name()))
                        .collect::<io::Result<std::collections::BTreeSet<_>>>()?,
                );
                return Err(io::Error::other("labeled pre-rename failure"));
            }
            Ok(())
        };
        bootstrap_with(&home, &mut fault).expect("home bootstrap remains available");
        let during = before_error.expect("fault was reached");
        let after = std::fs::read_dir(home.root())
            .expect("home entries")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            during.difference(&after).count(),
            1,
            "temporary entry removed"
        );
        assert!(after.difference(&during).next().is_none());
        let registry = cccc_core::Registry::load_existing(&home).expect("intact preparation");
        assert_eq!(
            registry.mail_wake_activation.expect("header").phase,
            cccc_core::registry::MailWakePhase::Preparing
        );
        assert!(active_header(&home).is_err());
        assert!(recover_existing(&home).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fresh_active_post_rename_failure_reports_published_unknown() {
        let capture: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(capture.kind, "chat.message");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("fresh")).expect("home");
        let fresh = home
            .try_create_fresh()
            .expect("create")
            .expect("fresh capability");
        home.initialize().expect("home");
        cccc_core::Registry::load(&home).expect("new registry");
        let registry_path = home.registry_path();
        let mut injected = false;
        let mut fault = |path: &std::path::Path, stage: PublishStage| {
            if path == registry_path && stage == PublishStage::DirectorySync {
                injected = true;
                return Err(io::Error::other("labeled sync failure"));
            }
            Ok(())
        };
        let error = activate_fresh(&home, &fresh, &mut fault).expect_err("unknown durability");
        assert!(injected);
        assert!(error.to_string().contains("published; durability unknown"));
        let visible = std::fs::read(&registry_path).expect("published bytes");
        let header = active_header(&home).expect("checksummed ACTIVE candidate");
        assert!(home_state(&home, &mut homes().lock().expect("homes")).is_err());
        recover_existing(&home).expect("explicit strict recovery");
        assert_eq!(active_header(&home).expect("same header"), header);
        assert_eq!(std::fs::read(&registry_path).expect("same bytes"), visible);
    }

    #[cfg(unix)]
    #[test]
    fn torn_or_checksum_mismatched_registry_is_unknown_not_reinitialized() {
        let capture: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(capture.kind, "chat.message");
        for damage in ["checksum", "truncated", "stripped"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let home = HomeLayout::from_path(temp.path().join("fresh")).expect("home");
            bootstrap(&home).expect("fresh activation");
            recover_existing(&home).expect("initial readiness");
            let path = home.registry_path();
            let original = std::fs::read(&path).expect("registry");
            let damaged = if damage == "truncated" {
                original[..original.len() / 2].to_vec()
            } else {
                let mut doc: serde_json::Value =
                    serde_json::from_slice(&original).expect("registry JSON");
                if damage == "stripped" {
                    let object = doc.as_object_mut().expect("registry object");
                    object.remove("mail_wake_activation");
                    object.remove("mail_wake_registry_sha256");
                } else {
                    doc["updated_at"] = json!("labeled integrity mutation");
                }
                serde_json::to_vec(&doc).expect("damaged registry JSON")
            };
            std::fs::write(&path, &damaged).expect("labeled torn bytes");
            assert!(cccc_core::Registry::load_existing(&home).is_err());
            assert!(cccc_core::Registry::load(&home).is_err());
            assert!(recover_existing(&home).is_err());
            assert!(home_state(&home, &mut homes().lock().expect("homes")).is_err());
            assert_eq!(
                std::fs::read(&path).expect("damaged bytes retained"),
                damaged
            );
            // Labeled restoration of the retained valid source. Recovery
            // re-derives integrity from that source; it never invents the
            // missing registry payload from damaged bytes.
            std::fs::write(&path, &original).expect("restore trusted source control");
            recover_existing(&home).expect("rederive from restored valid source");
            assert_eq!(std::fs::read(&path).expect("restored bytes"), original);
        }
    }

    #[cfg(unix)]
    #[test]
    fn preparing_lost_limiter_and_wrong_activation_hold_without_reset() {
        use cccc_core::registry::MailWakePhase;
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(event.kind, "chat.message");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        bootstrap(&home).expect("bootstrap");
        cccc_core::Registry::mutate_existing(&home, |registry| {
            registry
                .mail_wake_activation
                .as_mut()
                .expect("header")
                .phase = MailWakePhase::Preparing;
            Ok(())
        })
        .expect("labeled interrupted activation");
        bootstrap(&home).expect("reopen");
        assert!(active_header(&home).is_err());
        cccc_core::Registry::mutate_existing(&home, |registry| {
            let header = registry.mail_wake_activation.as_mut().expect("header");
            header.phase = MailWakePhase::Active;
            header.activation_id = "wrong-control".into();
            Ok(())
        })
        .expect("labeled activation mismatch");
        assert!(home_state(&home, &mut homes().lock().expect("homes")).is_err());
        std::fs::remove_file(global_path(&home)).expect("labeled limiter loss");
        bootstrap(&home).expect("reopen");
        assert!(home_state(&home, &mut Homes::new()).is_err());
        assert!(!global_path(&home).exists());
    }

    #[cfg(unix)]
    #[test]
    fn strict_recovery_faults_never_create_readiness_or_erase_attempt() {
        let source: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        for stage in [
            PublishStage::Write,
            PublishStage::DataSync,
            PublishStage::Rename,
            PublishStage::DirectorySync,
            PublishStage::AncestorSync,
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
            let store = GroupStore::new(home.clone()).expect("store");
            let group = store.create("recovery-control", "").expect("group");
            activate_control(&home);
            let mut state =
                GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("state");
            let pending = Pending {
                source_event_id: source.id.clone(),
                source_ts: source.ts.clone(),
                generation: "generation-control".into(),
                policy_fingerprint: "a".repeat(64),
            };
            let attempt = budget::Attempt {
                id: "attempt-control".into(),
                group_id: group.group_id.clone(),
                actor_id: "receiver".into(),
                generation: pending.generation.clone(),
                policy_fingerprint: pending.policy_fingerprint.clone(),
                source_event_id: source.id.clone(),
            };
            state.note(&attempt.actor_id, pending).expect("nomination");
            let prepared = state
                .prepare(&attempt, Instant::now())
                .expect("prepare")
                .expect("admit");
            let mut budget = Budget::load(&global_path(&home), Instant::now()).expect("limiter");
            assert!(
                budget
                    .reserve(attempt.clone(), Instant::now())
                    .expect("reserve")
            );
            state.commit(prepared).expect("group debit");
            let registry_path = home.registry_path();
            let before_registry = std::fs::read(&registry_path).expect("registry");
            let before_global = std::fs::read(global_path(&home)).expect("limiter");
            let state_path = store
                .state_dir(&group.group_id)
                .expect("dir")
                .join("automation.json");
            let before_group = std::fs::read(&state_path).expect("group");
            let mut injected = false;
            let mut fault = |path: &std::path::Path, current: PublishStage| {
                let target = if stage == PublishStage::AncestorSync {
                    path == home.root()
                } else {
                    path == registry_path
                };
                if target && current == stage {
                    injected = true;
                    return Err(io::Error::other("labeled publication fault"));
                }
                Ok(())
            };
            assert!(attempt_recovery(&home, true, &mut fault).is_err());
            assert!(injected);
            // DirectorySync fails after the actual registry rename. Matching
            // visible bytes cannot create readiness or certify that write.
            assert_eq!(
                std::fs::read(&registry_path).expect("visible registry"),
                before_registry
            );
            assert_eq!(
                std::fs::read(global_path(&home)).expect("limiter"),
                before_global
            );
            assert_eq!(std::fs::read(&state_path).expect("group"), before_group);
            {
                let mut state = homes().lock().expect("homes");
                assert!(home_state(&home, &mut state).is_err());
            }
            assert!(recover_on_start(&home).is_err());
            assert!(
                run(
                    &home,
                    &crate::dispatch_concurrency::DispatchLocks::default()
                )
                .is_err()
            );
            assert!(cccc_runtime::status(&group.group_id, "receiver").is_err());
            if stage == PublishStage::DirectorySync {
                // Labeled simulated process restart: forget only the local
                // latch; matching on-disk bytes still do not grant readiness.
                failed_publications()
                    .lock()
                    .expect("latch")
                    .remove(home.root());
                {
                    let mut state = homes().lock().expect("homes");
                    assert!(home_state(&home, &mut state).is_err());
                }
                recover_on_start(&home).expect("new-load strict recovery");
            } else {
                recover_existing(&home).expect("explicit recovery now");
            }
            let mut ready = homes().lock().expect("homes");
            let ready = home_state(&home, &mut ready).expect("readiness");
            assert_eq!(ready.global.in_flight(), Some(attempt));
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovery_holds_on_group_writer_and_changed_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("recovery-control", "").expect("group");
        activate_control(&home);
        GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("state");
        let group_path = store
            .group_dir(&group.group_id)
            .expect("group dir")
            .join("group.yaml");
        let lock_path = store
            .group_dir(&group.group_id)
            .expect("group dir")
            .join("group.yaml.lock");
        cccc_core::fs::with_exclusive_lock(&lock_path, || {
            assert!(recover_existing(&home).is_err());
            Ok(())
        })
        .expect("writer lock");

        let original = std::fs::read(&group_path).expect("original group");
        let mut changed = original.clone();
        changed.extend_from_slice(b"\n# labeled concurrent edit\n");
        let global = global_path(&home);
        let mut injected = false;
        let mut fault = |path: &std::path::Path, stage: PublishStage| {
            if !injected && path == global && stage == PublishStage::DirectorySync {
                // Labeled mutation models an uncooperative writer between
                // snapshot and publication; recovery must not overwrite it.
                std::fs::write(&group_path, &changed)?;
                injected = true;
            }
            Ok(())
        };
        assert!(attempt_recovery(&home, true, &mut fault).is_err());
        assert!(injected);
        assert_eq!(std::fs::read(&group_path).expect("group"), changed);
        assert!(home_state(&home, &mut homes().lock().expect("homes")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn prerequisite_publication_failure_never_establishes_readiness() {
        let capture: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(capture.kind, "chat.message");
        for target in 0..3 {
            let temp = tempfile::tempdir().expect("tempdir");
            let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
            let store = GroupStore::new(home.clone()).expect("store");
            let group = store.create("recovery-control", "").expect("group");
            activate_control(&home);
            GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("state");
            let paths = [
                global_path(&home),
                store
                    .group_dir(&group.group_id)
                    .expect("group dir")
                    .join("group.yaml"),
                store
                    .state_dir(&group.group_id)
                    .expect("state dir")
                    .join("automation.json"),
            ];
            let before = paths
                .iter()
                .map(std::fs::read)
                .collect::<io::Result<Vec<_>>>()
                .expect("snapshots");
            let mut injected = false;
            let mut fault = |path: &std::path::Path, stage: PublishStage| {
                if path == paths[target] && stage == PublishStage::DirectorySync {
                    injected = true;
                    return Err(io::Error::other("labeled prerequisite sync failure"));
                }
                Ok(())
            };
            assert!(attempt_recovery(&home, true, &mut fault).is_err());
            assert!(injected);
            for (path, expected) in paths.iter().zip(&before) {
                assert_eq!(&std::fs::read(path).expect("visible bytes"), expected);
            }
            assert!(home_state(&home, &mut homes().lock().expect("homes")).is_err());
            assert!(recover_on_start(&home).is_err());
            recover_existing(&home).expect("explicit strict recovery now");
        }
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_global_group_join_cannot_recover_readiness() {
        let source: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("partial-join-control", "").expect("group");
        activate_control(&home);
        let mut state =
            GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("state");
        let pending = Pending {
            source_event_id: source.id.clone(),
            source_ts: source.ts,
            generation: "generation-control".into(),
            policy_fingerprint: "a".repeat(64),
        };
        let attempt = budget::Attempt {
            id: "attempt-control".into(),
            group_id: group.group_id,
            actor_id: "receiver".into(),
            generation: pending.generation.clone(),
            policy_fingerprint: pending.policy_fingerprint.clone(),
            source_event_id: source.id,
        };
        state.note(&attempt.actor_id, pending).expect("nominate");
        let prepared = state
            .prepare(&attempt, Instant::now())
            .expect("prepare")
            .expect("admit");
        let mut budget = Budget::load(&global_path(&home), Instant::now()).expect("limiter");
        assert!(
            budget
                .reserve(attempt.clone(), Instant::now())
                .expect("reserve")
        );
        // Labeled interruption after the global debit, before the group debit.
        assert!(recover_existing(&home).is_err());
        assert_eq!(
            Budget::load(&global_path(&home), Instant::now())
                .expect("reload")
                .in_flight(),
            Some(attempt.clone())
        );
        state.commit(prepared).expect("complete matching control");
        recover_existing(&home).expect("strict recovery now");
        let mut ready = homes().lock().expect("homes");
        assert_eq!(
            home_state(&home, &mut ready)
                .expect("ready")
                .global
                .in_flight(),
            Some(attempt)
        );
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_preparing_and_caller_assertions_do_not_mint_allowance() {
        use cccc_core::registry::{MailWakeActivation, MailWakePhase};
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let fresh = home
            .try_create_fresh()
            .expect("exclusive create")
            .expect("fresh");
        home.initialize().expect("home setup");
        cccc_core::Registry::load(&home).expect("registry setup");
        let header = MailWakeActivation {
            version: ACTIVATION_SCHEMA,
            activation_id: "activation-control".into(),
            proof_digest: "a".repeat(64),
            implementation: implementation_digest().expect("implementation"),
            home_identity: fresh.verify_identity(&home).expect("identity"),
            phase: MailWakePhase::Preparing,
        };
        cccc_core::Registry::mutate_existing(&home, |registry| {
            registry.mail_wake_activation = Some(header.clone());
            Ok(())
        })
        .expect("labeled interruption after PREPARING");
        bootstrap(&home).expect("reopen");
        assert!(active_header(&home).is_err());
        assert!(!global_path(&home).exists());
        Budget::create_for_activation(&global_path(&home), &header.activation_id)
            .expect("limiter control");
        bootstrap(&home).expect("reopen");
        assert!(active_header(&home).is_err());
        let request = DaemonRequest {
            v: 1,
            op: "mail_wake_initialize".into(),
            args: json!({"by":"user", "reconciled":true, "event":event.id})
                .as_object()
                .expect("args")
                .clone(),
        };
        assert!(initialize(&home, &request).is_err());
        assert_eq!(
            cccc_core::Registry::load_existing(&home)
                .expect("registry")
                .mail_wake_activation,
            Some(header)
        );
    }

    #[cfg(unix)]
    #[test]
    fn registry_loss_and_auto_recreation_do_not_restore_active_authority() {
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(event.kind, "chat.message");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        bootstrap(&home).expect("bootstrap");
        assert!(active_header(&home).is_ok());
        std::fs::remove_file(home.registry_path()).expect("labeled registry loss");
        assert!(active_header(&home).is_err());
        assert!(cccc_core::Registry::load(&home).is_err());
        assert!(!home.registry_path().exists());
        assert!(active_header(&home).is_err());
        bootstrap(&home).expect("reopen");
        assert!(active_header(&home).is_err());
        assert!(global_path(&home).exists());
    }

    #[cfg(unix)]
    #[test]
    fn restore_loop_does_not_replace_unresolved_wake_owner() {
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let created = store.create("restore-control", "").expect("group");
        let mut group = cccc_core::group_scope::attach(
            &store,
            &created.group_id,
            cccc_core::scope::detect(temp.path()).expect("scope"),
        )
        .expect("attach");
        struct Cleanup(String, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = cccc_runtime::stop(&self.0, &self.1);
            }
        }
        let _cleanup = Cleanup(group.group_id.clone(), "receiver".into());
        group.running = true;
        group.state = GroupState::Active;
        let mut actor = Actor::new("receiver");
        actor.runtime = cccc_contracts::ActorRuntime::Custom;
        actor.command = vec!["sh".into(), "-c".into(), "exec sleep 30".into()];
        actor.generation = "generation-control".into();
        actor.wake_on_mail = true;
        group.actors.push(actor.clone());
        store.save(&group).expect("save");
        activate_control(&home);
        GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
        recover_existing(&home).expect("readiness control");
        let mut budget = Budget::load(&global_path(&home), Instant::now()).expect("limiter");
        let attempt = budget::Attempt {
            id: "attempt-control".into(),
            group_id: group.group_id.clone(),
            actor_id: actor.id.clone(),
            generation: actors::generation_identity(&actor),
            policy_fingerprint: "a".repeat(64),
            source_event_id: event.id,
        };
        assert!(
            budget
                .reserve(attempt.clone(), Instant::now())
                .expect("debit")
        );
        assert!(restore_blocked(&home, &group.group_id, &actor));
        let other = Actor::new("other");
        assert!(!restore_blocked(&home, &group.group_id, &other));
        super::super::runtime_restore::restore_group(&home, &store, &group.group_id)
            .expect("restore pass");
        assert!(cccc_runtime::status(&group.group_id, &actor.id).is_err());
        assert_eq!(
            Budget::load(&global_path(&home), Instant::now())
                .expect("reload")
                .in_flight(),
            Some(attempt)
        );
    }

    #[test]
    fn captured_mail_requires_literal_other_recipient_and_explicit_opt_in() {
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let mut actor = Actor::new("receiver");
        actor.generation = "control-generation".into();
        assert!(!eligible_actor(&actor));
        assert!(direct_mail_for(&event, &actor));
        actor.wake_on_mail = true;
        assert!(eligible_actor(&actor));
        actor.enabled = false; // labeled manual-Stop control
        assert!(!eligible_actor(&actor));

        let mut changed = event.clone(); // labeled selector and sender mutations
        changed
            .data
            .insert("to".into(), json!(["receiver", "@all"]));
        assert!(!direct_mail_for(&changed, &actor));
        changed = event.clone();
        changed.by = actor.id.clone();
        assert!(!direct_mail_for(&changed, &actor));
        changed = event;
        changed.data.insert("message_mode".into(), json!("send"));
        assert!(!direct_mail_for(&changed, &actor));
    }

    #[test]
    fn legacy_actor_uses_existing_generation_identity_and_unknown_identity_holds() {
        let mut actor = Actor::new("receiver");
        actor.wake_on_mail = true;
        actor.generation.clear(); // labeled legacy-record control
        assert!(eligible_actor(&actor));
        assert_eq!(
            actors::generation_identity(&actor),
            format!("legacy:{}", actor.created_at)
        );
        actor.created_at.clear(); // labeled missing-identity control
        assert!(!eligible_actor(&actor));
    }

    #[cfg(unix)]
    #[test]
    fn created_group_namespace_requires_activation_and_cannot_reset_a_duplicate() {
        let source: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("creation-control", "").expect("group");
        // Before activation there is no implicit first-use state.
        initialize_created_group(&home, &group).expect("inactive");
        assert!(GroupBudget::load(&home, &group.group_id, Instant::now(), false).is_err());
        // Labeled activated-limiter setup; this is not a production migration proof.
        activate_control(&home);
        initialize_created_group(&home, &group).expect("new group");
        let mut state =
            GroupBudget::load(&home, &group.group_id, Instant::now(), false).expect("state");
        let pending = Pending {
            source_event_id: source.id,
            source_ts: source.ts,
            generation: "generation-control".into(),
            policy_fingerprint: "a".repeat(64),
        };
        state.note("receiver", pending).expect("state mutation");
        let path = store
            .state_dir(&group.group_id)
            .expect("state dir")
            .join("automation.json");
        let before = std::fs::read(&path).expect("state bytes");
        assert!(initialize_created_group(&home, &group).is_err());
        assert_eq!(std::fs::read(path).expect("state retained"), before);
    }

    #[cfg(unix)]
    #[test]
    fn failed_wake_initialization_does_not_block_normal_group_creation() {
        let capture: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        assert_eq!(capture.kind, "chat.message");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        bootstrap(&home).expect("fresh home");
        recover_existing(&home).expect("readiness control");
        cccc_core::Registry::mutate_existing(&home, |registry| {
            // Labeled implementation-change control: the old activation no
            // longer authorizes the current binary to initialize a namespace.
            registry
                .mail_wake_activation
                .as_mut()
                .expect("header")
                .implementation = "0".repeat(64);
            Ok(())
        })
        .expect("mutate header");
        let request = DaemonRequest {
            v: 1,
            op: "group_create".into(),
            args: json!({"title":"creation-control", "by":"user"})
                .as_object()
                .expect("args")
                .clone(),
        };
        let group = super::super::groups::resolve_operation(&request)
            .expect("operation")
            .execute(&home, &request)
            .expect("ordinary creation");
        let group_id = group["group_id"].as_str().expect("group id");
        assert!(GroupBudget::load(&home, group_id, Instant::now(), false).is_err());
        assert!(home_state(&home, &mut homes().lock().expect("homes")).is_err());
        assert!(recover_existing(&home).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn captured_manual_stop_and_start_preserve_the_enabled_gate() {
        let lifecycle: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("fixtures/lifecycle.json"))
                .expect("captured lifecycle projection");
        assert_eq!(lifecycle[0]["kind"], "actor.stop");
        assert_eq!(lifecycle[1]["kind"], "actor.start");
        assert_eq!(lifecycle[0]["data"]["runner"], "pty");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let created = store.create("lifecycle-control", "").expect("group");
        let mut group = cccc_core::group_scope::attach(
            &store,
            &created.group_id,
            cccc_core::scope::detect(temp.path()).expect("scope"),
        )
        .expect("attach");
        struct Cleanup(String, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = cccc_runtime::stop(&self.0, &self.1);
            }
        }
        let _cleanup = Cleanup(group.group_id.clone(), "receiver".into());
        let mut actor = Actor::new("receiver");
        actor.runtime = cccc_contracts::ActorRuntime::Custom;
        actor.command = vec!["sh".into(), "-c".into(), "exec sleep 30".into()]; // labeled local launcher
        actor.generation = "generation-control".into();
        actor.wake_on_mail = true;
        group.actors.push(actor);
        group.running = true;
        group.state = GroupState::Active;
        store.save(&group).expect("save");
        activate_control(&home);
        GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
        recover_existing(&home).expect("readiness control");
        let request = DaemonRequest {
            v: 1,
            op: "actor_stop".into(),
            args: json!({"group_id":group.group_id,"actor_id":"receiver","by":"user"})
                .as_object()
                .expect("args")
                .clone(),
        };
        super::super::actors::lifecycle(
            &home,
            &request,
            lifecycle[0]["kind"].as_str().expect("kind"),
        )
        .expect("stop");
        let stopped = store.load(&group.group_id).expect("group");
        assert!(!stopped.actors[0].enabled);
        assert!(!eligible_actor(&stopped.actors[0]));
        super::super::actors::lifecycle(
            &home,
            &request,
            lifecycle[1]["kind"].as_str().expect("kind"),
        )
        .expect("start");
        let started = store.load(&group.group_id).expect("group");
        assert!(started.actors[0].enabled);
        assert!(eligible_actor(&started.actors[0]));
    }

    #[cfg(unix)]
    #[test]
    fn captured_mail_runs_the_worker_once_and_records_owned_start() {
        let mut event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let created = store.create("wake-control", "").expect("group");
        let mut group = cccc_core::group_scope::attach(
            &store,
            &created.group_id,
            cccc_core::scope::detect(temp.path()).expect("scope"),
        )
        .expect("attach");
        struct Cleanup(String, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = cccc_runtime::stop(&self.0, &self.1);
            }
        }
        let _cleanup = Cleanup(group.group_id.clone(), "receiver".into());
        let mut actor = Actor::new("receiver");
        actor.runtime = cccc_contracts::ActorRuntime::Custom;
        actor.command = vec!["sh".into(), "-c".into(), "exec sleep 30".into()]; // labeled local launcher
        actor.generation = "generation-control".into();
        actor.wake_on_mail = true;
        group.actors.push(actor);
        group.running = true;
        group.state = GroupState::Active;
        // Labeled policy control: the wake-specific cue must work even when
        // ordinary timed Mail notices are disabled.
        group.extra.insert(
            "delivery".into(),
            json!({"mail_notice_after_seconds": 0, "mail_notice_idle_after_seconds": 0}),
        );
        store.save(&group).expect("save");
        let group = store.load(&group.group_id).expect("group");
        activate_control(&home);
        GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
        recover_existing(&home).expect("readiness control");
        event.group_id = group.group_id.clone(); // labeled routing mutation
        let ledger = store.ledger_path(&group.group_id).expect("ledger");
        cccc_core::ledger::append(&ledger, &event).expect("capture");
        note_committed(&home, &group, &event).expect("nomination");
        let locks = crate::dispatch_concurrency::DispatchLocks::default();
        run(&home, &locks).expect("worker pass");
        assert!(cccc_runtime::confirmed_running(&group.group_id, "receiver").expect("owner"));
        run(&home, &locks).expect("duplicate pass");
        let starts = cccc_core::ledger::read_all(&ledger)
            .expect("events")
            .into_iter()
            .filter(|item| {
                item.kind == "actor.start" && item.data.get("reason") == Some(&json!("mail_wake"))
            })
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].by, "system");
        assert_eq!(starts[0].data["source_event_id"], event.id);
        let notices = cccc_core::ledger::read_all(&ledger)
            .expect("events")
            .into_iter()
            .filter(|item| {
                item.kind == "system.notify"
                    && item.data.get("kind") == Some(&json!("mail_notice"))
                    && item.data.get("target_actor_id") == Some(&json!("receiver"))
            })
            .count();
        assert_eq!(notices, 1);
        assert!(
            mail_pending::still_awaited(&home, &group, "receiver", &[event.id])
                .expect("unread state")
                .len()
                == 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn captured_mail_worker_respects_opt_out_manual_stop_and_group_state() {
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = cccc_runtime::stop(&self.0, "receiver");
            }
        }
        let source: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        for case in ["opt-out", "manual-stop", "paused", "stopped"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
            let store = GroupStore::new(home.clone()).expect("store");
            let created = store.create("wake-control", "").expect("group");
            let _cleanup = Cleanup(created.group_id.clone());
            let mut group = cccc_core::group_scope::attach(
                &store,
                &created.group_id,
                cccc_core::scope::detect(temp.path()).expect("scope"),
            )
            .expect("attach");
            let mut actor = Actor::new("receiver");
            actor.runtime = cccc_contracts::ActorRuntime::Custom;
            actor.command = vec!["sh".into(), "-c".into(), "exec sleep 30".into()];
            actor.generation = "generation-control".into();
            actor.wake_on_mail = case != "opt-out";
            actor.enabled = case != "manual-stop";
            group.actors.push(actor);
            group.running = true;
            group.state = match case {
                "paused" => GroupState::Paused,
                "stopped" => GroupState::Stopped,
                _ => GroupState::Active,
            };
            store.save(&group).expect("labeled policy control");
            let group = store.load(&group.group_id).expect("group");
            activate_control(&home);
            GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
            recover_existing(&home).expect("readiness control");
            let mut event = source.clone();
            event.group_id = group.group_id.clone(); // labeled routing mutation
            let ledger = store.ledger_path(&group.group_id).expect("ledger");
            cccc_core::ledger::append(&ledger, &event).expect("captured Mail");
            note_committed(&home, &group, &event).expect("callback");
            run(
                &home,
                &crate::dispatch_concurrency::DispatchLocks::default(),
            )
            .expect("worker");
            assert!(
                cccc_runtime::status(&group.group_id, "receiver").is_err(),
                "{case}"
            );
            assert!(
                !cccc_core::ledger::read_all(&ledger)
                    .expect("ledger")
                    .iter()
                    .any(|item| {
                        item.kind == "actor.start"
                            && item.data.get("reason") == Some(&json!("mail_wake"))
                    }),
                "{case}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn captured_mail_client_id_replay_does_not_append_or_nominate_twice() {
        let capture: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("replay-control", "").expect("group");
        let mut actor = Actor::new("receiver");
        actor.generation = "generation-control".into();
        actor.wake_on_mail = true;
        group.actors.push(actor);
        group.running = true;
        group.state = GroupState::Active;
        store.save(&group).expect("group");
        activate_control(&home);
        GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
        recover_existing(&home).expect("readiness control");
        // Source envelope fields are retained; the request text and client ID
        // are labeled coordination controls, not captured message content.
        let request = DaemonRequest { v: 1, op: "send".into(), args: json!({
            "group_id":group.group_id, "by":"user", "text":"control",
            "message_mode":capture.data["message_mode"], "to":capture.data["to"], "client_id":"replay-control"
        }).as_object().expect("args").clone() };
        let first = super::super::messaging::send(&home, &request, "chat.message").expect("send");
        let second =
            super::super::messaging::send(&home, &request, "chat.message").expect("replay");
        assert_eq!(first["event"]["id"], second["event"]["id"]);
        assert_eq!(second["duplicate"], true);
        let events =
            cccc_core::ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger"))
                .expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "chat.message")
                .count(),
            1
        );
        let mut guard = homes().lock().expect("homes");
        let state = home_state(&home, &mut guard).expect("state");
        let pending = group_budget(&home, state, &group.group_id)
            .expect("budget")
            .pending("receiver")
            .cloned()
            .expect("nomination");
        assert_eq!(
            Some(pending.source_event_id.as_str()),
            first["event"]["id"].as_str()
        );
    }

    #[test]
    fn captured_exit_and_notice_do_not_nominate_a_wake() {
        let exit: Event = serde_json::from_str(include_str!("fixtures/exit.json"))
            .expect("captured process exit");
        let notice: Event = serde_json::from_str(include_str!("fixtures/notice.json"))
            .expect("captured Mail notice");
        let actor = Actor::new("receiver");
        assert_eq!(exit.data.get("reason"), Some(&json!("process_exit")));
        assert_eq!(notice.data.get("kind"), Some(&json!("mail_notice")));
        assert!(!direct_mail_for(&exit, &actor));
        assert!(!direct_mail_for(&notice, &actor));
    }

    #[test]
    fn captured_mail_census_orders_across_groups_and_ties() {
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let time = chrono::DateTime::parse_from_rfc3339(&event.ts).expect("timestamp");
        // Labeled cross-group and earlier-time coordination controls around
        // the captured Mail envelope; they are not separate observed events.
        let candidate = |group: &str, actor: &str, source: &str, source_time| Candidate {
            group_id: group.into(),
            actor_id: actor.into(),
            generation: "generation-control".into(),
            source_id: source.into(),
            source_time,
            fingerprint: "a".repeat(64),
        };
        let mut candidates = vec![
            candidate("group-b", "receiver", &event.id, time),
            candidate("group-a", "receiver", &event.id, time),
            candidate(
                "group-z",
                "receiver",
                &event.id,
                time - chrono::Duration::seconds(1),
            ),
        ];
        order_candidates(&mut candidates);
        assert_eq!(
            candidates
                .iter()
                .map(|item| item.group_id.as_str())
                .collect::<Vec<_>>(),
            ["group-z", "group-a", "group-b"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_group_aborts_full_census_without_spending_a_debit() {
        let mut event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = GroupStore::new(home.clone()).expect("store");
        let good = store.create("census-control", "").expect("group");
        let bad = store.create("bad-group-control", "").expect("group");
        // The captured envelope is bound to the disposable group; the invalid
        // YAML is a labeled partial-census failure control.
        event.group_id = good.group_id.clone();
        cccc_core::ledger::append(&store.ledger_path(&good.group_id).expect("ledger"), &event)
            .expect("append source");
        activate_control(&home);
        GroupBudget::load(&home, &good.group_id, Instant::now(), true).expect("good setup");
        GroupBudget::load(&home, &bad.group_id, Instant::now(), true).expect("bad setup");
        recover_existing(&home).expect("readiness control");
        std::fs::write(
            store
                .group_dir(&bad.group_id)
                .expect("dir")
                .join("group.yaml"),
            b"[",
        )
        .expect("corrupt group control");
        assert!(
            census(
                &home,
                &crate::dispatch_concurrency::DispatchLocks::default()
            )
            .is_err()
        );
        assert!(
            Budget::load(&global_path(&home), Instant::now())
                .expect("budget")
                .in_flight()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn inactive_group_with_missing_namespace_aborts_complete_census() {
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("missing-control", "").expect("group");
        assert!(!eligible_group(&group));
        // Labeled activated limiter without the group's namespace.  The Mail
        // source remains the retained capture; no candidate is dispatched.
        assert_eq!(event.kind, "chat.message");
        activate_control(&home);
        GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
        recover_existing(&home).expect("readiness control");
        let path = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("automation.json");
        cccc_core::fs::write_json(&path, &json!({})).expect("labeled namespace loss");
        let locks = crate::dispatch_concurrency::DispatchLocks::default();
        assert!(census(&home, &locks).is_err());
        assert!(
            Budget::load(&global_path(&home), Instant::now())
                .expect("limiter")
                .in_flight()
                .is_none()
        );
        assert!(GroupBudget::load(&home, &group.group_id, Instant::now(), false).is_err());
    }

    #[test]
    fn deepseek_persisted_origin_is_bound_and_operator_start_clears_it() {
        use cccc_core::deepseek_restart_gate as gate;

        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let group = GroupStore::new(home.clone())
            .expect("store")
            .create("owner-control", "")
            .expect("group");
        // Labeled owner-incarnation controls around the captured Mail source.
        let origin = gate::WakeOrigin {
            attempt_id: "attempt-one".into(),
            actor_generation: "actor-generation".into(),
            policy_fingerprint: "a".repeat(64),
            source_event_id: event.id,
        };
        gate::record_running_generation_with_wake(
            &home,
            &group.group_id,
            "receiver",
            "created",
            "owner-one",
            Some(&origin),
        )
        .expect("record");
        assert!(
            gate::wake_origin_matches(
                &home,
                &group.group_id,
                "receiver",
                "created",
                "owner-one",
                &origin,
            )
            .expect("read")
        );
        let mut changed = origin.clone();
        changed.attempt_id = "attempt-two".into();
        assert!(
            !gate::wake_origin_matches(
                &home,
                &group.group_id,
                "receiver",
                "created",
                "owner-one",
                &changed,
            )
            .expect("read")
        );
        gate::record_running_generation(&home, &group.group_id, "receiver", "created", "owner-two")
            .expect("operator control");
        assert!(
            !gate::wake_origin_matches(
                &home,
                &group.group_id,
                "receiver",
                "created",
                "owner-two",
                &origin,
            )
            .expect("read")
        );
        assert!(
            !gate::wake_origin_matches(
                &home,
                &group.group_id,
                "receiver",
                "created",
                "owner-one",
                &origin,
            )
            .expect("read")
        );
    }

    #[cfg(unix)]
    #[test]
    fn pty_wake_binds_actual_session_and_not_operator_replacement() {
        struct Cleanup(String, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = cccc_runtime::stop(&self.0, &self.1);
            }
        }
        let event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = GroupStore::new(home.clone()).expect("store");
        let created = store.create("pty-owner-control", "").expect("group");
        let mut group = cccc_core::group_scope::attach(
            &store,
            &created.group_id,
            cccc_core::scope::detect(temp.path()).expect("scope"),
        )
        .expect("attach");
        let mut actor = Actor::new("receiver");
        actor.runtime = cccc_contracts::ActorRuntime::Custom;
        actor.generation = "generation-control".into();
        // Labeled local launcher control, not a provider trace.
        actor.command = vec!["sh".into(), "-c".into(), "exec sleep 30".into()];
        group.actors.push(actor.clone());
        store.save(&group).expect("save");
        let _cleanup = Cleanup(group.group_id.clone(), actor.id.clone());
        let origin = cccc_core::wake_origin::WakeOrigin {
            attempt_id: "attempt-one".into(),
            actor_generation: actors::generation_identity(&actor),
            policy_fingerprint: "a".repeat(64),
            source_event_id: event.id,
        };
        super::super::actor_runtime::start_pty_wake(&home, &group, &actor, &origin)
            .expect("launch");
        let confirmed = |wanted: &cccc_core::wake_origin::WakeOrigin| {
            super::super::actor_runtime::with_confirmed_pty_wake(
                &home,
                &group,
                &actor,
                wanted,
                || Ok(()),
            )
            .expect("owner observation")
            .is_some()
        };
        assert!(confirmed(&origin));
        let failed: io::Result<Option<()>> = super::super::actor_runtime::with_confirmed_pty_wake(
            &home,
            &group,
            &actor,
            &origin,
            || Err(io::Error::other("result write failed")),
        );
        assert!(failed.is_err());
        assert!(confirmed(&origin));
        let owner_path = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("pty/owners/receiver.json");
        let saved = std::fs::read(&owner_path).expect("owner record");
        std::fs::write(&owner_path, b"{").expect("corrupt owner control");
        assert!(
            super::super::actor_runtime::with_confirmed_pty_wake(
                &home,
                &group,
                &actor,
                &origin,
                || Ok(()),
            )
            .is_err()
        );
        std::fs::write(&owner_path, saved).expect("restore owner control");
        assert!(confirmed(&origin));
        let mut other = origin.clone();
        other.attempt_id = "attempt-two".into();
        assert!(!confirmed(&other));
        assert!(
            super::super::actor_runtime::start_pty_wake(&home, &group, &actor, &other).is_err()
        );
        assert!(confirmed(&origin));
        cccc_runtime::stop(&group.group_id, &actor.id).expect("stop wake owner");
        // Labeled absent-registry control: the durable record alone cannot
        // certify an owner after a daemon restart.
        assert!(!confirmed(&origin));
        super::super::actor_runtime::apply_resolved(&home, &group, &actor, "actor.start")
            .expect("operator replacement");
        assert!(!confirmed(&origin));
    }

    #[cfg(unix)]
    #[test]
    fn pty_reconcile_joins_real_owner_after_missing_group_result() {
        struct Cleanup(String, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = cccc_runtime::stop(&self.0, &self.1);
            }
        }
        let mut event: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = GroupStore::new(home.clone()).expect("store");
        let created = store.create("pty-recovery-control", "").expect("group");
        let mut group = cccc_core::group_scope::attach(
            &store,
            &created.group_id,
            cccc_core::scope::detect(temp.path()).expect("scope"),
        )
        .expect("attach");
        group.running = true;
        group.state = GroupState::Active;
        let mut actor = Actor::new("receiver");
        actor.runtime = cccc_contracts::ActorRuntime::Custom;
        actor.generation = "generation-control".into();
        actor.wake_on_mail = true;
        actor.command = vec!["sh".into(), "-c".into(), "exec sleep 30".into()];
        group.actors.push(actor.clone());
        store.save(&group).expect("save");
        let group = store.load(&group.group_id).expect("reload");
        let actor = group.actors[0].clone();
        let _cleanup = Cleanup(group.group_id.clone(), actor.id.clone());
        // Labeled group-id binding of the captured Mail; the launcher below is
        // a local coordination control, not a provider trace.
        event.group_id = group.group_id.clone();
        cccc_core::ledger::append(&store.ledger_path(&group.group_id).expect("ledger"), &event)
            .expect("append source");
        activate_control(&home);
        // Labeled first-use setup; product activation is separately held.
        GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
        recover_existing(&home).expect("readiness control");
        let fingerprint = policy_fingerprint(&home, &group, &actor).expect("fingerprint");
        // Labeled private-environment mutation; the value never enters the
        // persisted wake record, and restoring it restores this process's hash.
        super::super::actor_secrets::replace(
            &home,
            &group.group_id,
            &actor.id,
            std::collections::BTreeMap::from([("CONTROL".into(), "changed".into())]),
        )
        .expect("private environment control");
        assert_ne!(
            policy_fingerprint(&home, &group, &actor).expect("changed"),
            fingerprint
        );
        super::super::actor_secrets::replace(
            &home,
            &group.group_id,
            &actor.id,
            std::collections::BTreeMap::new(),
        )
        .expect("restore environment control");
        assert_eq!(
            policy_fingerprint(&home, &group, &actor).expect("restored"),
            fingerprint
        );
        let attempt = budget::Attempt {
            id: uuid::Uuid::new_v4().simple().to_string(),
            group_id: group.group_id.clone(),
            actor_id: actor.id.clone(),
            generation: actors::generation_identity(&actor),
            policy_fingerprint: fingerprint.clone(),
            source_event_id: event.id.clone(),
        };
        let now = Instant::now();
        {
            let mut guard = homes().lock().expect("homes");
            let state = home_state(&home, &mut guard).expect("state");
            let budget = group_budget(&home, state, &group.group_id).expect("group budget");
            budget
                .note(
                    &actor.id,
                    Pending {
                        source_event_id: event.id.clone(),
                        source_ts: event.ts.clone(),
                        generation: actors::generation_identity(&actor),
                        policy_fingerprint: fingerprint,
                    },
                )
                .expect("nominate");
            let prepared = budget
                .prepare(&attempt, now)
                .expect("prepare")
                .expect("allowed");
            assert!(
                state
                    .global
                    .reserve(attempt.clone(), now)
                    .expect("global debit")
            );
            group_budget(&home, state, &group.group_id)
                .expect("budget")
                .commit(prepared)
                .expect("group debit");
        }
        super::super::actor_runtime::start_pty_wake(&home, &group, &actor, &wake_origin(&attempt))
            .expect("launch");
        // Labeled loss of the post-launch result: rejoin the actual owner
        // without another spawn, then verify the ordered records are settled.
        reconcile(
            &home,
            &crate::dispatch_concurrency::DispatchLocks::default(),
        )
        .expect("reconcile");
        let mut guard = homes().lock().expect("homes");
        let state = home_state(&home, &mut guard).expect("state");
        assert!(state.global.in_flight().is_none());
        assert!(
            group_budget(&home, state, &group.group_id)
                .expect("budget")
                .settled_matches(&attempt)
        );
        assert!(
            cccc_core::ledger::find_event(
                &store.ledger_path(&group.group_id).expect("ledger"),
                &attempt.id
            )
            .expect("read start")
            .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_callbacks_and_censuses_share_a_cross_group_launch_budget() {
        struct Cleanup(Vec<String>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for group in &self.0 {
                    let _ = cccc_runtime::stop(group, "receiver");
                }
            }
        }
        let source: Event = serde_json::from_str(include_str!("fixtures/mail.json"))
            .expect("captured Mail envelope");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut inputs = Vec::new();
        for index in 0..3 {
            let created = store.create("budget-control", "").expect("group");
            let mut group = cccc_core::group_scope::attach(
                &store,
                &created.group_id,
                cccc_core::scope::detect(temp.path()).expect("scope"),
            )
            .expect("attach");
            group.running = true;
            group.state = GroupState::Active;
            let mut actor = Actor::new("receiver");
            actor.runtime = cccc_contracts::ActorRuntime::Custom;
            actor.generation = format!("generation-control-{index}");
            actor.wake_on_mail = true;
            // Local controlled launcher; the Mail below is retained capture.
            actor.command = vec!["sh".into(), "-c".into(), "exec sleep 30".into()];
            group.actors.push(actor);
            store.save(&group).expect("save");
            let group = store.load(&group.group_id).expect("reload");
            let mut event = source.clone();
            // Labeled group and identity mutations create independent targets.
            event.group_id = group.group_id.clone();
            event.id = uuid::Uuid::new_v4().simple().to_string();
            cccc_core::ledger::append(&store.ledger_path(&group.group_id).expect("ledger"), &event)
                .expect("append source");
            inputs.push((group, event));
        }
        let _cleanup = Cleanup(
            inputs
                .iter()
                .map(|(group, _)| group.group_id.clone())
                .collect(),
        );
        activate_control(&home);
        // Labeled first-use setup; product activation is separately held.
        for (group, _) in &inputs {
            GroupBudget::load(&home, &group.group_id, Instant::now(), true).expect("group setup");
        }
        recover_existing(&home).expect("readiness control");
        let locks = crate::dispatch_concurrency::DispatchLocks::default();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        std::thread::scope(|scope| {
            let handles = (0..4)
                .map(|index| {
                    let (group, event) = &inputs[index % inputs.len()];
                    let barrier = std::sync::Arc::clone(&barrier);
                    let locks = locks.clone();
                    let home = home.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        locks.with_group_write_blocking(&group.group_id, || {
                            note_committed(&home, group, event)
                        })
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle.join().expect("callback thread").expect("callback");
            }
        });
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let snapshots = std::thread::scope(|scope| {
            let handles = (0..4)
                .map(|_| {
                    let barrier = std::sync::Arc::clone(&barrier);
                    let locks = locks.clone();
                    let home = home.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        census(&home, &locks)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("census thread").expect("census"))
                .collect::<Vec<_>>()
        });
        let expected = snapshots[0]
            .iter()
            .map(|item| item.group_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(expected.len(), 3);
        assert!(snapshots.iter().all(|items| {
            items
                .iter()
                .map(|item| item.group_id.clone())
                .collect::<Vec<_>>()
                == expected
        }));
        let mut started = 0;
        let mut held = 0;
        for candidate in &snapshots[0] {
            match locks
                .with_group_write_blocking(&candidate.group_id, || admit(&home, candidate))
                .expect("admission")
            {
                Admission::Started => started += 1,
                Admission::Held => held += 1,
                Admission::Stale => panic!("fresh candidate became stale"),
            }
        }
        assert_eq!((started, held), (2, 1));
    }
}
