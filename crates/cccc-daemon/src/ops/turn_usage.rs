//! Provider-reported counters, committed only at an owned turn's settlement.
use cccc_contracts::{Event, utc_now};
use cccc_core::{GroupStore, HomeLayout, fs::with_exclusive_lock, ledger};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::ops::ControlFlow;

#[derive(Debug)]
struct Sample {
    tokens: [Option<u64>; 4],
    model: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Accumulator {
    started_at: String,
    samples: BTreeMap<String, Sample>,
}

impl Accumulator {
    pub(crate) fn new() -> Self {
        Self {
            started_at: utc_now(),
            samples: BTreeMap::new(),
        }
    }

    pub(crate) fn pi(&mut self, id: String, message: &Value) {
        self.observe(id, message, ["input", "output", "cacheRead", "cacheWrite"]);
    }

    pub(crate) fn claude(&mut self, record: &Value) {
        // Several transcript content records can carry the same message snapshot.
        // Replace that snapshot; only distinct provider messages are summed.
        if let Some(id) = record
            .pointer("/message/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.observe(
                id.to_owned(),
                &record["message"],
                [
                    "input_tokens",
                    "output_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                ],
            );
        } else {
            self.observe(String::new(), &Value::Null, [""; 4]);
        }
    }

    fn observe(&mut self, id: String, message: &Value, fields: [&str; 4]) {
        let usage = &message["usage"];
        self.samples.insert(
            id,
            Sample {
                tokens: fields.map(|field| usage.get(field).and_then(Value::as_u64)),
                model: message
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned),
            },
        );
    }

