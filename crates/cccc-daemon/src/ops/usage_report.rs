//! Bounded, privacy-filtered source records for the local usage report.
use crate::dispatch::{OpError, OpResult, object, required_arg, store};
use cccc_contracts::DaemonRequest;
use cccc_core::{HomeLayout, context::ContextStore, ledger};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

fn bounded_text(value: &Value, max: usize) -> Option<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
}
fn task_id(value: &Value) -> Option<&str> {
    let id = bounded_text(value, 21)?;
    let digits = id.strip_prefix('T')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}
fn state(value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let status = bounded_text(&value["status"], 16)
        .filter(|s| matches!(*s, "active" | "planned" | "done" | "archived"));
    let waiting = bounded_text(&value["waiting_on"], 16)
        .filter(|s| matches!(*s, "none" | "actor" | "user" | "external"));
    json!({"revision":value["revision"].as_u64(), "status":status,
        "assignee":bounded_text(&value["assignee"],128).filter(|s|s.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'-'|b'_'|b'.'))), "waiting_on":waiting,
        "blocked":value["blocked"].as_bool()})
}

pub(super) fn run(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let group_id = required_arg(request, "group_id")?;
    super::context::load_group(home, &group_id)?;
    let groups = store(home)?;
    let group_dir = groups.group_dir(&group_id).map_err(OpError::io)?;
    let ledger_path = groups.ledger_path(&group_id).map_err(OpError::io)?;
    let (events, has_more, ledger_available) =
        match ledger::tail_existing_read_only(&ledger_path, 1000, None) {
            Ok((events, has_more)) => (events, has_more, true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), true, false),
            Err(error) => return Err(OpError::io(error)),
        };
    let fetched = events.len();
    let mut page_hash = Sha256::new();
    let mut event_hashes = Vec::with_capacity(events.len());
    for event in &events {
        let bytes =
            serde_json::to_vec(event).map_err(|error| OpError::io(std::io::Error::other(error)))?;
        let size = u64::try_from(bytes.len())
            .map_err(|error| OpError::io(std::io::Error::other(error)))?;
        page_hash.update(size.to_be_bytes());
        page_hash.update(&bytes);
        event_hashes.push(format!("{:x}", Sha256::digest(&bytes)));
    }
    let page_hash = ledger_available.then(|| format!("{:x}", page_hash.finalize()));
    let mut filtered = Vec::new();
    let mut referenced = BTreeSet::new();
    if let Some(id) = request.args.get("task_id").and_then(task_id) {
        referenced.insert(id.to_owned());
    }
    let mut changes_truncated = false;
    let mut remaining_changes = 1000_usize;
    let mut transition_gap = false;
    for (event, event_hash) in events.into_iter().zip(event_hashes) {
        if event.group_id != group_id {
            transition_gap = true;
            continue;
        }
        let data = match event.kind.as_str() {
            "turn.usage" => {
                let v = Value::Object(event.data);
                let id = task_id(&v["task_ref"]);
                if let Some(id) = id {
                    referenced.insert(id.to_owned());
                }
                json!({"task_ref":id,"model_id":bounded_text(&v["model_id"],256).filter(|s|s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-'|b'_'|b'.'|b'/'|b':'|b'@'))),
                    "runtime":bounded_text(&v["runtime"],16).filter(|s|matches!(*s,"pi"|"claude")),
                    "input_tokens":v["input_tokens"].as_u64(),"output_tokens":v["output_tokens"].as_u64(),
                    "cache_read_tokens":v["cache_read_tokens"].as_u64(),"cache_write_tokens":v["cache_write_tokens"].as_u64(),
                    "started_at":bounded_text(&v["started_at"],64),"ended_at":bounded_text(&v["ended_at"],64),
                    "status":bounded_text(&v["status"],32).filter(|s| matches!(*s,"completed"|"failed"|"cancelled"))})
            }
            "context.sync" => {
                let mut projected = Vec::new();
                if let Some(changes) = event.data.get("changes").and_then(Value::as_array) {
                    if changes.len() > remaining_changes {
                        changes_truncated = true;
                    }
                    let admitted = changes.len().min(remaining_changes);
                    remaining_changes -= admitted;
                    for change in changes.iter().take(admitted) {
                        let is_task = change["op"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("task."));
                        let transition = &change["task_transition"];
                        if is_task
                            && ((transition["schema"] != 1 && change["task_no_change"] != true)
                                || change["op"] == "task.delete")
                        {
                            transition_gap = true;
                        }
                        let Some(id) = task_id(&transition["task_id"]) else {
                            if is_task && change["task_no_change"] != true {
                                transition_gap = true;
                            }
                            continue;
                        };
                        if transition["schema"] != 1 {
                            continue;
                        }
                        referenced.insert(id.to_owned());
                        projected.push(json!({"task_transition":{"schema":1,"task_id":id,
                            "before":state(&transition["before"]),"after":state(&transition["after"])}}));
                    }
                } else {
                    transition_gap = true;
                }
                json!({"changes":projected})
            }
            _ => continue,
        };
        filtered.push(json!({"id":event.id,"ts":event.ts,"kind":event.kind,"group_id":event.group_id,"canonical_sha256":event_hash,"data":data}));
    }
    // If the lock or migration proof is missing, the snapshot remains unknown;
    // reporting never initializes or migrates the task store.
    let snapshot = ContextStore::load_selected_read_only(&group_dir, &referenced).ok().map(|tasks| {
        tasks.into_iter()
            .map(|task| json!({"id":task.get("id"), "status":task.get("status").and_then(|v|bounded_text(v,16)).filter(|s|matches!(*s,"active"|"planned"|"done"|"archived")),
                "waiting_on":task.get("waiting_on").and_then(|v|bounded_text(v,16)).filter(|s|matches!(*s,"none"|"actor"|"user"|"external")),
                "blocked":task.get("blocked").and_then(Value::as_bool)})).collect::<Vec<_>>()
    });
    object(
        json!({"events":filtered,"has_more":has_more,"fetched":fetched,"ledger_snapshot_available":ledger_available,"canonical_page_sha256":page_hash,"changes_truncated":changes_truncated,"transition_gap":transition_gap,"tasks":snapshot}),
    )
}

