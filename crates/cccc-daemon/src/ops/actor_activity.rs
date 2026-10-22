use cccc_contracts::{Actor, Event};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, ledger};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    state: String,
    runner: String,
}

#[derive(Default)]
pub struct Publisher {
    previous: BTreeMap<String, BTreeMap<String, Fingerprint>>,
    /// The daemon's dispatch locks. Without them (tests) every tick may record
    /// actor health; with them a tick records it only once runtimes are
    /// restored and while no operation holds the Group.
    locks: Option<crate::dispatch_concurrency::DispatchLocks>,
}

impl Publisher {
    pub fn with_locks(locks: crate::dispatch_concurrency::DispatchLocks) -> Self {
        Self {
            previous: BTreeMap::new(),
            locks: Some(locks),
        }
    }

    pub fn tick(&mut self, home: &HomeLayout) -> io::Result<()> {
        let store = GroupStore::new(home.clone())?;
        let groups = store.list()?;
        let known = groups
            .iter()
            .map(|group| group.group_id.clone())
            .collect::<BTreeSet<_>>();
        self.previous.retain(|group_id, _| known.contains(group_id));
        super::actor_health::retain_groups(&known);
        for meta in groups {
            let permit = self
                .locks
                .as_ref()
                .map(|locks| locks.try_group_read(&meta.group_id));
            let Ok(group) = store.load(&meta.group_id) else {
                continue;
            };
            let current = self.publish_group(&store, &group);
            let emit = match &permit {
                None => true,
                Some(permit) => permit.is_some() && super::runtime_restore::settled(),
            };
            super::actor_health::tick_group(&store, &group, &current, emit);
        }
        Ok(())
    }

    /// Publish the Group's activity if it changed. Returns, for each running
    /// Actor, (running, whether its runtime reports its working state).
    fn publish_group(
        &mut self,
        store: &GroupStore,
        group: &GroupDoc,
    ) -> BTreeMap<String, (bool, bool)> {
        let mut payloads = Vec::new();
        let mut snapshot = BTreeMap::new();
        let mut current = BTreeMap::new();
        for actor in &group.actors {
            if let Some(payload) = running_payload(store.home(), group, actor) {
                let runner = text(&payload, "runner_effective", "pty");
                let state = text(&payload, "effective_working_state", "waiting");
                let reason = text(&payload, "effective_working_reason", "");
                current.insert(
                    actor.id.clone(),
                    (true, super::working_state::reports_state(&reason)),
                );
                snapshot.insert(actor.id.clone(), Fingerprint { state, runner });
                payloads.push(Value::Object(payload));
            }
        }
        let previous = self
            .previous
            .get(&group.group_id)
            .cloned()
            .unwrap_or_default();
        if previous == snapshot {
            return current;
        }
        for (actor_id, fingerprint) in &previous {
            if !snapshot.contains_key(actor_id) {
                payloads.push(json!({
                    "id":actor_id,
                    "running":false,
                    "runner_effective":fingerprint.runner,
                    "idle_seconds":null,
                    "effective_working_state":"stopped",
                    "effective_working_reason":"runner_not_running",
                    "effective_working_updated_at":cccc_contracts::utc_now(),
                    "effective_active_task_id":null,
                }));
            }
        }
        let mut event = Event::new("actor.activity", &group.group_id);
        event.by = "system".into();
        event.data = json!({"actors":payloads})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let result = store
            .ledger_path(&group.group_id)
            .and_then(|path| ledger::append(&path, &event));
        match result {
            Ok(()) => {
                // Only committed events advance the publication cursor. A
                // failed append is retried by the next ordinary status tick.
                self.previous.insert(group.group_id.clone(), snapshot);
            }
            Err(error) => tracing::warn!(
                %error,
                group_id = %group.group_id,
                "failed to append actor.activity"
            ),
        }
        current
    }
}

pub(super) fn running_payload(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
) -> Option<Map<String, Value>> {
    let running = super::actor_runtime_status::resolve(group, actor).running;
    if !running {
        return None;
    }
    let mut payload =
        super::working_state::runtime_actor_fields(home, actor, &group.group_id, true);
    payload.insert("id".into(), json!(actor.id));
    payload.insert("running".into(), Value::Bool(true));
    Some(payload)
}

fn text(payload: &Map<String, Value>, key: &str, default: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use cccc_contracts::{Actor, RunnerKind};
    use cccc_core::actors;
    use cccc_runtime::LaunchSpec;
    use std::collections::BTreeMap;

    #[test]
    fn retries_failed_publication_without_repeating_committed_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("activity retry", "").expect("group");
        let mut publisher = Publisher::default();
        publisher.previous.insert(
            group.group_id.clone(),
            BTreeMap::from([(
                "peer".into(),
                Fingerprint {
                    state: "working".into(),
                    runner: "pty".into(),
                },
            )]),
        );
        // A stopped Actor still needs its transition published after a transient
        // storage error. Obstruct only this fixture's ledger writer lock.
        let lock = store
            .group_dir(&group.group_id)
            .expect("group path")
            .join("state/ledger/ledger.lock");
        std::fs::create_dir_all(&lock).expect("obstruct writer");
        publisher
            .tick(&home)
            .expect("failed publication is isolated");
        std::fs::remove_dir(&lock).expect("restore writer");
        publisher.tick(&home).expect("recovery tick");
        publisher.tick(&home).expect("stable tick");

        let events = ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger"))
            .expect("events")
            .into_iter()
            .filter(|event| event.kind == "actor.activity")
            .collect::<Vec<_>>();
        assert_eq!(
            events.len(),
            1,
            "retry must publish the lost transition once"
        );
        assert_eq!(events[0].data["actors"][0]["id"], "peer");
        assert_eq!(events[0].data["actors"][0]["running"], false);
    }

    #[test]
    fn publishes_initial_and_stopped_runtime_snapshots_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("activity", "").expect("group");
        store
            .mutate(&group.group_id, |doc| {
                let mut actor = Actor::new("peer");
                actor.runner = RunnerKind::Pty;
                actors::add(doc, actor)
            })
            .expect("actor");
        cccc_runtime::start(LaunchSpec {
            group_id: group.group_id.clone(),
            actor_id: "peer".into(),
            runner: RunnerKind::Pty,
            command: vec!["sh".into(), "-c".into(), "sleep 5".into()],
            cwd: temp.path().into(),
            env: BTreeMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("runtime");
        let mut publisher = Publisher::default();
        publisher.tick(&home).expect("initial tick");
        publisher.tick(&home).expect("stable tick");
        cccc_runtime::stop(&group.group_id, "peer").expect("stop");
        publisher.tick(&home).expect("stopped tick");

        let events = ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger path"))
            .expect("events")
            .into_iter()
            .filter(|event| event.kind == "actor.activity")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data["actors"][0]["running"], true);
        assert_eq!(
            events[1].data["actors"][0]["effective_working_state"],
            "stopped"
        );
    }
}
