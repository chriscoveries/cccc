use serde_json::{Map, Value};
use std::io;

use super::apply::apply_all;
use super::migration::migrate_legacy_json;
use super::model::{ContextDoc, ContextSyncResult};
use super::yaml_storage::{self, ContextPaths};
use crate::fs::with_exclusive_lock;
use crate::{GroupStore, HomeLayout};

#[derive(Debug, Clone)]
pub struct ContextStore {
    home: HomeLayout,
}

impl ContextStore {
    pub fn new(home: HomeLayout) -> io::Result<Self> {
        home.initialize().map_err(io::Error::other)?;
        Ok(Self { home })
    }

    pub fn load(&self, group_id: &str) -> io::Result<ContextDoc> {
        let paths = self.paths(group_id)?;
        with_exclusive_lock(&paths.lock_file, || {
            migrate_legacy_json(&paths)?;
            yaml_storage::load(&paths)
        })
    }

    pub fn load_overview(&self, group_id: &str) -> io::Result<ContextDoc> {
        let paths = self.paths(group_id)?;
        with_exclusive_lock(&paths.lock_file, || {
            migrate_legacy_json(&paths)?;
            yaml_storage::load_overview(&paths)
        })
    }

    /// Read an already migrated context without initializing or migrating it.
    /// If the lock or migration proof is missing, callers must report unknown.
    pub fn load_selected_read_only(
        group_dir: &std::path::Path,
        ids: &std::collections::BTreeSet<String>,
    ) -> io::Result<Vec<Map<String, Value>>> {
        use fs2::FileExt;
        if ids.len() > 1000 {
            return Err(io::Error::other("too many task ids for a bounded snapshot"));
        }
        let paths = ContextPaths::new(group_dir);
        let lock = std::fs::File::open(&paths.lock_file)?;
        FileExt::lock_shared(&lock)?;
        if paths.legacy_file.try_exists()? {
            let migrated = match paths.migration_file.metadata() {
                Ok(metadata) => metadata.is_file(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => return Err(error),
            };
            if !migrated {
                return Err(io::Error::other(
                    "legacy context is not known to be migrated",
                ));
            }
        }
        let mut tasks = Vec::new();
        for id in ids {
            if !super::yaml_storage::is_canonical_task_id(id) {
                return Err(io::Error::other("invalid task id"));
            }
            let path = paths.tasks_dir.join(format!("{id}.yaml"));
            let metadata = match path.symlink_metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
                return Err(io::Error::other(
                    "task file is not regular or exceeds 1 MiB",
                ));
            }
            let task = super::yaml_files::read_yaml_map(&path)?;
            if task.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(io::Error::other("task file id mismatch"));
            }
            let status = task
                .get("status")
                .and_then(Value::as_str)
                .filter(|s| matches!(*s, "planned" | "active" | "done" | "archived"));
            let waiting = task
                .get("waiting_on")
                .and_then(Value::as_str)
                .filter(|s| matches!(*s, "none" | "user" | "actor" | "external"));
            let blocked = match task.get("blocked_by") {
                None | Some(Value::Null) => Some(false),
                Some(Value::Array(items)) => Some(!items.is_empty()),
                _ => None,
            };
            tasks.push(
                serde_json::json!({"id":id,"status":status,"waiting_on":waiting,"blocked":blocked})
                    .as_object()
                    .expect("object")
                    .clone(),
            );
        }
        Ok(tasks)
    }

    pub fn version(&self, document: &ContextDoc) -> io::Result<String> {
        Ok(format!("ctxv:{}", document.revision))
    }

    pub fn tasks_version(&self, document: &ContextDoc) -> String {
        format!("tasksv:{}", document.tasks_revision)
    }

    pub fn sync(
        &self,
        group_id: &str,
        operations: &[Map<String, Value>],
        if_version: Option<&str>,
        by: &str,
        dry_run: bool,
    ) -> io::Result<ContextSyncResult> {
        self.sync_checked(group_id, operations, if_version, by, dry_run, |_, _| Ok(()))
    }

    /// Authorize each operation against the preceding operations' in-memory
    /// result, under the same storage lock. Nothing is persisted on rejection.
    pub fn sync_checked(
        &self,
        group_id: &str,
        operations: &[Map<String, Value>],
        if_version: Option<&str>,
        by: &str,
        dry_run: bool,
        authorize: impl FnMut(&ContextDoc, &Map<String, Value>) -> io::Result<()>,
    ) -> io::Result<ContextSyncResult> {
        let paths = self.paths(group_id)?;
        with_exclusive_lock(&paths.lock_file, || {
            migrate_legacy_json(&paths)?;
            let before = yaml_storage::load(&paths)?;
            let current_version = self.version(&before)?;
            if if_version.is_some_and(|expected| expected != current_version) {
                return Err(io::Error::other("version_conflict"));
            }
            let mut document = before.clone();
            let changes = apply_all(&mut document, operations, by, authorize).map_err(|error| {
                if error
                    .get_ref()
                    .is_some_and(|cause| cause.is::<super::TaskConflict>())
                {
                    error
                } else {
                    io::Error::new(io::ErrorKind::InvalidInput, error)
                }
            })?;
            yaml_storage::touch_updated_at(&mut document);
            let version = if dry_run {
                current_version
            } else {
                let state = yaml_storage::persist_diff(&paths, &before, &document)?;
                document.revision = state.global_rev;
                document.tasks_revision = state.tasks_rev;
                self.version(&document)?
            };
            Ok(ContextSyncResult {
                context: document,
                version,
                changes,
                dry_run,
            })
        })
    }

    fn paths(&self, group_id: &str) -> io::Result<ContextPaths> {
        let groups = GroupStore::new(self.home.clone())?;
        Ok(ContextPaths::new(&groups.group_dir(group_id)?))
    }
}