#[cfg(test)]
mod captured_tests {
    use super::*;
    use cccc_contracts::Event;
    use cccc_core::GroupStore;

    #[test]
    fn captured_source_strips_content_and_reports_legacy_transition_gaps() {
        let temp = tempfile::tempdir().expect("captured test data and setup");
        let home = HomeLayout::from_path(temp.path()).expect("captured test data and setup");
        let groups = GroupStore::new(home.clone()).expect("captured test data and setup");
        let group = groups
            .create("capture", "")
            .expect("captured test data and setup");
        let group_dir = groups
            .group_dir(&group.group_id)
            .expect("captured test data and setup");
        let context = group_dir.join("context");
        std::fs::create_dir_all(context.join("tasks")).expect("captured test data and setup");
        std::fs::File::create(context.join(".rust-context.lock"))
            .expect("captured test data and setup");
        let tasks: Vec<Value> = serde_json::from_str(include_str!(
            "../../../cccc-core/src/context/captured_tasks.json"
        ))
        .expect("captured test data and setup");
        std::fs::write(
            context.join("tasks/T001.yaml"),
            serde_yaml::to_string(&tasks[0]).expect("captured test data and setup"),
        )
        .expect("captured test data and setup");
        let captured: Vec<Value> = serde_json::from_str(include_str!(
            "../../../cccc-core/src/usage_report_captured.json"
        ))
        .expect("captured test data and setup");
        for (index, mut row) in [captured[0].clone(), captured[5].clone()]
            .into_iter()
            .enumerate()
        {
            row["group_id"] = json!(group.group_id);
            // Labeled mutation: an opaque content field must never leave the source.
            if index == 0 {
                row["data"]["prompt"] = json!("content-to-omit");
            }
            let event: Event = serde_json::from_value(row).expect("captured test data and setup");
            ledger::append(
                &groups
                    .ledger_path(&group.group_id)
                    .expect("captured test data and setup"),
                &event,
            )
            .expect("captured test data and setup");
        }
        let request = DaemonRequest {
            v: 1,
            op: "usage_report_source".into(),
            args: json!({"group_id":group.group_id,"task_id":"T001"})
                .as_object()
                .expect("captured test data and setup")
                .clone(),
        };
        let result = run(&home, &request).expect("captured test data and setup");
        assert_eq!(result["ledger_snapshot_available"], true);
        assert_eq!(result["transition_gap"], true);
        assert_eq!(result["tasks"][0]["status"], "done");
        assert!(
            !serde_json::to_string(&result)
                .expect("captured test data and setup")
                .contains("content-to-omit")
        );
    }
}
