//! Durable side effects of renaming a stopped member. The group rename intent
//! fences actor operations until all moves have completed.

use cccc_core::context::ContextStore;
use cccc_core::{GroupDoc, GroupStore, HomeLayout, fs, inbox};
use serde_json::{Value, json};
use std::fs::{self as std_fs, FileType};
use std::io;
use std::path::{Path, PathBuf};

use super::{actor_secrets, runtime_session};

pub(super) fn prepare(home: &HomeLayout, group: &GroupDoc, old: &str, new: &str) -> io::Result<()> {
    let group_id = &group.group_id;
    ContextStore::new(home.clone())?.validate_actor_rename(group_id, old, new)?;
    inbox::validate_actor_cursor_rename(home, group_id, new)?;
    let paths = state_paths(home, group_id, old, new)?;
    for file in &paths {
        file.validate()?;
    }
    let _ = read_primary_receipt(&paths[0].source)?;
    actor_secrets::with_actor_rename_paths(home, group_id, old, new, |source, target| {
        Move::new(source, target, false).validate()
    })
}

/// Complete an already-recorded intent. A missing source and a present
/// destination indicate a previously completed move, so a retry can finish
/// remaining work without writing a second copy of any secret.
pub(super) fn complete(home: &HomeLayout, group_id: &str, old: &str, new: &str) -> io::Result<()> {
    let paths = state_paths(home, group_id, old, new)?;
    for file in &paths {
        file.complete()?;
    }
    actor_secrets::with_actor_rename_paths(home, group_id, old, new, |source, target| {
        Move::new(source, target, false).complete()
    })?;
    update_primary_receipt(home, group_id, old, new)?;
    inbox::rename_actor_cursor(home, group_id, old, new)?;
    ContextStore::new(home.clone())?.rename_actor_refs(group_id, old, new)
}

struct Move {
    source: PathBuf,
    target: PathBuf,
    directory: bool,
}

impl Move {
    fn new(source: impl Into<PathBuf>, target: impl Into<PathBuf>, directory: bool) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            directory,
        }
    }

    fn validate(&self) -> io::Result<()> {
        self.exists(&self.source)?;
        if self.exists(&self.target)? {
            return Err(self.conflict());
        }
        Ok(())
    }

    fn complete(&self) -> io::Result<()> {
        if self.exists(&self.source)? {
            if self.exists(&self.target)? {
                return Err(self.conflict());
            }
            std_fs::rename(&self.source, &self.target)?;
        }
        // The move is within one directory. Flush its entries before the
        // group publishes the new member, including when retrying a sync.
        #[cfg(unix)]
        if self.exists(&self.target)?
            && let Some(parent) = self.target.parent()
        {
            std_fs::File::open(parent)?.sync_all()?;
        }
        #[cfg(not(unix))]
        self.exists(&self.target)?;
        Ok(())
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        let expected = if self.directory {
            FileType::is_dir
        } else {
            FileType::is_file
        };
        match std_fs::symlink_metadata(path) {
            Ok(metadata) if expected(&metadata.file_type()) => Ok(true),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "actor rename state entry has the wrong file type",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn conflict(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "destination actor has retained state at {}",
                self.target.display()
            ),
        )
    }
}

fn state_paths(home: &HomeLayout, group_id: &str, old: &str, new: &str) -> io::Result<Vec<Move>> {
    let state = GroupStore::new(home.clone())?.state_dir(group_id)?;
    let mut moved = runtime_session::actor_rename_receipt_paths(home, group_id, old)?
        .into_iter()
        .zip(runtime_session::actor_rename_receipt_paths(
            home, group_id, new,
        )?)
        .map(|(source, target)| Move::new(source, target, false))
        .collect::<Vec<_>>();
    for directory in ["terminal", "deepseek", "runtime_sessions/pi"] {
        let directory = state.join(directory);
        moved.push(Move::new(directory.join(old), directory.join(new), true));
    }
    let legacy_headless = state.join("runners/headless");
    moved.push(Move::new(
        legacy_headless.join(format!("{old}.json")),
        legacy_headless.join(format!("{new}.json")),
        false,
    ));
    Ok(moved)
}

