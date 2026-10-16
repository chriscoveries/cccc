//! Scoped test-only config mutation after the real native admission callback.
//! No provider hook, environment mutation, sleeps or production behavior change.
use cccc_core::HomeLayout;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

type Key = (PathBuf, String, String);
type Hook = Box<dyn FnOnce() -> io::Result<()> + Send>;
fn hooks() -> &'static Mutex<HashMap<Key, Hook>> {
    static HOOKS: OnceLock<Mutex<HashMap<Key, Hook>>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}
pub(crate) struct AdmissionHook {
    key: Key,
}
impl AdmissionHook {
    pub(crate) fn new(home: &HomeLayout, group: &str, actor: &str, hook: Hook) -> Self {
        let key = (
            home.root().to_path_buf(),
            group.to_owned(),
            actor.to_owned(),
        );
        assert!(
            hooks()
                .lock()
                .expect("admission hooks")
                .insert(key.clone(), hook)
                .is_none()
        );
        Self { key }
    }
}
impl Drop for AdmissionHook {
    fn drop(&mut self) {
        hooks().lock().expect("admission hooks").remove(&self.key);
    }
}
pub(super) fn after_native_admission(
    home: &HomeLayout,
    group: &str,
    actor: &str,
) -> io::Result<()> {
    let key = (
        home.root().to_path_buf(),
        group.to_owned(),
        actor.to_owned(),
    );
    let hook = {
        let mut hooks = hooks()
            .lock()
            .map_err(|_| io::Error::other("native admission test hook poisoned"))?;
        hooks.remove(&key)
    };
    // Drop the registry mutex before mutating only this fixture's Group file.
    match hook {
        Some(hook) => hook(),
        None => Ok(()),
    }
}

#[test]
fn standalone_emit_still_resolves_current_role_configuration() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let store = cccc_core::GroupStore::new(home.clone()).expect("store");
    let mut group = store
        .create("standalone memory outcome selection", "")
        .expect("group");
    group.actors = vec![
        cccc_contracts::Actor::new("oldlead"),
        cccc_contracts::Actor::new("newlead"),
    ];
    store.save(&group).expect("initial roles");
    group.actors[0].internal_kind = Some("fixture_hidden".into());
    store
        .save(&group)
        .expect("change configured role before standalone emit");
    let event = super::emit(
        &home,
        &group.group_id,
        "worker",
        1,
        2,
        "restarted",
        "fixture outcome",
    )
    .expect("standalone current role");
    assert_eq!(event.data["target_actor_id"], "newlead");
    let ledger = cccc_core::ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger"))
        .expect("events");
    assert_eq!(ledger.iter().filter(|e| e.id == event.id).count(), 1);
}