    pub(crate) fn summary(&self, source: &str) -> Value {
        let sum = |index: usize| {
            (!self.samples.is_empty())
                .then_some(0_u64)
                .and_then(|zero| {
                    self.samples
                        .values()
                        .try_fold(zero, |sum, sample| sum.checked_add(sample.tokens[index]?))
                })
        };
        let models: BTreeSet<_> = self
            .samples
            .values()
            .map(|sample| sample.model.as_deref())
            .collect();
        let model = if models.len() == 1 {
            models.first().copied().flatten()
        } else {
            None
        };
        json!({"source":source,"model_id":model,"input_tokens":sum(0),
            "output_tokens":sum(1),"cache_read_tokens":sum(2),"cache_write_tokens":sum(3),
            "usage_samples":self.samples.len(),"started_at":self.started_at,"ended_at":utc_now()})
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    generation: &str,
    turn_id: &str,
    delivery_id: Option<&str>,
    task_id: Option<&str>,
    task_ref: Option<&str>,
    usage: &Value,
    status: &str,
) -> io::Result<()> {
    let store = GroupStore::new(home.clone())?;
    let path = store.ledger_path(group_id)?;
    let key =
        serde_json::to_vec(&(group_id, actor_id, generation, turn_id)).map_err(io::Error::other)?;
    let mut event = Event::new("turn.usage", group_id);
    event.id = format!("turn-usage-{:x}", Sha256::digest(key));
    event.by = actor_id.to_owned();
    event.data = json!({"actor_id":actor_id,"generation":generation,"turn_id":turn_id,
        "delivery_id":delivery_id,"task_id":task_id,"task_ref":task_ref,"status":status})
    .as_object()
    .expect("object")
    .clone();
    for field in [
        "source",
        "model_id",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "usage_samples",
        "started_at",
        "ended_at",
    ] {
        event.data.insert(field.into(), usage[field].clone());
    }
    let directory = store.state_dir(group_id)?.join("turn-usage");
    with_exclusive_lock(&directory.join("writer.lock"), || {
        if directory.join("committed").join(&event.id).try_exists()? {
            return Ok(());
        }
        let pending = directory.join("pending").join(&event.id);
        let recovering = pending.try_exists()?;
        if !recovering {
            cccc_core::fs::write_json(&pending, &event)?;
        }
        deliver(&path, &directory, &pending, recovering)
    })
}

fn deliver(
    path: &std::path::Path,
    directory: &std::path::Path,
    pending: &std::path::Path,
    recovering: bool,
) -> io::Result<()> {
    let event: Event = cccc_core::fs::read_json(pending)?;
    if event.kind != "turn.usage"
        || pending.file_name().and_then(|v| v.to_str()) != Some(event.id.as_str())
    {
        return Err(io::Error::other("invalid pending turn usage"));
    }
    let done = directory.join("committed").join(&event.id);
    if !done.try_exists()? {
        // Only an interrupted append needs a recovery scan. Ordinary turns
        // never build or retain a generic full-history index.
        let mut committed = false;
        if recovering {
            ledger::visit_newest_first(path, |existing| {
                if existing.id == event.id {
                    committed = true;
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })?;
        }
        if !committed {
            ledger::append(path, &event)?;
        }
        cccc_core::fs::atomic_write(&done, event.id.as_bytes())?;
    }
    std::fs::remove_file(pending)
}

/// Retry durable pending records on the existing Actor activity tick, including after restart.
pub(super) fn flush(home: &HomeLayout, group_id: &str) -> io::Result<()> {
    let store = GroupStore::new(home.clone())?;
    let directory = store.state_dir(group_id)?.join("turn-usage");
    let pending = directory.join("pending");
    if !pending.try_exists()? {
        return Ok(());
    }
    with_exclusive_lock(&directory.join("writer.lock"), || {
        for entry in std::fs::read_dir(&pending)?.take(16) {
            deliver(
                &store.ledger_path(group_id)?,
                &directory,
                &entry?.path(),
                true,
            )?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> Vec<Value> {
        include_str!("turn_usage/claude.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).expect("redacted capture"))
            .collect()
    }

    fn usage() -> Value {
        let captured: Value = serde_json::from_str(include_str!("turn_usage/deliveries.json"))
            .expect("captured deliveries");
        captured["turns"][0]["usage"].clone()
    }

    #[test]
    fn captured_claude_messages_are_deduplicated_before_summing() {
        let mut totals = Accumulator::new();
        for row in claude().iter().filter(|row| row["type"] == "assistant") {
            totals.claude(row);
        }
        let result = totals.summary("claude_transcript");
        assert_eq!(result["usage_samples"], 7);
        assert_eq!(result["input_tokens"], 14);
        assert_eq!(result["output_tokens"], 4698);
        assert_eq!(result["cache_read_tokens"], 1521929);
        assert_eq!(result["cache_write_tokens"], 9321);
        assert_eq!(result["model_id"], "claude-opus-5-5");
    }

    #[test]
    fn captured_claude_snapshot_model_follows_the_latest_message_sample() {
        let rows = claude();
        let captured = &rows[0];
        let mut missing = captured.clone();
        missing["message"]
            .as_object_mut()
            .expect("captured message is an object")
            .remove("model");
        let mut totals = Accumulator::new();
        totals.claude(&missing);
        assert!(totals.summary("claude_transcript")["model_id"].is_null());
        totals.claude(captured);
        let summary = totals.summary("claude_transcript");
        assert_eq!(
            summary["model_id"], captured["message"]["model"],
            "latest captured snapshot restores the model"
        );
        assert_eq!(summary["usage_samples"], 1);
        assert_eq!(
            summary["input_tokens"],
            captured["message"]["usage"]["input_tokens"]
        );
        totals.claude(&missing);
        assert!(totals.summary("claude_transcript")["model_id"].is_null());
        // A superseded conflicting label cannot outlive the message snapshot.
        let mut changed = captured.clone();
        changed["message"]["model"] = rows[1]["message"]["id"].clone();
        totals.claude(&changed);
        totals.claude(captured);
        assert_eq!(
            totals.summary("claude_transcript")["model_id"],
            captured["message"]["model"]
        );
        assert_ne!(captured["message"]["id"], rows[1]["message"]["id"]);
        let mut distinct = rows[1].clone();
        distinct["message"]["model"] = Value::Null;
        totals.claude(&distinct);
        assert!(totals.summary("claude_transcript")["model_id"].is_null());
    }

    #[test]
    fn capture_mutations_preserve_unknown_counters_and_identity() {
        let captured = claude();
        // Each negative control changes one field of actual assistant metadata.
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            let mut row = captured[0].clone();
            row["message"]["usage"]
                .as_object_mut()
                .expect("captured usage is an object")
                .remove(field);
            let mut totals = Accumulator::new();
            totals.claude(&row);
            let normalized = match field {
                "cache_read_input_tokens" => "cache_read_tokens",
                "cache_creation_input_tokens" => "cache_write_tokens",
                other => other,
            };
            assert!(totals.summary("claude_transcript")[normalized].is_null());
        }
        for field in ["id", "usage"] {
            let mut row = captured[1].clone();
            row["message"]
                .as_object_mut()
                .expect("captured message is an object")
                .remove(field);
            let mut totals = Accumulator::new();
            totals.claude(&captured[0]);
            totals.claude(&row);
            assert!(totals.summary("claude_transcript")["input_tokens"].is_null());
        }
        let mut row = captured[1].clone();
        row["message"]["model"] = Value::Null;
        row["message"]["usage"]["input_tokens"] = json!(u64::MAX);
        let mut totals = Accumulator::new();
        totals.claude(&captured[0]);
        totals.claude(&row);
        let result = totals.summary("claude_transcript");
        assert!(result["model_id"].is_null());
        assert!(result["input_tokens"].is_null());
        assert!(Accumulator::new().summary("pi_rpc")["input_tokens"].is_null());
        let mut conflict = captured[1].clone();
        conflict["message"]["model"] = captured[1]["message"]["id"].clone();
        let mut totals = Accumulator::new();
        totals.claude(&captured[0]);
        totals.claude(&conflict);
        assert!(totals.summary("claude_transcript")["model_id"].is_null());
    }

    #[test]
    fn captured_usage_retries_append_and_marker_crash_boundaries_once() {
        let temp = tempfile::tempdir().expect("create temporary test directory");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("create test home");
        let store = GroupStore::new(home.clone()).expect("open test group store");
        let group = store
            .create("usage retry", "")
            .expect("create test group")
            .group_id;
        let path = store.ledger_path(&group).expect("test group ledger path");
        let state = store.state_dir(&group).expect("test group state path");
        let lock = state.join("ledger/ledger.lock");
        if lock.is_file() {
            std::fs::remove_file(&lock).expect("remove test ledger lock file");
        }
        std::fs::create_dir_all(&lock).expect("install deliberate ledger lock obstruction");
        assert!(
            record(
                &home,
                &group,
                "peer",
                "generation",
                "turn",
                Some("delivery"),
                Some("runtime-task"),
                Some("T001"),
                &usage(),
                "failed"
            )
            .is_err()
        );
        std::fs::remove_dir(&lock).expect("remove deliberate ledger lock obstruction");
        crate::ops::actor_activity::Publisher::default()
            .tick(&home)
            .expect("activity tick retries pending usage");
        let read = || {
            ledger::read_all(&path)
                .expect("read test usage ledger")
                .into_iter()
                .filter(|event| event.kind == "turn.usage")
                .collect::<Vec<_>>()
        };
        let events = read();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.data["task_id"], "runtime-task");
        assert_eq!(event.data["task_ref"], "T001");
        assert_eq!(event.data["delivery_id"], "delivery");
        assert_eq!(event.data["status"], "failed");
        let directory = state.join("turn-usage");
        std::fs::remove_file(directory.join("committed").join(&event.id))
            .expect("remove commit marker to model crash boundary");
        cccc_core::fs::write_json(&directory.join("pending").join(&event.id), event)
            .expect("restore captured event as pending");
        flush(&home, &group).expect("recover pending usage after append");
        record(
            &home,
            &group,
            "peer",
            "generation",
            "turn",
            Some("delivery"),
            Some("runtime-task"),
            Some("T001"),
            &usage(),
            "failed",
        )
        .expect("retry identical settled usage");
        assert_eq!(read().len(), 1);
        for status in ["completed", "cancelled"] {
            record(
                &home,
                &group,
                "peer",
                "generation",
                status,
                None,
                None,
                None,
                &usage(),
                status,
            )
            .expect("record settled status with unknown attribution");
        }
        let events = read();
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .skip(1)
                .all(|event| event.data["task_ref"].is_null())
        );
        assert_eq!(
            std::fs::read_dir(directory.join("pending"))
                .expect("read pending usage directory")
                .count(),
            0
        );
    }
}
