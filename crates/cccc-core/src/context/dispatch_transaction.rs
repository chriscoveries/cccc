//! A bounded undo journal joins task-file updates and their ledger batch.
//! The complete, durable ledger suffix is the commit decision. Recovery runs
//! before daemon listeners or runtime restoration, and before context access.
use cccc_contracts::Event;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::dispatch::DispatchState;
use super::model::ContextDoc;
use super::yaml_storage::{self, ContextPaths};
use crate::fs::{atomic_write, read_json, with_exclusive_lock, write_json};
use crate::{GroupStore, HomeLayout, ledger};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BeforeFile {
    relative: String,
    bytes: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u8,
    ledger_offset: u64,
    prefix_tail: Vec<u8>,
    ledger_bytes: Vec<u8>,
    before: Vec<BeforeFile>,
}

fn journal(group: &Path) -> PathBuf {
    group.join("state/task-dispatch-transaction.json")
}

pub(super) fn run(
    home: &HomeLayout,
    group_id: &str,
    change: impl FnOnce(&mut ContextDoc, &mut DispatchState) -> io::Result<Vec<Event>>,
) -> io::Result<Vec<Event>> {
    run_with(home, group_id, change, |file, bytes| {
        file.write_all(bytes)?;
        file.sync_all()
    })
}

/// The last admission check runs after task persistence and immediately before
/// the ledger/outbox commit. A rejection follows the same complete rollback
/// path as a failed append; no assignment or notification becomes visible.
pub(super) fn run_guarded(
    home: &HomeLayout,
    group_id: &str,
    change: impl FnOnce(&mut ContextDoc, &mut DispatchState) -> io::Result<Vec<Event>>,
    admit: impl FnOnce() -> io::Result<()>,
) -> io::Result<Vec<Event>> {
    run_with(home, group_id, change, |file, bytes| {
        admit()?;
        file.write_all(bytes)?;
        file.sync_all()
    })
}