#[cfg(test)]
mod captured_read_only_tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn captured_tasks_are_read_without_initialization_or_legacy_migration() {
        let temp = tempfile::tempdir().expect("isolated task capture");
        let paths = ContextPaths::new(temp.path());
        let tasks: Vec<Value> = serde_json::from_str(include_str!("captured_tasks.json"))
            .expect("captured test data and setup");
        std::fs::create_dir_all(&paths.tasks_dir).expect("captured test data and setup");
        std::fs::write(
            paths.tasks_dir.join("T001.yaml"),
            serde_yaml::to_string(&tasks[0]).expect("captured test data and setup"),
        )
        .expect("captured test data and setup");
        let ids = std::collections::BTreeSet::from(["T001".to_string()]);
        assert!(ContextStore::load_selected_read_only(temp.path(), &ids).is_err());
        assert!(!paths.lock_file.exists());
        std::fs::File::create(&paths.lock_file).expect("captured test data and setup");
        std::fs::create_dir_all(
            paths
                .legacy_file
                .parent()
                .expect("captured test data and setup"),
        )
        .expect("captured test data and setup");
        let legacy =
            serde_json::to_vec(&json!({"tasks":tasks})).expect("captured test data and setup");
        std::fs::write(&paths.legacy_file, &legacy).expect("captured test data and setup");
        assert!(ContextStore::load_selected_read_only(temp.path(), &ids).is_err());
        assert!(!paths.migration_file.exists());
        assert_eq!(
            std::fs::read(&paths.legacy_file).expect("captured test data and setup"),
            legacy
        );
        // Marker is an explicit test control: it represents a prior migration.
        std::fs::write(&paths.migration_file, b"{}").expect("captured test data and setup");
        let loaded = ContextStore::load_selected_read_only(temp.path(), &ids)
            .expect("captured test data and setup");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0]["id"], "T001");
        assert_eq!(
            std::fs::read(&paths.legacy_file).expect("captured test data and setup"),
            legacy
        );
    }
}

#[cfg(test)]
mod captured_append_failure_tests {
    use super::*;
    use cccc_contracts::Event;
    use serde_json::{Value, json};

    #[test]
    fn committed_captured_task_can_outlive_a_failed_ledger_append() {
        let temp = tempfile::tempdir().expect("captured test data and setup");
        let home = HomeLayout::from_path(temp.path()).expect("captured test data and setup");
        let groups = GroupStore::new(home.clone()).expect("captured test data and setup");
        let group = groups
            .create("capture", "")
            .expect("captured test data and setup");
        let group_dir = groups
            .group_dir(&group.group_id)
            .expect("captured test data and setup");
        let paths = ContextPaths::new(&group_dir);
        std::fs::create_dir_all(&paths.tasks_dir).expect("captured test data and setup");
        let tasks: Vec<Value> = serde_json::from_str(include_str!("captured_tasks.json"))
            .expect("captured test data and setup");
        std::fs::write(
            paths.tasks_dir.join("T001.yaml"),
            serde_yaml::to_string(&tasks[0]).expect("captured test data and setup"),
        )
        .expect("captured test data and setup");
        let contexts = ContextStore::new(home).expect("captured test data and setup");
        let mutation = json!({"op":"task.move","task_id":"T001","status":"active"})
            .as_object()
            .expect("captured test data and setup")
            .clone();
        let applied = contexts
            .sync(&group.group_id, &[mutation], None, "user", false)
            .expect("captured test data and setup");
        assert_eq!(
            applied.changes[0]["task_transition"]["after"]["status"],
            "active"
        );
        // A directory in place of a ledger is a labeled I/O failure control;
        // it does not represent an observed production append failure.
        let impossible_ledger = group_dir.join("not-a-ledger");
        std::fs::create_dir(&impossible_ledger).expect("captured test data and setup");
        let event = Event::new("context.sync", &group.group_id);
        assert!(crate::ledger::append(&impossible_ledger, &event).is_err());
        let current = contexts
            .load(&group.group_id)
            .expect("captured test data and setup");
        assert_eq!(current.tasks[0]["status"], "active");
    }
}