fn update_primary_receipt(home: &HomeLayout, group: &str, old: &str, new: &str) -> io::Result<()> {
    let state = GroupStore::new(home.clone())?.state_dir(group)?;
    let receipts = state.join("runtime_sessions");
    let path = receipts.join(format!("{new}.json"));
    let Some(mut document) = read_primary_receipt(&path)? else {
        return Ok(());
    };
    let mut changed = false;
    if document["actor_id"].as_str() == Some(old) {
        document["actor_id"] = json!(new);
        changed = true;
    }
    if document["runtime"] == "pi"
        && document["transport"] == "pi_rpc"
        && let Some(file) = document["session_file"].as_str().map(Path::new)
    {
        // Pi requires its receipt's recorded file to belong to its actor's
        // sessions directory. Rebase that exact path after moving the tree.
        let pi = match std_fs::canonicalize(receipts.join("pi")) {
            Ok(path) => Some(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let old_sessions = pi.as_ref().map(|path| path.join(old).join("sessions"));
        if let (Some(pi), Some(name)) = (pi, file.file_name())
            && file.parent() == old_sessions.as_deref()
        {
            let new_file = pi.join(new).join("sessions").join(name);
            document["session_file"] = json!(new_file);
            changed = true;
        }
    }
    if changed {
        fs::write_json_committed(&path, &document)?;
    }
    Ok(())
}

fn read_primary_receipt(path: &Path) -> io::Result<Option<Value>> {
    match fs::read_json::<Value>(path) {
        Ok(document) if document.is_object() => Ok(Some(document)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime session receipt is not an object",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_contracts::ActorRuntime;
    use std::collections::BTreeMap;

    #[test]
    fn actor_rename_moves_cursor_task_secrets_and_provider_receipts_idempotently() {
        let temp = tempfile::tempdir().expect("rename temp");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("rename home");
        let store = GroupStore::new(home.clone()).expect("rename store");
        let mut group = store.create("Rename state", "").expect("rename group");
        let group_id = &group.group_id;
        let contexts = ContextStore::new(home.clone()).expect("context");
        contexts
            .sync(
                group_id,
                &[json!({
                    "op":"task.create",
                    "title":"Retained task",
                    "assignee":"old-actor",
                })
                .as_object()
                .expect("operation")
                .clone()],
                None,
                "user",
                false,
            )
            .expect("task");
        let state = store.state_dir(group_id).expect("state directory");
        let cursor = json!({"event_id":"unread-boundary","ts":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:01Z"});
        fs::write_json(
            &state.join("read_cursors.json"),
            &json!({"schema":1,"cursors":{"old-actor":cursor}}),
        )
        .expect("mail cursor");
        let old_history = state.join("terminal/old-actor");
        std_fs::create_dir_all(&old_history).expect("terminal directory");
        std_fs::write(old_history.join("record.pty"), b"retained transcript").expect("transcript");
        let private = BTreeMap::from([("FIXTURE_VALUE".into(), "kept privately".into())]);
        actor_secrets::replace(&home, group_id, "old-actor", private.clone()).expect("private env");
        let old_receipt = state.join("runtime_sessions/old-actor.json");
        fs::write_json(
            &old_receipt,
            &json!({"actor_id":"old-actor","provider_session_id":"primary-fixture"}),
        )
        .expect("primary receipt");

        let command = vec!["devin".into(), "acp".into()];
        let mut environment = BTreeMap::from([("CCCC_ACTOR_ID".into(), "old-actor".into())]);
        let old_acp = runtime_session::native_acp::receipt_path(
            &home,
            group_id,
            "old-actor",
            ActorRuntime::Devin,
        )
        .expect("old ACP path");
        let (identity, parts) =
            runtime_session::native_acp::launch(temp.path(), &command, &environment);
        runtime_session::native_acp::record(&old_acp, &identity, &parts, "retained-session", true)
            .expect("old ACP receipt");

        prepare(&home, &group, "old-actor", "renamed").expect("preflight");
        for _ in 0..2 {
            complete(&home, group_id, "old-actor", "renamed").expect("complete state move");
        }
        group
            .actor_aliases
            .insert("old-actor".into(), "renamed".into());
        store.save(&group).expect("publish rename");

        let document = contexts.load(group_id).expect("retained task");
        assert_eq!(document.tasks[0]["assignee"], "renamed");
        assert_eq!(document.tasks[0]["rev"], 2);
        assert_eq!(
            inbox::cursor_details(&home, group_id, "renamed").expect("retained cursor"),
            (
                "unread-boundary".into(),
                "2026-01-01T00:00:00Z".into(),
                "2026-01-01T00:00:01Z".into()
            )
        );
        assert_eq!(
            actor_secrets::values(&home, group_id, "renamed").expect("private value"),
            private
        );
        assert!(
            actor_secrets::values(&home, group_id, "old-actor")
                .expect("old private values")
                .is_empty()
        );
        assert!(!old_history.exists() && !old_receipt.exists() && !old_acp.exists());
        assert_eq!(
            std_fs::read(state.join("terminal/renamed/record.pty")).expect("retained transcript"),
            b"retained transcript"
        );
        let primary: Value =
            fs::read_json(&state.join("runtime_sessions/renamed.json")).expect("renamed receipt");
        assert_eq!(primary["actor_id"], "renamed");
        assert_eq!(primary["provider_session_id"], "primary-fixture");

        let acp = runtime_session::native_acp::receipt_path(
            &home,
            group_id,
            "renamed",
            ActorRuntime::Devin,
        )
        .expect("renamed ACP receipt");
        environment.insert("CCCC_ACTOR_ID".into(), "renamed".into());
        let candidates =
            runtime_session::renamed_environments(&home, group_id, "renamed", &environment)
                .expect("rename fingerprints");
        assert_eq!(candidates.len(), 1);
        let (identity, parts) =
            runtime_session::native_acp::launch(temp.path(), &command, &candidates[0]);
        assert_eq!(
            runtime_session::native_acp::prepare(&acp, &identity, &parts).expect("provider resume"),
            Some("retained-session".into())
        );
        let mut changed_environment = candidates[0].clone();
        changed_environment.insert("PROVIDER_SETTING".into(), "changed".into());
        let (identity, parts) =
            runtime_session::native_acp::launch(temp.path(), &command, &changed_environment);
        assert_eq!(
            runtime_session::native_acp::prepare(&acp, &identity, &parts).expect("changed launch"),
            None
        );
    }

    #[test]
    fn actor_rename_rebases_pi_history_and_preserves_mcp_locator() {
        let temp = tempfile::tempdir().expect("rename temp");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("rename home");
        let store = GroupStore::new(home.clone()).expect("rename store");
        let mut group = store.create("Rename history", "").expect("rename group");
        let group_id = &group.group_id;
        let state = store.state_dir(group_id).expect("state directory");
        let old_sessions = state.join("runtime_sessions/pi/old-actor/sessions");
        std_fs::create_dir_all(&old_sessions).expect("Pi session directory");
        let file = std_fs::canonicalize(old_sessions)
            .expect("canonical Pi path")
            .join("conversation.jsonl");
        std_fs::write(&file, b"example history").expect("Pi history");
        fs::write_json(
            &state.join("runtime_sessions/old-actor.json"),
            &json!({"runtime":"pi","transport":"pi_rpc","actor_id":"old-actor","session_file":file}),
        )
        .expect("Pi receipt");
        let binding = cccc_core::owned_mcp::binding_file(&home, &group, "old-actor", "creation")
            .expect("old locator");
        fs::write_json(&binding, &json!({})).expect("owned MCP binding");

        prepare(&home, &group, "old-actor", "renamed").expect("preflight");
        complete(&home, group_id, "old-actor", "renamed").expect("complete history move");
        group
            .actor_aliases
            .insert("old-actor".into(), "renamed".into());

        let moved = state.join("runtime_sessions/pi/renamed/sessions/conversation.jsonl");
        assert!(!file.exists());
        assert_eq!(
            std_fs::read(&moved).expect("moved Pi history"),
            b"example history"
        );
        let receipt: Value =
            fs::read_json(&state.join("runtime_sessions/renamed.json")).expect("Pi receipt");
        assert_eq!(
            receipt["session_file"],
            json!(std_fs::canonicalize(moved).expect("canonical renamed Pi path"))
        );
        assert_eq!(
            cccc_core::owned_mcp::binding_file(&home, &group, "renamed", "creation")
                .expect("renamed locator"),
            binding
        );
    }

    #[test]
    fn actor_rename_preflight_refuses_an_active_task() {
        let temp = tempfile::tempdir().expect("rename temp");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("rename home");
        let group = GroupStore::new(home.clone())
            .expect("rename store")
            .create("Rename task", "")
            .expect("rename group");
        let contexts = ContextStore::new(home.clone()).expect("context");
        contexts
            .sync(
                &group.group_id,
                &[json!({
                    "op":"task.create",
                    "title":"Active task",
                    "assignee":"old-actor",
                    "status":"active",
                })
                .as_object()
                .expect("operation")
                .clone()],
                None,
                "user",
                false,
            )
            .expect("active task");
        let refusal = prepare(&home, &group, "old-actor", "renamed").expect_err("active task");
        assert_eq!(refusal.kind(), io::ErrorKind::WouldBlock);
    }
}
