use cccc_contracts::utc_now;
use serde_json::{Map, Value, json};
use std::io;

use super::model::ContextDoc;

pub fn apply_all(
    document: &mut ContextDoc,
    operations: &[Map<String, Value>],
    by: &str,
    mut authorize: impl FnMut(&ContextDoc, &Map<String, Value>) -> io::Result<()>,
) -> io::Result<Vec<Value>> {
    let mut changes = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let name = operation.get("op").and_then(Value::as_str).unwrap_or("");
        authorize(document, operation)?;
        if name.starts_with("task.") {
            super::task_revision::check(&document.tasks, operation)?;
        }
        let tasks = name.starts_with("task.").then(|| document.tasks.clone());
        let mut change = json!({"index": index, "op": name, "detail": "applied"});
        if name == "task.create" {
            // The created task's id, or the existing task a replayed
            // `client_request_id` names, so the caller need not search for it.
            let created = super::task_apply::create(document, operation, by)?;
            change["task_id"] = Value::String(created.task_id);
            if created.replayed {
                change["replayed"] = Value::Bool(true);
            }
        } else {
            apply_one(document, name, operation, by)?;
        }
        if let Some(tasks) = tasks {
            super::task_revision::advance(&tasks, &mut document.tasks)?;
            let task_id = change
                .get("task_id")
                .or_else(|| operation.get("task_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            if let Some(id) = task_id {
                let before = tasks
                    .iter()
                    .find(|task| task.get("id").and_then(Value::as_str) == Some(id.as_str()));
                let after = document
                    .tasks
                    .iter()
                    .find(|task| task.get("id").and_then(Value::as_str) == Some(id.as_str()));
                let before = before.map(task_state);
                let after = after.map(task_state);
                if before != after {
                    change["task_transition"] =
                        json!({"schema": 1, "task_id": id, "before": before, "after": after});
                } else if name != "task.delete" {
                    change["task_no_change"] = Value::Bool(true);
                }
            }
        }
        changes.push(change);
    }
    Ok(changes)
}

// Only accounting metadata belongs in the public ledger: never copy notes,
// titles, checklist contents, or provider payloads from a task.
fn task_state(task: &Map<String, Value>) -> Value {
    let actor = task.get("assignee").and_then(Value::as_str).filter(|s| {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    });
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
    json!({"revision": task.get("rev").and_then(Value::as_u64).unwrap_or(0), "status": status,
        "assignee": actor, "waiting_on": waiting, "blocked": blocked})
}

fn apply_one(
    document: &mut ContextDoc,
    name: &str,
    operation: &Map<String, Value>,
    by: &str,
) -> io::Result<()> {
    match name {
        "coordination.brief.update" => update_brief(document, operation, by),
        "coordination.note.add" => add_note(document, operation, by),
        "task.update" => super::task_apply::update(document, operation),
        "task.move" => super::task_apply::move_task(document, operation),
        "task.restore" => super::task_apply::restore(document, operation),
        "task.delete" => super::task_apply::delete(document, operation),
        "agent_state.update" => super::agent_state_apply::update(document, operation),
        "agent_state.clear" => super::agent_state_apply::clear(document, operation),
        "meta.merge" => merge_meta(document, operation),
        _ => Err(io::Error::other(format!("unknown context op: {name}"))),
    }
}

fn update_brief(doc: &mut ContextDoc, op: &Map<String, Value>, by: &str) -> io::Result<()> {
    let brief = doc.coordination.entry("brief").or_insert_with(|| json!({}));
    let target = brief
        .as_object_mut()
        .ok_or_else(|| io::Error::other("invalid brief"))?;
    for key in [
        "objective",
        "current_focus",
        "constraints",
        "project_brief",
        "project_brief_stale",
    ] {
        if let Some(value) = op.get(key) {
            target.insert(key.into(), value.clone());
        }
    }
    target.insert("updated_by".into(), Value::String(by.into()));
    target.insert("updated_at".into(), Value::String(utc_now()));
    Ok(())
}

