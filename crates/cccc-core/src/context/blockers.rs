//! Which of a task's `blocked_by` entries still hold it back.

use serde_json::{Map, Value};
use std::collections::HashMap;

/// Each task's status by id, taken from one task list, to tell open blockers
/// from resolved ones.
pub struct Blockers {
    status: HashMap<String, String>,
}

impl Blockers {
    pub fn new<'a>(tasks: impl IntoIterator<Item = &'a Map<String, Value>>) -> Self {
        let status = tasks
            .into_iter()
            .filter_map(|task| {
                let id = task.get("id")?.as_str()?;
                Some((id.to_owned(), status(task).to_owned()))
            })
            .collect();
        Self { status }
    }

    /// The entries of `task`'s `blocked_by` that are not done or archived. An
    /// entry that names no task stays open: nothing can complete it.
    pub fn open(&self, task: &Map<String, Value>) -> Vec<String> {
        task.get("blocked_by")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| match entry.as_str() {
                Some(id) => (!matches!(
                    self.status.get(id).map(String::as_str),
                    Some("done" | "archived")
                ))
                .then(|| id.to_owned()),
                None => Some(entry.to_string()),
            })
            .collect()
    }

    /// Tasks in `tasks`, the list after a change, that this (earlier) list saw
    /// held back by blockers that are all resolved now, at least one of them by
    /// entering done. Each comes with the blockers that entered done. Done and
    /// archived tasks are left out.
    pub fn unblocked_in<'t>(
        &self,
        tasks: &'t [Map<String, Value>],
    ) -> Vec<(&'t Map<String, Value>, Vec<String>)> {
        let after = Self::new(tasks);
        tasks
            .iter()
            .filter(|task| !matches!(status(task), "done" | "archived"))
            .filter_map(|task| {
                let held = self.open(task);
                if held.is_empty() || !after.open(task).is_empty() {
                    return None;
                }
                let done = held
                    .into_iter()
                    .filter(|id| after.status.get(id).map(String::as_str) == Some("done"))
                    .collect::<Vec<_>>();
                (!done.is_empty()).then_some((task, done))
            })
            .collect()
    }
}

fn status(task: &Map<String, Value>) -> &str {
    task.get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .unwrap_or("planned")
}
