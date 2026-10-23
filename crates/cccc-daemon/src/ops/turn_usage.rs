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

    fn observe(&mut self, id: String, message: &Value, fields: [&str; 4]) {
        // A message with missing usage still participates: omitting it would
        // silently turn a partial observation into an apparently complete sum.
        let usage = &message["usage"];
        self.samples.insert(
            id,
            Sample {
                tokens: fields.map(|field| usage.get(field).and_then(Value::as_u64)),
                model: message
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| !model.trim().is_empty())
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

pub(super) fn record(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    generation: &str,
    turn_id: &str,
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
    event.data = json!({"actor_id":actor_id,"turn_id":turn_id,
        "generation":generation,"status":status})
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
    with_exclusive_lock(&directory.join("writer.lock"), || {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // A fresh publisher models daemon restart; no in-memory receipts survive.
        crate::ops::actor_activity::Publisher::default()
            .tick(&home)
            .expect("recover committed append after restart");
        record(
            &home,
            &group.group_id,
            "peer",
            "generation",
            "captured",
            &captured_usage(),
            "completed",
        )
        .expect("duplicate completion after restart");
        assert_eq!(
            ledger::read_all(&path)
                .expect("ledger")
                .iter()
                .filter(|event| event.kind == "turn.usage")
                .count(),
            1
        );
    }
}