fn add_note(doc: &mut ContextDoc, op: &Map<String, Value>, by: &str) -> io::Result<()> {
    let summary = required(op, "summary")?;
    let kind = string(op, "kind").unwrap_or("decision");
    let key = match kind {
        "decision" => "recent_decisions",
        "handoff" => "recent_handoffs",
        _ => return Err(io::Error::other("note kind must be decision or handoff")),
    };
    let notes = doc.coordination.entry(key).or_insert_with(|| json!([]));
    let target = notes
        .as_array_mut()
        .ok_or_else(|| io::Error::other("invalid notes"))?;
    target.push(json!({
        "at": utc_now(),
        "summary": summary, "task_id": op.get("task_id").cloned().unwrap_or(Value::Null),
        "by": by,
    }));
    if target.len() > 100 {
        target.drain(..target.len() - 100);
    }
    Ok(())
}

fn merge_meta(doc: &mut ContextDoc, op: &Map<String, Value>) -> io::Result<()> {
    let data = op
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| io::Error::other("data is required"))?;
    if let Some(value) = data.get("project_status") {
        doc.meta.insert("project_status".into(), value.clone());
    }
    Ok(())
}

fn required<'a>(op: &'a Map<String, Value>, key: &str) -> io::Result<&'a str> {
    string(op, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| io::Error::other(format!("{key} is required")))
}
fn string<'a>(op: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    op.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod captured_transition_tests {
    use super::*;

    fn captured_context() -> ContextDoc {
        // These are metadata-only projections of three real completed tasks;
        // task numbers and actor identity were consistently replaced.
        let tasks = serde_json::from_str(include_str!("captured_tasks.json"))
            .expect("captured task metadata");
        ContextDoc {
            tasks,
            ..ContextDoc::default()
        }
    }

    #[test]
    fn changed_and_noop_operations_report_only_observed_task_metadata() {
        let mut context = captured_context();
        // A labeled mutation of a captured done task exercises a reopen.
        let operation = json!({"op":"task.move","task_id":"T001","status":"active"})
            .as_object()
            .expect("operation")
            .clone();
        let changes = apply_all(&mut context, &[operation.clone()], "user", |_, _| Ok(()))
            .expect("apply captured task mutation");
        let transition = &changes[0]["task_transition"];
        assert_eq!(transition["task_id"], "T001");
        assert_eq!(transition["before"]["status"], "done");
        assert_eq!(transition["after"]["status"], "active");
        assert_eq!(transition["after"]["assignee"], "actor-a");
        assert_eq!(transition["before"]["revision"], 0);
        assert_eq!(transition["after"]["revision"], 1);
        assert_eq!(transition["after"]["blocked"], false);
        assert!(transition["after"].get("title").is_none());
        assert!(transition["after"].get("notes").is_none());
        let noop =
            apply_all(&mut context, &[operation], "user", |_, _| Ok(())).expect("repeat status");
        assert!(noop[0].get("task_transition").is_none());
        assert_eq!(noop[0]["task_no_change"], true);
    }
    #[test]
    fn captured_task_batch_keeps_order_and_blocker_transition() {
        let mut context = captured_context();
        let operations = [
            json!({"op":"task.move","task_id":"T001","status":"active"}),
            json!({"op":"task.update","task_id":"T001","blocked_by":["T002"]}),
        ]
        .map(|operation| {
            operation
                .as_object()
                .expect("captured test data and setup")
                .clone()
        });
        let changes = apply_all(&mut context, &operations, "user", |_, _| Ok(()))
            .expect("captured test data and setup");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["task_transition"]["after"]["status"], "active");
        assert_eq!(changes[0]["task_transition"]["after"]["revision"], 1);
        assert_eq!(
            changes[1]["task_transition"]["before"],
            changes[0]["task_transition"]["after"]
        );
        assert_eq!(changes[1]["task_transition"]["after"]["revision"], 2);
        assert_eq!(changes[1]["task_transition"]["after"]["blocked"], true);
        assert!(
            changes[1]["task_transition"]["after"]
                .get("blocked_by")
                .is_none()
        );
    }
}
