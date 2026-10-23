//! Provider-reported counters, committed only at an owned turn's settlement.
use cccc_contracts::{DaemonRequest, Event, utc_now};
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
            .or_else(|| record.get("uuid").and_then(Value::as_str))
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
        }
    }

    fn observe(&mut self, id: String, message: &Value, fields: [&str; 4]) {
        let Some(usage) = message.get("usage").filter(|v| v.is_object()) else {
            return;
        };
        self.samples.insert(
            id,
            Sample {
                tokens: fields.map(|field| usage.get(field).and_then(Value::as_u64)),
                model: message
                    .get("model")
                    .and_then(Value::as_str)
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
        let models = self
            .samples
            .values()
            .map(|s| s.model.as_deref())
            .collect::<BTreeSet<_>>();
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
    task_id: Option<&str>,
    usage: &Value,
    status: &str,
) -> io::Result<()> {
    let store = GroupStore::new(home.clone())?;
    let path = store.ledger_path(group_id)?;
    let key =
        serde_json::to_vec(&(group_id, actor_id, generation, turn_id)).map_err(io::Error::other)?;
    let id = format!("turn-usage-{:x}", Sha256::digest(key));
    let mut event = Event::new("turn.usage", group_id);
    event.id = id;
    event.by = actor_id.to_owned();
    event.data = json!({"actor_id":actor_id,"task_id":task_id,"turn_id":turn_id,
        "generation":generation,"turn_count":1,"status":status})
    .as_object()
    .expect("object")
    .clone();
    // Never persist opaque provider payloads, prompt text or tool arguments.
    for field in [
        "model_id",
        "source",
        "started_at",
        "ended_at",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "usage_samples",
    ] {
        event.data.insert(field.into(), usage[field].clone());
    }
    let directory = store.state_dir(group_id)?.join("turn-usage");
    let recorded = with_exclusive_lock(&directory.join("writer.lock"), || {
        let done = directory.join("committed").join(&event.id);
        if done.exists() {
            return Ok(());
        }
        let pending = directory.join("pending").join(&event.id);
        let recovering = pending.exists();
        if !recovering {
            cccc_core::fs::write_json(&pending, &event)?;
        }
        deliver(&path, &directory, &pending, recovering)
    });
    recorded?;
    if let Some(task_id) = task_id {
        // After the event is durable, so a turn nobody recorded never moves a
        // task's spend. Best effort: a task store that will not take the write
        // must not cost the turn its usage record.
        accrue_task_cost(
            home,
            group_id,
            task_id,
            usage,
            std::path::Path::new(super::MODELS_PATH),
        );
    }
    Ok(())
}

/// What this catalog says the model costs, applied to the task the turn named.
fn accrue_task_cost(
    home: &HomeLayout,
    group_id: &str,
    task_id: &str,
    usage: &Value,
    catalog: &std::path::Path,
) {
    let registry = cccc_core::models::ModelRegistry::load_from_path(catalog);
    let outcome = registry.turn_cost(usage.get("model_id").and_then(Value::as_str), usage);
    let mut operation = serde_json::Map::new();
    operation.insert("op".into(), json!("task.cost.accrue"));
    operation.insert("task_id".into(), json!(task_id));
    match outcome {
        cccc_core::models::TurnCost::Priced { usd, .. } => {
            operation.insert("outcome".into(), json!("priced"));
            operation.insert("usd".into(), json!(usd));
        }
        cccc_core::models::TurnCost::Unpriced { .. } => {
            operation.insert("outcome".into(), json!("unpriced"));
        }
        cccc_core::models::TurnCost::Unmetered => {
            operation.insert("outcome".into(), json!("unmetered"));
        }
    }
    let applied = cccc_core::context::ContextStore::new(home.clone())
        .and_then(|store| store.sync(group_id, &[operation], None, "cccc-daemon", false));
    if let Err(error) = applied {
        tracing::warn!(%task_id, %error, "turn metered; the task's cost was not written");
    }
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
    if !done.exists() {
        // Only an interrupted append needs a recovery scan. Ordinary turns and
        // read queries never build or retain the generic full-history index.
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
    if !pending.exists() {
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

pub(super) fn list(home: &HomeLayout, request: &DaemonRequest) -> crate::dispatch::OpResult {
    use crate::dispatch::{OpError, object, required_arg, string_arg};
    let group = required_arg(request, "group_id")?;
    let path = GroupStore::new(home.clone())
        .map_err(OpError::io)?
        .ledger_path(&group)
        .map_err(OpError::io)?;
    let actor = string_arg(request, "actor_id");
    let limit = request
        .args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 1000) as usize;
    let mut events = Vec::new();
    ledger::visit_newest_first(&path, |event| {
        if event.kind == "turn.usage"
            && actor
                .as_ref()
                .is_none_or(|a| event.data["actor_id"].as_str() == Some(a.as_str()))
        {
            events.push(event);
        }
        if events.len() == limit {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .map_err(OpError::io)?;
    events.reverse();
    object(json!({"usage":events}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn captured_usage() -> Value {
        crate::ops::codex_voice_analyst::replay_captured_pi_turn(
            crate::ops::codex_voice_analyst::CAPTURED_PI_TOOL_TURN,
            "captured",
        )
        .last()
        .expect("completion")["params"]["turn"]["usage"]
            .clone()
    }

    #[test]
    fn captured_turn_usage_recovers_an_interrupted_ledger_append_without_duplicates() {
        let temp = tempfile::tempdir().expect("home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("usage recovery", "").expect("group");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        let lock = path
            .parent()
            .expect("ledger directory")
            .join("state/ledger/ledger.lock");
        if lock.is_file() {
            std::fs::remove_file(&lock).expect("remove disposable writer lock");
        }
        std::fs::create_dir_all(&lock).expect("obstruct ledger writer");
        assert!(
            record(
                &home,
                &group.group_id,
                "peer",
                "generation",
                "captured",
                None,
                &captured_usage(),
                "completed"
            )
            .is_err()
        );
        std::fs::remove_dir(&lock).expect("restore ledger writer");
        // The ordinary publisher performs recovery, with no provider notification replay.
        crate::ops::actor_activity::Publisher::default()
            .tick(&home)
            .expect("recovery tick");
        let usage = ledger::read_all(&path)
            .expect("ledger")
            .into_iter()
            .find(|event| event.kind == "turn.usage")
            .expect("durable usage");
        let directory = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("turn-usage");
        // Emulate a crash after append but before the committed marker is durable.
        std::fs::remove_file(directory.join("committed").join(&usage.id)).expect("marker");
        cccc_core::fs::write_json(&directory.join("pending").join(&usage.id), &usage)
            .expect("pending record");
        flush(&home, &group.group_id).expect("recover committed append");
        assert_eq!(
            ledger::read_all(&path)
                .expect("ledger")
                .iter()
                .filter(|event| event.kind == "turn.usage")
                .count(),
            1
        );
    }

    #[test]
    fn missing_captured_counter_remains_unknown_instead_of_zero() {
        let mut record: Value = serde_json::from_str(
            include_str!("codex_voice_analyst/claude/captured_turn_usage.jsonl")
                .lines()
                .next()
                .expect("record"),
        )
        .expect("metadata");
        record["message"]["usage"]
            .as_object_mut()
            .expect("usage")
            .remove("output_tokens");
        let mut totals = Accumulator::new();
        totals.claude(&record);
        let usage = totals.summary("claude_transcript");
        assert!(usage["output_tokens"].is_null());
        assert_eq!(
            usage["input_tokens"],
            record["message"]["usage"]["input_tokens"]
        );
    }

    /// The first captured Claude record: 2 input, 40 output, 213,575 cache reads
    /// and 905 cache writes — 214,522 metered tokens.
    fn captured_claude_usage(model_id: &str) -> Value {
        let record: Value = serde_json::from_str(
            include_str!("codex_voice_analyst/claude/captured_turn_usage.jsonl")
                .lines()
                .next()
                .expect("record"),
        )
        .expect("record");
        let usage = &record["message"]["usage"];
        json!({
            "model_id": model_id,
            "input_tokens": usage["input_tokens"],
            "output_tokens": usage["output_tokens"],
            "cache_read_tokens": usage["cache_read_input_tokens"],
            "cache_write_tokens": usage["cache_creation_input_tokens"],
        })
    }

    /// Two catalog entries at the prices the owner set: a token-priced model and
    /// a free one. The catalog file is the daemon's own format.
    fn catalog(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("models.toml");
        std::fs::write(
            &path,
            "[models.ds1017]\nusd_per_mtok = 0.07\n\n[models.free]\nusd_per_mtok = 0.0\n",
        )
        .expect("catalog");
        path
    }

    fn group_with_task(home: &HomeLayout) -> (String, String) {
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("cost", "").expect("group");
        let synced = cccc_core::context::ContextStore::new(home.clone())
            .expect("context")
            .sync(
                &group.group_id,
                &[json!({"op":"task.create","title":"metered work"})
                    .as_object()
                    .cloned()
                    .expect("operation")],
                None,
                "user",
                false,
            )
            .expect("create task");
        let task_id = synced.changes[0]["task_id"]
            .as_str()
            .expect("task id")
            .to_owned();
        (group.group_id, task_id)
    }

    fn task(home: &HomeLayout, group_id: &str, task_id: &str) -> Map<String, Value> {
        let doc = cccc_core::context::ContextStore::new(home.clone())
            .expect("context")
            .load(group_id)
            .expect("load");
        doc.tasks
            .into_iter()
            .find(|task| task["id"].as_str() == Some(task_id))
            .expect("task")
    }

    #[test]
    fn a_metered_turn_costs_the_task_what_the_catalog_says() {
        let temp = tempfile::tempdir().expect("home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let (group_id, task_id) = group_with_task(&home);
        let path = catalog(temp.path());

        accrue_task_cost(
            &home,
            &group_id,
            &task_id,
            &captured_claude_usage("ds1017"),
            &path,
        );

        let task = task(&home, &group_id, &task_id);
        // 214_522 tokens x $0.07 per million.
        let spent = task["cost_usd"].as_f64().expect("cost");
        assert!((spent - 0.01501654).abs() < 1e-12, "{spent}");
        assert_eq!(task["cost_turns"], 1);
        assert_eq!(task["unpriced_turns"], 0);
        assert_eq!(task["unmetered_turns"], 0);
        assert!(task["cost_updated_at"].as_str().is_some());
    }

    #[test]
    fn a_turn_on_an_unpriced_model_is_counted_and_never_guessed() {
        let temp = tempfile::tempdir().expect("home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let (group_id, task_id) = group_with_task(&home);
        let path = catalog(temp.path());

        accrue_task_cost(
            &home,
            &group_id,
            &task_id,
            &captured_claude_usage("not-in-the-catalog"),
            &path,
        );

        let task = task(&home, &group_id, &task_id);
        assert_eq!(task["cost_usd"], 0.0);
        assert_eq!(task["unpriced_turns"], 1);
        assert_eq!(task["cost_turns"], 0);
    }

    #[test]
    fn a_free_model_is_priced_at_zero_and_not_called_unpriced() {
        let temp = tempfile::tempdir().expect("home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let (group_id, task_id) = group_with_task(&home);
        let path = catalog(temp.path());

        accrue_task_cost(
            &home,
            &group_id,
            &task_id,
            &captured_claude_usage("free"),
            &path,
        );

        let task = task(&home, &group_id, &task_id);
        assert_eq!(task["cost_usd"], 0.0);
        assert_eq!(task["cost_turns"], 1);
        assert_eq!(task["unpriced_turns"], 0);
    }

    #[test]
    fn a_turn_that_reported_no_counters_is_unmetered_not_free() {
        let temp = tempfile::tempdir().expect("home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let (group_id, task_id) = group_with_task(&home);
        let path = catalog(temp.path());

        accrue_task_cost(
            &home,
            &group_id,
            &task_id,
            &json!({"model_id": "ds1017"}),
            &path,
        );

        let task = task(&home, &group_id, &task_id);
        assert_eq!(task["unmetered_turns"], 1);
        assert_eq!(task["cost_turns"], 0);
        assert_eq!(task["unpriced_turns"], 0);
        assert_eq!(task["cost_usd"], 0.0);
    }

    #[test]
    fn a_turn_for_a_task_that_is_not_there_leaves_no_trace() {
        let temp = tempfile::tempdir().expect("home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let (group_id, task_id) = group_with_task(&home);
        let path = catalog(temp.path());

        // Best effort by design: the usage event is already durable, and an
        // unknown task must not make the accounting path fail the turn.
        accrue_task_cost(
            &home,
            &group_id,
            "T999",
            &captured_claude_usage("ds1017"),
            &path,
        );

        let task = task(&home, &group_id, &task_id);
        assert_eq!(task["cost_usd"], 0.0);
        assert_eq!(task["cost_turns"], 0);
    }
}
