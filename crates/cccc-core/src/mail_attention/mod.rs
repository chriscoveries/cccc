//! Journaled mail attention. Mail cursors remain exclusively owned by `inbox`.
//!
//! An attention token is shared by passive carriers and native boundary pulls.
//! Plain PTY delivery is never an admission signal. The cache is rebuildable;
//! committed ledger facts, rather than a successful terminal write, own budgets.
use cccc_contracts::{Actor, Event, GroupState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::{GroupDoc, GroupStore, HomeLayout, actors, fs, inbox, ledger};
mod journal_integrity;
mod projection;
mod state;
#[cfg(test)]
mod tests;
use projection::{Projection, project};
use state::Transaction;

pub const POLICY_VERSION: u32 = 1;
pub const FIRST_ATTENTION_SECONDS: i64 = 300;
pub const EXPIRY_SECONDS: i64 = 72 * 60 * 60;
pub const MAX_STANDALONE_WAKEUPS: u32 = 3;
pub const LEGACY_DRAIN_GROUP: &str = "g_819ab6ffb46b";
pub const LEGACY_DRAIN_RULE: &str = "mail-drain";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActorState {
    pub incarnation: String,
    pub episode_id: Option<String>,
    pub opened_by: Option<String>,
    pub set_hash: String,
    pub attention_count: usize,
    pub attempt_no: u32,
    pub due_at: i64,
    pub standalone_wakeups: u32,
    pub last_presentation_at: Option<i64>,
    pub expiry_watermark: Option<i64>,
    pub pending: Option<Reservation>,
    pub legacy_unverified: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub token: String,
    pub owner: String,
    pub carrier_ids: Vec<String>,
    pub standalone: bool,
    pub reserved_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Hint {
    /// Compatibility alias: automatic counts always describe attention, not stale unread mail.
    pub count: usize,
    pub attention_count: usize,
    pub token: String,
    pub action: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub unread_count: usize,
    pub attention_count: usize,
    pub expired_unread_count: usize,
    pub invalid_unread_count: usize,
    pub state: ActorState,
}

fn process_owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

fn digest(parts: &[&str]) -> String {
    format!("{:x}", Sha256::digest(parts.join("\0").as_bytes()))
}

fn incarnation(actor: &Actor) -> String {
    digest(&[&actor.id, &actor.created_at, &actor.generation])
}

fn backoff(actor: &str, attempt: u32) -> i64 {
    let minutes = (10_i64.saturating_mul(1_i64 << attempt.saturating_sub(1).min(6))).min(360);
    let hash = Sha256::digest(format!("{actor}:{attempt}").as_bytes());
    minutes * 60 + i64::from(hash[0] % 31)
}

fn initial_delay(group: &GroupDoc) -> i64 {
    group
        .extra
        .get("delivery")
        .and_then(|v| v.get("mail_notice_after_seconds"))
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(FIRST_ATTENTION_SECONDS)
}

fn standalone_enabled(group: &GroupDoc) -> bool {
    group
        .extra
        .get("delivery")
        .and_then(|v| v.get("mail_notice_after_seconds"))
        .and_then(Value::as_i64)
        .is_none_or(|v| v > 0)
}

/// Legacy notices and the specifically identified competing timer cannot enter
/// any ordinary delivery/replay path. Other automation is unaffected.
pub fn is_mail_originated(group_id: &str, event: &Event) -> bool {
    event.kind == "system.notify"
        && (matches!(
            event.data.get("kind").and_then(Value::as_str),
            Some("mail_notice" | "mail_attention")
        ) || (group_id == LEGACY_DRAIN_GROUP
            && event.data.get("kind").and_then(Value::as_str) == Some("automation")
            && event
                .data
                .get("context")
                .and_then(|v| v.get("rule_id"))
                .and_then(Value::as_str)
                == Some(LEGACY_DRAIN_RULE)))
}

/// Disable only the measured legacy rule and retain its exact prior definition
/// in the journal. A crash after the fact but before the YAML write is retried.
pub fn retire_legacy_rule(home: &HomeLayout, group_id: &str) -> io::Result<()> {
    if group_id != LEGACY_DRAIN_GROUP {
        return Ok(());
    }
    let store = GroupStore::new(home.clone())?;
    let current = store.load(group_id)?;
    let eligible = current
        .automation
        .get("rules")
        .and_then(Value::as_array)
        .is_some_and(|rules| {
            rules.iter().any(|r| {
                r.get("id").and_then(Value::as_str) == Some(LEGACY_DRAIN_RULE)
                    && r.get("enabled").and_then(Value::as_bool) != Some(false)
                    && r.pointer("/action/kind").and_then(Value::as_str) == Some("notify")
                    && r.pointer("/trigger/kind").and_then(Value::as_str) == Some("interval")
                    && r.pointer("/trigger/every_seconds").and_then(Value::as_i64) == Some(300)
            })
        });
    if !eligible {
        return Ok(());
    }
    store.mutate(group_id, |group| {
        let Some(rules) = group.automation.get_mut("rules").and_then(Value::as_array_mut) else { return Ok(()); };
        let Some(rule) = rules.iter_mut().find(|r| r.get("id").and_then(Value::as_str) == Some(LEGACY_DRAIN_RULE)
            && r.get("action").and_then(|v| v.get("kind")).and_then(Value::as_str) == Some("notify")
            && r.get("trigger").and_then(|v| v.get("kind")).and_then(Value::as_str) == Some("interval")
            && r.get("trigger").and_then(|v| v.get("every_seconds")).and_then(Value::as_i64) == Some(300)) else { return Ok(()); };
        if rule.get("enabled").and_then(Value::as_bool) == Some(false) { return Ok(()); }
        let path = store.ledger_path(group_id)?;
        let recorded = ledger::inspect(&path, |events, _| events.iter().any(|e| e.kind == "mail.attention"
            && e.data.get("action").and_then(Value::as_str) == Some("legacy_rule_retired")))?;
        if !recorded {
            let mut event = Event::new("mail.attention", group_id);
            event.by = "system".into();
            event.data = json!({"version":POLICY_VERSION,"action":"legacy_rule_retired","rule_id":LEGACY_DRAIN_RULE,"previous_rule":rule}).as_object().cloned().expect("object");
            ledger::append(&path, &event)?;
        }
        rule.as_object_mut().ok_or_else(|| io::Error::other("invalid legacy rule"))?.insert("enabled".into(), json!(false));
        Ok(())
    })
}

fn transact<T>(
    home: &HomeLayout,
    group_id: &str,
    now: i64,
    owner: &str,
    operation: impl FnOnce(&mut Transaction) -> io::Result<T>,
) -> io::Result<T> {
    let store = GroupStore::new(home.clone())?;
    let dir = store.group_dir(group_id)?.join("state");
    fs::with_exclusive_lock(&dir.join("mail-attention.lock"), || {
        let group = store.load(group_id)?;
        let path = store.ledger_path(group_id)?;
        journal_integrity::validate(&path)?;
        let cursor_map = inbox::cursors(home, group_id)?;
        let events = ledger::inspect(&path, |events, _| {
            events
                .iter()
                .map(|event| {
                    let mut compact = Event {
                        v: event.v,
                        id: event.id.clone(),
                        ts: event.ts.clone(),
                        kind: event.kind.clone(),
                        group_id: event.group_id.clone(),
                        scope_key: String::new(),
                        by: event.by.clone(),
                        data: serde_json::Map::new(),
                    };
                    if event.kind == "mail.attention" {
                        compact.data = event.data.clone();
                    } else {
                        let keys: &[&str] = match event.kind.as_str() {
                            "chat.message" => &["message_mode", "to", "reply_to"],
                            "mail.read" => &["actor_id", "event_id"],
                            "runtime.delivery" => &["actor_id", "source_event_id", "state"],
                            "system.notify" => &["kind", "target_actor_id", "context"],
                            "actor.add" => &["actor"],
                            _ => &[],
                        };
                        compact.data = keys
                            .iter()
                            .filter_map(|key| {
                                event.data.get(*key).map(|v| ((*key).to_owned(), v.clone()))
                            })
                            .collect();
                    }
                    compact
                })
                .collect()
        })?;
        let mut tx = Transaction::replay(
            group,
            events,
            cursor_map,
            now,
            owner,
            path,
            dir.join("mail-attention.json"),
        )?;
        tx.refresh()?;
        let result = operation(&mut tx)?;
        // Every authority fact has already been fsynced and verified. Cache
        // failure cannot strand a committed reservation that never reached input.
        if let Err(error) = tx.save_cache() {
            diagnose_once(group_id, &error);
        }
        Ok(result)
    })
}

/// Reuses the daemon's 60-second unread cadence. Busy/unsupported runtimes only
/// refresh one token; a scan does not create a notification or an input job.
pub fn scan(home: &HomeLayout, group_id: &str, now: i64) -> io::Result<()> {
    transact(home, group_id, now, process_owner(), |_| Ok(()))
}

/// Inspection is separate from presentation: it neither advances a Mail cursor
/// nor claims an attention token. The ledger projection survives cache loss.
pub fn inspect(home: &HomeLayout, group_id: &str, actor_id: &str, now: i64) -> io::Result<Summary> {
    let store = GroupStore::new(home.clone())?;
    let group = store.load(group_id)?;
    let actor =
        actors::find(&group, actor_id).ok_or_else(|| io::Error::other("actor not found"))?;
    let cursors = inbox::cursors(home, group_id)?;
    let path = store.ledger_path(group_id)?;
    journal_integrity::validate(&path)?;
    ledger::inspect(&path, |events, positions| {
        let cache = state::replay(events)?;
        let state = cache
            .actors
            .get(actor_id)
            .filter(|s| s.incarnation == incarnation(actor))
            .cloned()
            .unwrap_or_else(|| ActorState {
                incarnation: incarnation(actor),
                ..ActorState::default()
            });
        let p = project(
            &group,
            actor,
            events,
            positions,
            cursors.get(actor_id).map(String::as_str),
            now,
            state.expiry_watermark,
        );
        Ok(Summary {
            unread_count: p.unread_count,
            attention_count: p.ids.len(),
            expired_unread_count: p.expired_count,
            invalid_unread_count: p.invalid_count,
            state,
        })
    })?
}

/// MCP/bootstrap responses use this daemon-owned claim. It journals the offer
/// BEFORE returning; losing the response cannot authorize immediate replay.
pub fn offer_context(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    carrier: &str,
    now: i64,
) -> io::Result<Option<Hint>> {
    transact(home, group_id, now, process_owner(), |tx| {
        tx.present(actor_id, &[carrier.to_owned()], false, true)
    })
}

/// Prepare one passive hint for an already occurring ordinary delivery. The
/// reservation is durable before external input and shared with MCP carriers.
pub fn reserve_delivery_hint(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    carrier_ids: &[String],
    now: i64,
) -> io::Result<Option<Hint>> {
    transact(home, group_id, now, process_owner(), |tx| {
        tx.present(actor_id, carrier_ids, false, false)
    })
}

/// Called only while an authoritative runtime/input admission lease is held.
/// In this foundation patch the sole adapter is structured between-turn pull;
/// native terminal sessions deliberately remain context-only until they provide
/// an atomic idle admission operation. Caller gives ordinary work priority.
pub fn reserve_boundary_turn(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    source_id: &str,
    now: i64,
) -> io::Result<Option<Hint>> {
    transact(home, group_id, now, process_owner(), |tx| {
        tx.present(actor_id, &[source_id.to_owned()], true, false)
    })
}

/// A coalesced carrier resolves its token once even if it has many source IDs.
/// Runtime acceptance of the carrier is NOT delivery or consumption of Mail.
pub fn finish_carrier(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    source_id: &str,
    outcome: &str,
    now: i64,
) -> io::Result<()> {
    if !matches!(outcome, "accepted" | "failed" | "ambiguous") {
        return Ok(());
    }
    transact(home, group_id, now, process_owner(), |tx| {
        tx.finish(actor_id, source_id, outcome)
    })
}

pub fn new_boundary_source(
    group_id: &str,
    actor_id: &str,
    hint: &Hint,
    source_id: String,
) -> Event {
    let mut event = Event::new("system.notify", group_id);
    event.id = source_id;
    event.by = "system".into();
    event.data = json!({"kind":"mail_attention","im_visibility":"internal","target_actor_id":actor_id,
        "title":"Mail waiting","message":"Mail is waiting. Call cccc_inbox_read at this boundary, handle actionable mail under group policy, then finish this turn.",
        "context":{"token":hint.token,"attention_count":hint.attention_count,"admission":"structured_pull"}}).as_object().cloned().expect("object");
    event
}

/// Reproject immediately before the existing carrier's input. A Mail read,
/// expiry, lifecycle change or lost reservation removes the optional hint;
/// ordinary work still proceeds. No additional token is minted here.
pub fn validate_delivery_hint(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    token: &str,
    now: i64,
) -> io::Result<Option<usize>> {
    transact(home, group_id, now, process_owner(), |tx| {
        tx.validate(actor_id, token)
    })
}

/// Bounded process-local diagnostics. A broken attention journal suppresses
/// optional attention without disabling unrelated automation or delivery.
pub fn diagnose_once(group_id: &str, error: &io::Error) {
    static SEEN: OnceLock<std::sync::Mutex<BTreeMap<String, String>>> = OnceLock::new();
    let message = error.to_string();
    if let Ok(mut seen) = SEEN.get_or_init(Default::default).lock() {
        if seen.get(group_id) == Some(&message) {
            return;
        }
        // One entry per Group, bounded even when Groups are repeatedly created.
        if seen.len() >= 1024 && !seen.contains_key(group_id) {
            seen.clear();
        }
        seen.insert(group_id.to_owned(), message);
        tracing::warn!(%group_id, %error, "mail attention suppressed; optional policy unavailable");
    }
}