fn run_with(
    home: &HomeLayout,
    group_id: &str,
    change: impl FnOnce(&mut ContextDoc, &mut DispatchState) -> io::Result<Vec<Event>>,
    append: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> io::Result<Vec<Event>> {
    let store = GroupStore::new(home.clone())?;
    let group = store.group_dir(group_id)?;
    let paths = ContextPaths::new(&group);
    with_exclusive_lock(&paths.lock_file, || {
        recover_locked(&group)?;
        super::migration::migrate_legacy_json(&paths)?;
        let before = yaml_storage::load(&paths)?;
        let mut after = before.clone();
        let state_path = group.join("state/task-dispatch.json");
        let mut state = if state_path.try_exists()? {
            read_json(&state_path)?
        } else {
            DispatchState::default()
        };
        let previous = state.clone();
        let events = change(&mut after, &mut state)?;
        if events.is_empty() {
            if before.tasks != after.tasks {
                return Err(io::Error::other("task mutation without dispatch events"));
            }
            if previous != state {
                write_json(&state_path, &state)?;
            }
            return Ok(events);
        }
        let ledger_path = store.ledger_path(group_id)?;
        // Other writers do not retain a ledger lock while acquiring context.
        // The surrounding daemon group write permit also excludes group reads.
        let lock = ledger::acquire_writer_lock(&ledger_path)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&ledger_path)?;
        let offset = file.metadata()?.len();
        let prefix_tail = tail_at(&mut file, offset)?;
        if offset > 0 && prefix_tail.last() != Some(&b'\n') {
            return Err(io::Error::other(
                "dispatch refuses an incomplete ledger tail",
            ));
        }
        let mut bytes = Vec::new();
        for event in &events {
            serde_json::to_writer(&mut bytes, event).map_err(io::Error::other)?;
            bytes.push(b'\n');
        }
        let mut names = vec![
            "context/version_state.json".to_owned(),
            "state/task-dispatch.json".to_owned(),
        ];
        for task in &after.tasks {
            let id = task
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if before
                .tasks
                .iter()
                .find(|task| task.get("id").and_then(serde_json::Value::as_str) == Some(id))
                != Some(task)
            {
                if !super::yaml_files::is_task_id(id) {
                    return Err(io::Error::other("invalid dispatch task id"));
                }
                names.push(format!("context/tasks/{id}.yaml"));
            }
        }
        names.sort();
        names.dedup();
        let snapshots = names
            .into_iter()
            .map(|relative| {
                let path = group.join(&relative);
                if path.is_symlink() {
                    return Err(io::Error::other("dispatch refuses a symlink target"));
                }
                let bytes = match fs::read(path) {
                    Ok(bytes) => Some(bytes),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e),
                };
                Ok(BeforeFile { relative, bytes })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let intent = Intent {
            version: 1,
            ledger_offset: offset,
            prefix_tail,
            ledger_bytes: bytes,
            before: snapshots,
        };
        let intent_path = journal(&group);
        write_json(&intent_path, &intent)?;
        let result = (|| {
            yaml_storage::persist_diff(&paths, &before, &after)?;
            write_json(&state_path, &state)?;
            file.seek(SeekFrom::Start(offset))?;
            append(&mut file, &intent.ledger_bytes)
        })();
        if let Err(error) = result {
            if !suffix_matches(&mut file, &intent)? {
                rollback(&group, &mut file, &intent)?;
                fs::remove_file(&intent_path)?;
                return Err(error);
            }
            // A complete suffix reconciles a write/sync error after commit,
            // just as the ordinary single-event ledger append does.
        }
        let _ = fs::remove_file(&intent_path); // committed; retry cleanup on recovery
        drop(lock);
        ledger::note_batch_append(&ledger_path, &file, &events, offset);
        drop(file);
        Ok(events)
    })
}

/// Caller holds the context lock. A partial transaction is restored before a
/// task reader/writer proceeds; recovery failure is an error, never stale data.
pub(super) fn recover_locked(group: &Path) -> io::Result<()> {
    let path = journal(group);
    if !path.try_exists()? {
        return Ok(());
    }
    let intent: Intent = read_json(&path)?;
    if intent.version != 1 || intent.before.iter().any(|entry| !allowed(&entry.relative)) {
        return Err(io::Error::other("invalid task dispatch journal"));
    }
    let ledger_path = group.join("ledger.jsonl");
    let _lock = ledger::acquire_writer_lock(&ledger_path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&ledger_path)?;
    if !suffix_matches(&mut file, &intent)? {
        rollback(group, &mut file, &intent)?;
    }
    fs::remove_file(path)?;
    crate::ledger_index::invalidate_path(&ledger_path);
    Ok(())
}

fn allowed(relative: &str) -> bool {
    matches!(
        relative,
        "context/version_state.json" | "state/task-dispatch.json"
    ) || relative
        .strip_prefix("context/tasks/")
        .and_then(|p| p.strip_suffix(".yaml"))
        .is_some_and(super::yaml_files::is_task_id)
}

fn tail_at(file: &mut File, offset: u64) -> io::Result<Vec<u8>> {
    let start = offset.saturating_sub(64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (offset - start) as usize];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn suffix_matches(file: &mut File, intent: &Intent) -> io::Result<bool> {
    if file.metadata()?.len() < intent.ledger_offset
        || tail_at(file, intent.ledger_offset)? != intent.prefix_tail
    {
        return Err(io::Error::other(
            "dispatch ledger prefix changed; recovery refused",
        ));
    }
    let end = intent
        .ledger_offset
        .checked_add(intent.ledger_bytes.len() as u64)
        .ok_or_else(|| io::Error::other("invalid dispatch ledger offset"))?;
    if file.metadata()?.len() < end {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(intent.ledger_offset))?;
    let mut bytes = vec![0; intent.ledger_bytes.len()];
    file.read_exact(&mut bytes)?;
    Ok(bytes == intent.ledger_bytes)
}

fn rollback(group: &Path, file: &mut File, intent: &Intent) -> io::Result<()> {
    let len = file.metadata()?.len();
    let end = intent
        .ledger_offset
        .checked_add(intent.ledger_bytes.len() as u64)
        .ok_or_else(|| io::Error::other("invalid dispatch ledger offset"))?;
    if len < intent.ledger_offset || len > end {
        return Err(io::Error::other(
            "unexpected dispatch ledger length; rollback refused",
        ));
    }
    file.seek(SeekFrom::Start(intent.ledger_offset))?;
    let mut appended = Vec::new();
    file.read_to_end(&mut appended)?;
    if !intent.ledger_bytes.starts_with(&appended) {
        return Err(io::Error::other("foreign ledger suffix; rollback refused"));
    }
    file.set_len(intent.ledger_offset)?;
    file.sync_all()?;
    for entry in &intent.before {
        if !allowed(&entry.relative) {
            return Err(io::Error::other("invalid rollback target"));
        }
        let path = group.join(&entry.relative);
        match &entry.bytes {
            Some(bytes) => atomic_write(&path, bytes)?,
            None => match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            },
        }
    }
    Ok(())
}

pub fn recover_all(home: &HomeLayout) -> io::Result<()> {
    home.initialize().map_err(io::Error::other)?;
    for entry in fs::read_dir(home.groups_dir())? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        if journal(&dir).try_exists()? {
            let paths = ContextPaths::new(&dir);
            with_exclusive_lock(&paths.lock_file, || recover_locked(&dir))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextStore;
    use serde_json::{Value, json};

    fn fixture() -> (tempfile::TempDir, HomeLayout, String, String) {
        let capture: Value = serde_json::from_str(include_str!(
            "../../../cccc-mcp/tests/fixtures/captured_context.json"
        ))
        .expect("real task capture");
        let temp = tempfile::tempdir().expect("temp");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("groups");
        let id = store
            .create("dispatch transaction", "")
            .expect("group")
            .group_id;
        let dir = store
            .group_dir(&id)
            .expect("group dir")
            .join("context/tasks");
        fs::create_dir_all(&dir).expect("tasks");
        for task in capture["tasks"].as_array().expect("tasks").iter().take(2) {
            crate::fs::write_yaml(
                &dir.join(format!("{}.yaml", task["id"].as_str().expect("id"))),
                task,
            )
            .expect("seed real task");
        }
        let actor = capture["agent_states"][0]["actor_id"]
            .as_str()
            .expect("actor")
            .to_owned();
        (temp, home, id, actor)
    }
    fn assign(doc: &mut ContextDoc, actor: &str, group: &str) -> io::Result<Vec<Event>> {
        let ids = doc
            .tasks
            .iter()
            .map(|t| t["id"].as_str().expect("id").to_owned())
            .collect::<Vec<_>>();
        let mut events = Vec::new();
        for id in ids {
            let op = json!({"op":"task.update","task_id":id,"assignee":actor});
            super::super::task_apply::update(doc, op.as_object().expect("operation"))?;
            let mut event = Event::new("task.assign", group);
            event.data = json!({"task_id":id,"actor_id":actor})
                .as_object()
                .expect("data")
                .clone();
            events.push(event);
        }
        Ok(events)
    }

    #[test]
    fn startup_ignores_directories_without_a_dispatch_journal() {
        let (_temp, home, _id, _actor) = fixture();
        fs::create_dir(home.groups_dir().join("unregistered-directory")).expect("directory");
        recover_all(&home).expect("no unrelated group parsing");
    }

    #[test]
    fn partial_ledger_io_restores_the_whole_captured_task_batch() {
        let (_temp, home, id, actor) = fixture();
        let contexts = ContextStore::new(home.clone()).expect("context");
        let before = contexts.load(&id).expect("before");
        let result = run_with(
            &home,
            &id,
            |doc, _| assign(doc, &actor, &id),
            |file, bytes| {
                file.write_all(&bytes[..bytes.len() / 2])?;
                Err(io::Error::other("injected interrupted batch"))
            },
        );
        assert!(result.is_err());
        assert_eq!(contexts.load(&id).expect("after rollback"), before);
        let groups = GroupStore::new(home).expect("groups");
        let path = groups.ledger_path(&id).expect("ledger");
        assert_eq!(fs::metadata(path).expect("ledger").len(), 0);
    }

    #[test]
    fn crash_before_commit_recovers_before_any_task_read() {
        let (_temp, home, id, actor) = fixture();
        let contexts = ContextStore::new(home.clone()).expect("context");
        let before = contexts.load(&id).expect("before");
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with(
                &home,
                &id,
                |doc, _| assign(doc, &actor, &id),
                |file, bytes| {
                    file.write_all(&bytes[..bytes.len() / 2])?;
                    panic!("simulated interruption after a partial append")
                },
            )
        }));
        assert!(crashed.is_err());
        assert_eq!(contexts.load(&id).expect("recovered task read"), before);
        let groups = GroupStore::new(home).expect("groups");
        assert!(!journal(&groups.group_dir(&id).expect("group")).exists());
        assert_eq!(
            fs::metadata(groups.ledger_path(&id).expect("ledger"))
                .expect("ledger")
                .len(),
            0
        );
    }

    #[test]
    fn crash_after_complete_suffix_keeps_one_committed_assignment_batch() {
        let (_temp, home, id, actor) = fixture();
        let contexts = ContextStore::new(home.clone()).expect("context");
        let before = contexts.load(&id).expect("before");
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with(
                &home,
                &id,
                |doc, _| assign(doc, &actor, &id),
                |file, bytes| {
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    panic!("simulated interruption after commit")
                },
            )
        }));
        assert!(crashed.is_err());
        recover_all(&home).expect("startup recovery");
        let after = contexts.load(&id).expect("after");
        assert!(after.tasks.iter().all(|t| t["assignee"] == actor));
        assert_eq!(after.tasks.len(), before.tasks.len());
        let groups = GroupStore::new(home).expect("groups");
        let events = ledger::read_all(&groups.ledger_path(&id).expect("ledger")).expect("events");
        assert_eq!(events.len(), 2);
        contexts.load(&id).expect("second read");
        assert_eq!(
            ledger::read_all(&groups.ledger_path(&id).expect("ledger"))
                .expect("events")
                .len(),
            2
        );
    }

    #[test]
    fn complete_append_is_reconciled_when_the_writer_reports_a_late_error() {
        let (_temp, home, id, actor) = fixture();
        let events = run_with(
            &home,
            &id,
            |doc, _| assign(doc, &actor, &id),
            |file, bytes| {
                file.write_all(bytes)?;
                file.sync_all()?;
                Err(io::Error::other("injected late error"))
            },
        )
        .expect("committed suffix is success");
        assert_eq!(events.len(), 2);
        assert!(
            ContextStore::new(home)
                .expect("context")
                .load(&id)
                .expect("after")
                .tasks
                .iter()
                .all(|t| t["assignee"] == actor)
        );
    }
}
