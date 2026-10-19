use cccc_contracts::{Actor, ActorRuntime, ActorSubmit, Event, GroupState};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, inbox};
use serde::Serialize;
use serde_json::{Map, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::ops::actor_delivery_worker;

mod drain;
mod lifecycle;
pub(crate) use drain::{drain_group, pending_group_ids};
pub(crate) use lifecycle::{DeferredWorkerShutdown, cancel_actor};
pub use lifecycle::{shutdown_actor, shutdown_all, shutdown_group};

const QUEUE_CAPACITY: usize = 256;
const COMPLETION_CAPACITY: usize = 4096;
const BATCH_CAPACITY: usize = 64;
const DEFERRED_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(4);
/// Attempts inside one round before the batch returns to the deferred queue.
const DELIVERY_ATTEMPTS_PER_ROUND: u64 = 3;
/// Owner spec dd17cbba item 3: a batch the runtime *refuses* is retried a
/// bounded number of times, then settled Failed with one surfaced error event.
/// Without a bound, a job the runtime can never accept (an oversize or
/// timed-out turn) is re-offered for ever and keeps re-breaking the session it
/// is aimed at.
///
/// Refusal is distinguished from not-ready on purpose. An actor that is still
/// booting, or busy with a turn, has not refused anything: holding its message
/// is a queue position, and expiring it would drop mail during normal work.
/// Only a runtime that was asked and said no counts against this budget.
const DELIVERY_REJECTION_ATTEMPTS: u32 = 3;

fn rejection_backoff(attempts: u32) -> std::time::Duration {
    let exponent = attempts.saturating_sub(1).min(3);
    std::time::Duration::from_millis(250 * (1_u64 << exponent))
}

/// Consecutive failed batches after which the per-actor breaker opens. The
/// breaker is the second level of the same guard: three separate messages
/// failing in a row means the actor, not the message, is the fault.
const DELIVERY_BREAKER_FAILURES: u32 = 3;
/// How long an open breaker refuses new work before it half-opens (the next
/// batch is attempted again, and a success closes it).
const DELIVERY_BREAKER_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);
const TERMINAL_SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(1_500);
const TERMINAL_REPEAT_SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

type Key = (String, String);

/// Deliver text to the Runtime's native terminal. A successful return means
/// the payload and submit keys were written to that terminal; whether the
/// Runtime steers the active turn or queues the input remains its own policy.
pub(super) fn submit_terminal_text(
    group_id: &str,
    actor: &Actor,
    text: &str,
    cancelled: &AtomicBool,
) -> bool {
    let raw = text.trim_end_matches(['\r', '\n']);
    if raw.is_empty() {
        return false;
    }
    if super::local_headless::running(group_id, &actor.id)
        && !cccc_runtime::wait_for_input_ready(
            group_id,
            &actor.id,
            std::time::Duration::from_secs(15),
            cancelled,
        )
        .unwrap_or(false)
    {
        return false;
    }
    let bracketed = raw.contains(['\r', '\n'])
        && cccc_runtime::bracketed_paste_enabled(group_id, &actor.id).unwrap_or(false);
    let payload = if bracketed {
        format!("\u{1b}[200~{raw}\u{1b}[201~")
    } else if raw.contains(['\r', '\n']) {
        raw.lines().collect::<Vec<_>>().join(" ")
    } else {
        raw.to_owned()
    };
    let submits = terminal_submit_sequence(actor);
    let delay = if submits.is_empty() {
        std::time::Duration::ZERO
    } else {
        TERMINAL_SUBMIT_DELAY
    };
    cccc_runtime::submit_sequence_interruptible(
        group_id,
        &actor.id,
        payload.as_bytes(),
        submits,
        delay,
        TERMINAL_REPEAT_SUBMIT_DELAY,
        cancelled,
    )
    .unwrap_or(false)
}

pub(super) fn terminal_submit_sequence(actor: &Actor) -> &'static [&'static [u8]] {
    match actor.submit {
        ActorSubmit::Enter
            if matches!(actor.runtime, ActorRuntime::Codex | ActorRuntime::Copilot) =>
        {
            &[b"\r", b"\r"]
        }
        ActorSubmit::Enter => &[b"\r"],
        ActorSubmit::Newline => &[b"\n"],
        ActorSubmit::None => &[],
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DispatchReport {
    pub accepted: bool,
    pub state: &'static str,
    pub targeted: usize,
    pub online: usize,
    pub queued: usize,
}

#[derive(Clone)]
pub(super) struct DeliveryJob {
    pub home: HomeLayout,
    pub group: GroupDoc,
    pub actor: Actor,
    pub event: Event,
}

/// A delivery outcome whose ledger record is still pending. It holds the
/// in-flight slot until recorded, so the event is never dispatched again.
pub(super) struct DeliveryCompletion {
    pub group_id: String,
    pub actor_id: String,
    pub actor_created_at: String,
    pub event_id: String,
    pub transport: String,
    /// Set for a withdrawn delivery (never sent); otherwise it was accepted.
    pub withdrawn_reason: Option<String>,
}

struct DeliveryWorker {
    sender: Option<SyncSender<DeliveryJob>>,
    cancelled: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DeliveryWorker {
    fn shutdown(mut self) {
        self.stop();
    }

    fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.sender.take();
    }

    fn stop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("PTY delivery worker panicked during shutdown");
        }
    }
}

impl Drop for DeliveryWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn workers() -> &'static Mutex<HashMap<Key, DeliveryWorker>> {
    static WORKERS: OnceLock<Mutex<HashMap<Key, DeliveryWorker>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(all(test, unix))]
pub(super) fn worker_cancellation(group_id: &str, actor_id: &str) -> Option<Arc<AtomicBool>> {
    workers()
        .lock()
        .expect("delivery workers")
        .get(&(group_id.to_owned(), actor_id.to_owned()))
        .map(|worker| Arc::clone(&worker.cancelled))
}

fn completions() -> &'static Mutex<VecDeque<DeliveryCompletion>> {
    static COMPLETIONS: OnceLock<Mutex<VecDeque<DeliveryCompletion>>> = OnceLock::new();
    COMPLETIONS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn in_flight() -> &'static Mutex<HashSet<(String, String, String)>> {
    static IN_FLIGHT: OnceLock<Mutex<HashSet<(String, String, String)>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn record_completion(completion: DeliveryCompletion) {
    if let Ok(mut queue) = completions().lock() {
        if queue.len() >= COMPLETION_CAPACITY {
            if let Some(dropped) = queue.pop_front() {
                clear_in_flight(|item| {
                    item.0 == dropped.group_id
                        && item.1 == dropped.actor_id
                        && item.2 == dropped.event_id
                });
            }
            tracing::warn!("PTY delivery completion queue reached capacity");
        }
        queue.push_back(completion);
    }
}

pub(super) fn complete_job(job: &DeliveryJob) {
    let transport = delivery_transport(&job.home, &job.group, &job.actor);
    match crate::ops::runtime_delivery::append_state(
        &job.home,
        &job.group.group_id,
        &job.actor.id,
        &job.actor.created_at,
        &job.event.id,
        transport,
        crate::ops::runtime_delivery::DeliveryOutcome::Accepted,
    ) {
        Ok(_) => release_in_flight(job),
        Err(error) => {
            tracing::warn!(
                message = %error.message,
                event_id = %job.event.id,
                "runtime accepted delivery but its ledger result is pending"
            );
            record_completion(DeliveryCompletion {
                group_id: job.group.group_id.clone(),
                actor_id: job.actor.id.clone(),
                actor_created_at: job.actor.created_at.clone(),
                event_id: job.event.id.clone(),
                transport: transport.into(),
                withdrawn_reason: None,
            });
        }
    }
}

fn clear_in_flight(mut remove: impl FnMut(&(String, String, String)) -> bool) {
    if let Ok(mut pending) = in_flight().lock() {
        pending.retain(|item| !remove(item));
    }
}

pub(super) fn release_in_flight(job: &DeliveryJob) {
    if let Ok(mut pending) = in_flight().lock() {
        pending.remove(&(
            job.group.group_id.clone(),
            job.actor.id.clone(),
            job.event.id.clone(),
        ));
    }
}

fn fail_job(job: &DeliveryJob, reason: &str) {
    if let Err(error) = crate::ops::runtime_delivery::append_state(
        &job.home,
        &job.group.group_id,
        &job.actor.id,
        &job.actor.created_at,
        &job.event.id,
        delivery_transport(&job.home, &job.group, &job.actor),
        crate::ops::runtime_delivery::DeliveryOutcome::Failed(reason),
    ) {
        tracing::warn!(
            message = %error.message,
            event_id = %job.event.id,
            "failed to record interrupted runtime delivery"
        );
    }
    release_in_flight(job);
}

/// Close deliveries that were never sent and must not be retried. A failed
/// record is queued like an accepted one: only the record is retried, never
/// the submission, and the event keeps its in-flight slot meanwhile.
pub(super) fn withdraw_jobs(jobs: &[DeliveryJob], reason: &str) {
    for job in jobs {
        let transport = delivery_transport(&job.home, &job.group, &job.actor);
        match crate::ops::runtime_delivery::append_state(
            &job.home,
            &job.group.group_id,
            &job.actor.id,
            &job.actor.created_at,
            &job.event.id,
            transport,
            crate::ops::runtime_delivery::DeliveryOutcome::Withdrawn(reason),
        ) {
            Ok(_) => release_in_flight(job),
            Err(error) => {
                tracing::warn!(message = %error.message, event_id = %job.event.id, "withdrawn delivery record is pending");
                record_completion(DeliveryCompletion {
                    group_id: job.group.group_id.clone(),
                    actor_id: job.actor.id.clone(),
                    actor_created_at: job.actor.created_at.clone(),
                    event_id: job.event.id.clone(),
                    transport: transport.into(),
                    withdrawn_reason: Some(reason.into()),
                });
            }
        }
    }
}

pub(super) fn fail_jobs(jobs: &[DeliveryJob], reason: &str) {
    for job in jobs {
        fail_job(job, reason);
    }
}

/// Settle a batch the runtime refused `attempts` times: every job Failed, one
/// error event for the message, and (if this batch exhausted the breaker)
/// one circuit-open event. Returns whether the breaker opened.
fn settle_rejected_batch(batch: &[DeliveryJob], reason: &str, attempts: u32) -> bool {
    fail_jobs(batch, reason);
    let Some(head) = batch.first() else {
        return false;
    };
    surface_delivery_error(
        &head.home,
        &head.group,
        &head.actor,
        &format!(
            "delivery-failed:{}:{}:{}",
            head.group.group_id, head.actor.id, head.event.id
        ),
        "Message could not be delivered",
        &format!(
            "Gave up on message {} after {} rejected attempts ({reason}). It is recorded as failed and will not be retried.",
            head.event.id, attempts
        ),
    );
    let key = (head.group.group_id.clone(), head.actor.id.clone());
    let opened = breaker_failed(&key);
    if opened {
        surface_delivery_error(
            &head.home,
            &head.group,
            &head.actor,
            &format!(
                "delivery-circuit-open:{}:{}",
                head.group.group_id, head.actor.id
            ),
            "Delivery paused for this actor",
            &format!(
                "{} messages in a row could not be delivered; new deliveries are refused for {} seconds before one more attempt is made.",
                DELIVERY_BREAKER_FAILURES,
                DELIVERY_BREAKER_COOLDOWN.as_secs()
            ),
        );
    }
    opened
}

pub fn dispatch(home: &HomeLayout, group: &GroupDoc, event: &Event) -> DispatchReport {
    if !matches!(event.kind.as_str(), "chat.message" | "system.notify") {
        return report(0, 0, 0);
    }

    // A memory-restart notice remains addressed to its foreman while the group
    // is stopped. Let dispatch select that target; dispatch_to_inner reports it
    // without enqueueing work for a paused or stopped group.
    let is_memory_restart = event.kind == "system.notify"
        && event.data.get("kind").and_then(serde_json::Value::as_str) == Some("memory_restart");
    if matches!(group.state, GroupState::Paused | GroupState::Stopped) && !is_memory_restart {
        return report(0, 0, 0);
    }

    if event.kind == "chat.message"
        && event
            .data
            .get("message_mode")
            .and_then(serde_json::Value::as_str)
            == Some("mail")
    {
        return mail_report();
    }

    let targets: Vec<_> = group
        .actors
        .iter()
        .filter(|actor| {
            (!crate::ops::actor_runtime::is_structured(actor)
                || crate::ops::local_headless::supports(actor)
                || actor.runtime == ActorRuntime::Deepseek)
                && event_targets_actor(group, event, &actor.id)
        })
        .cloned()
        .collect();
    dispatch_to(home, group, event, &targets, false)
}

fn event_targets_actor(group: &GroupDoc, event: &Event, actor_id: &str) -> bool {
    if event.kind == "system.notify"
        && event.data.get("kind").and_then(serde_json::Value::as_str) == Some("memory_restart")
    {
        return event
            .data
            .get("target_actor_id")
            .and_then(serde_json::Value::as_str)
            == Some(actor_id);
    }
    if event.kind == "system.notify"
        && matches!(
            event.data.get("kind").and_then(serde_json::Value::as_str),
            Some("mail_notice" | "reply_notice" | "task_notice")
        )
    {
        return event
            .data
            .get("target_actor_id")
            .and_then(serde_json::Value::as_str)
            == Some(actor_id);
    }
    inbox::is_for_actor(group, event, actor_id)
}

pub fn dispatch_to(
    home: &HomeLayout,
    group: &GroupDoc,
    event: &Event,
    targets: &[Actor],
    force_ambiguous: bool,
) -> DispatchReport {
    dispatch_to_inner(home, group, event, targets, force_ambiguous, false)
}

pub fn dispatch_preclaimed(
    home: &HomeLayout,
    group: &GroupDoc,
    event: &Event,
    targets: &[Actor],
) -> DispatchReport {
    dispatch_to_inner(home, group, event, targets, false, true)
}

fn dispatch_to_inner(
    home: &HomeLayout,
    group: &GroupDoc,
    event: &Event,
    targets: &[Actor],
    force_ambiguous: bool,
    preclaimed: bool,
) -> DispatchReport {
    if matches!(group.state, GroupState::Paused | GroupState::Stopped) {
        return report(targets.len(), 0, 0);
    }
    let mut queued = 0;
    let mut online = 0;
    for actor in targets {
        if !actor.enabled {
            continue;
        }
        let actor_online = if actor.runtime == ActorRuntime::Deepseek {
            crate::ops::deepseek_runtime::running(&group.group_id, &actor.id)
        } else if crate::ops::local_headless::uses_managed_delivery(&group.group_id, actor) {
            crate::ops::local_headless::running(&group.group_id, &actor.id)
        } else {
            cccc_runtime::status(&group.group_id, &actor.id).is_ok_and(|status| status.running)
        };
        if actor_online {
            online += 1;
        }
        let transport = delivery_transport(home, group, actor);
        if preclaimed && actor.runtime.is_web_model() {
            // Structured Web Model consumers take the durable claim through
            // runtime_wait_next_turn. Do not enqueue the actor on the PTY lane.
            queued += 1;
            continue;
        }
        if !preclaimed {
            match crate::ops::runtime_delivery::claim(
                home,
                group,
                actor,
                &event.id,
                transport,
                force_ambiguous,
            ) {
                Ok(crate::ops::runtime_delivery::ClaimResult::Claimed) => {}
                Ok(crate::ops::runtime_delivery::ClaimResult::Terminal(_)) => continue,
                Err(error) => {
                    tracing::warn!(
                        group_id = %group.group_id,
                        actor_id = %actor.id,
                        event_id = %event.id,
                        message = %error.message,
                        "failed to claim runtime delivery"
                    );
                    continue;
                }
            }
        }
        if enqueue(DeliveryJob {
            home: home.clone(),
            group: group.clone(),
            actor: actor.clone(),
            event: event.clone(),
        }) {
            queued += 1;
        } else if let Err(error) = crate::ops::runtime_delivery::append_state(
            home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            &event.id,
            transport,
            crate::ops::runtime_delivery::DeliveryOutcome::Failed(
                "daemon delivery queue did not accept the event",
            ),
        ) {
            tracing::warn!(message = %error.message, "failed to record rejected runtime delivery");
        }
    }
    report(targets.len(), online, queued)
}

pub fn dispatch_unread(home: &HomeLayout, group: &GroupDoc, actor_id: &str) -> usize {
    if matches!(group.state, GroupState::Paused | GroupState::Stopped) {
        return 0;
    }
    let Some(actor) = group.actors.iter().find(|actor| actor.id == actor_id) else {
        return 0;
    };
    if !actor.enabled
        || (crate::ops::actor_runtime::is_structured(actor)
            && !crate::ops::local_headless::supports(actor)
            && actor.runtime != ActorRuntime::Deepseek)
    {
        return 0;
    }
    let events = match crate::ops::runtime_delivery::pending_sources(
        home,
        group,
        actor,
        QUEUE_CAPACITY,
    ) {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(message = %error.message, %actor_id, "failed to load pending runtime deliveries");
            return 0;
        }
    };
    events
        .into_iter()
        .map(|event| dispatch_to(home, group, &event, std::slice::from_ref(actor), false).queued)
        .sum()
}

pub fn dispatch_group_unread(home: &HomeLayout, group: &GroupDoc) -> usize {
    group
        .actors
        .iter()
        .map(|actor| dispatch_unread(home, group, &actor.id))
        .sum()
}

pub fn mail_report() -> DispatchReport {
    DispatchReport {
        accepted: true,
        state: "mail",
        targeted: 0,
        online: 0,
        queued: 0,
    }
}

pub(super) fn delivery_transport(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
) -> &'static str {
    if actor.runtime == ActorRuntime::Deepseek {
        "deepseek"
    } else if actor.runtime == ActorRuntime::ChatgptDots {
        "web_model_pull"
    } else if actor.runtime.is_web_model() {
        web_model_delivery_transport(home, group, actor)
    } else if crate::ops::local_headless::uses_managed_delivery(&group.group_id, actor) {
        "managed_session"
    } else {
        "pty"
    }
}

fn web_model_delivery_transport(
    home: &HomeLayout,
    _group: &GroupDoc,
    actor: &Actor,
) -> &'static str {
    let setting = |names: &[&str]| {
        names
            .iter()
            .filter_map(|name| actor.env.get(*name))
            .map(|value| value.trim().to_ascii_lowercase())
            .find(|value| !value.is_empty())
            .or_else(|| {
                names
                    .iter()
                    .filter_map(|name| std::env::var(name).ok())
                    .map(|value| value.trim().to_ascii_lowercase())
                    .find(|value| !value.is_empty())
            })
            .unwrap_or_default()
    };
    if actor.runtime == ActorRuntime::GrokWebModel {
        return "web_model_browser";
    }
    let mode = setting(&["CCCC_WEB_MODEL_DELIVERY_MODE", "CCCC_WEB_MODEL_DELIVERY"]);
    if matches!(
        mode.as_str(),
        "pull" | "native" | "remote_mcp" | "off" | "disabled" | "none"
    ) {
        return "web_model_pull";
    }
    if matches!(
        mode.as_str(),
        "browser" | "chatgpt" | "chatgpt_browser" | "browser_delivery"
    ) {
        return "web_model_browser";
    }
    let mut provider = setting(&["CCCC_WEB_MODEL_PROVIDER", "CCCC_WEB_MODEL_BROWSER_PROVIDER"]);
    if provider.is_empty() {
        provider = cccc_core::web_model_connectors::load(home)
            .unwrap_or_default()
            .into_iter()
            .find(|connector| {
                connector["revoked"] != true && connector["provider"] == "chatgpt_web"
            })
            .and_then(|connector| connector["provider"].as_str().map(str::to_owned))
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default();
    }
    if matches!(
        provider.as_str(),
        "chatgpt" | "chatgpt_web" | "browser_web_model" | "chatgpt_browser"
    ) {
        "web_model_browser"
    } else {
        "web_model_pull"
    }
}

fn report(targeted: usize, online: usize, queued: usize) -> DispatchReport {
    let state = if queued > 0 {
        "queued"
    } else if online > 0 {
        "queue_full"
    } else if targeted > 0 {
        "blocked"
    } else {
        "no_recipients"
    };
    DispatchReport {
        accepted: true,
        state,
        targeted,
        online,
        queued,
    }
}

fn enqueue(job: DeliveryJob) -> bool {
    let key = (job.group.group_id.clone(), job.actor.id.clone());
    let delivery_key = (key.0.clone(), key.1.clone(), job.event.id.clone());
    let reserved = in_flight()
        .lock()
        .map(|mut pending| pending.insert(delivery_key.clone()))
        .unwrap_or(false);
    if !reserved {
        return false;
    }
    let mut map = match workers().lock() {
        Ok(map) => map,
        Err(_) => {
            clear_in_flight(|item| item == &delivery_key);
            return false;
        }
    };
    let worker = map.entry(key.clone()).or_insert_with(|| spawn_worker(&key));
    let Some(sender) = worker.sender.as_ref() else {
        clear_in_flight(|item| item == &delivery_key);
        return false;
    };
    match sender.try_send(job) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            clear_in_flight(|item| item == &delivery_key);
            tracing::warn!(group_id = %key.0, actor_id = %key.1, "PTY delivery queue is full");
            false
        }
        Err(TrySendError::Disconnected(job)) => {
            let worker = spawn_worker(&key);
            let result = worker
                .sender
                .as_ref()
                .is_some_and(|sender| sender.try_send(job).is_ok());
            let stale = map.insert(key, worker);
            drop(map);
            if let Some(stale) = stale {
                stale.shutdown();
            }
            if !result {
                clear_in_flight(|item| item == &delivery_key);
            }
            result
        }
    }
}

fn spawn_worker(key: &Key) -> DeliveryWorker {
    let rejection_attempts = DELIVERY_REJECTION_ATTEMPTS;
    let (sender, receiver) = mpsc::sync_channel::<DeliveryJob>(QUEUE_CAPACITY);
    let name = format!("cccc-delivery:{}:{}", key.0, key.1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let thread_cancelled = Arc::clone(&cancelled);
    let key = (*key).clone();
    let thread = std::thread::Builder::new().name(name).spawn(move || {
        let mut preamble_session = String::new();
        let mut deferred = Vec::new();
        let mut deferred_failures: u32 = 0;
        let mut rejected_attempts: u32 = 0;
        while !thread_cancelled.load(Ordering::Acquire) {
            let mut batch = if deferred.is_empty() {
                let Ok(job) = receiver.recv() else {
                    break;
                };
                vec![job]
            } else if deferred.len() >= BATCH_CAPACITY {
                // Failed startup or paused delivery must not grow this batch
                // beyond its limit by draining the bounded channel on retries.
                if !actor_delivery_worker::interruptible_sleep(
                    deferred_retry_delay(deferred_failures),
                    &thread_cancelled,
                ) {
                    break;
                }
                std::mem::take(&mut deferred)
            } else {
                match receiver.recv_timeout(deferred_retry_delay(deferred_failures)) {
                    Ok(job) => {
                        let mut batch = std::mem::take(&mut deferred);
                        batch.push(job);
                        batch
                    }
                    Err(RecvTimeoutError::Timeout) => std::mem::take(&mut deferred),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            if batch[0].actor.runtime != ActorRuntime::Custom
                && batch[0].actor.runtime != ActorRuntime::Deepseek
                && !crate::ops::local_headless::supports(&batch[0].actor)
            {
                while batch.len() < BATCH_CAPACITY {
                    match receiver.try_recv() {
                        Ok(job) => batch.push(job),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }
            if batch.is_empty() {
                continue;
            }
            // An open breaker refuses work before any runtime call: the actor
            // is the fault, so re-offering the same broken session only
            // multiplies the damage the retry cap just stopped.
            if breaker_blocks(&key) {
                let reason = format!(
                    "delivery circuit open for {} after {} consecutive failed messages",
                    key.1, DELIVERY_BREAKER_FAILURES
                );
                fail_jobs(&batch, &reason);
                deferred.clear();
                deferred_failures = 0;
                continue;
            }
            let mut delivered = false;
            let mut terminal_reason = None;
            let mut rejection: Option<String> = None;
            for attempt in 0..DELIVERY_ATTEMPTS_PER_ROUND {
                match actor_delivery_worker::process_batch(
                    &mut batch,
                    &mut preamble_session,
                    &thread_cancelled,
                ) {
                    actor_delivery_worker::BatchOutcome::Delivered => {
                        delivered = true;
                        break;
                    }
                    actor_delivery_worker::BatchOutcome::Terminal(reason) => {
                        terminal_reason = Some(reason);
                        break;
                    }
                    // The runtime was asked and refused this batch. That is a
                    // fault, not a queue position, so it is counted, and the
                    // inner transient-retry loop stops here: the rejection
                    // budget counts real submissions, not probes.
                    actor_delivery_worker::BatchOutcome::Rejected(reason) => {
                        rejection = Some(reason);
                        break;
                    }
                    actor_delivery_worker::BatchOutcome::Retry => rejection = None,
                }
                if thread_cancelled.load(Ordering::Acquire) {
                    break;
                }
                if !actor_delivery_worker::interruptible_sleep(
                    std::time::Duration::from_millis(250 * (attempt + 1)),
                    &thread_cancelled,
                ) {
                    break;
                }
            }
            if let Some(reason) = terminal_reason {
                fail_jobs(&batch, &reason);
                deferred_failures = 0;
                rejected_attempts = 0;
            } else if delivered {
                breaker_delivered(&key);
                deferred_failures = 0;
                rejected_attempts = 0;
            } else if let Some(reason) = rejection {
                rejected_attempts = rejected_attempts.saturating_add(1);
                if rejected_attempts >= rejection_attempts {
                    // Owner spec (dd17cbba item 3): a batch the runtime keeps
                    // refusing is retried a bounded number of times, then
                    // settled Failed with exactly one surfaced error. The old
                    // behaviour re-offered it for ever.
                    if batch.is_empty() {
                        rejected_attempts = 0;
                        continue;
                    }
                    let reason = format!(
                        "runtime rejected delivery {} times: {reason}",
                        rejected_attempts
                    );
                    settle_rejected_batch(&batch, &reason, rejected_attempts);
                    deferred.clear();
                    deferred_failures = 0;
                    rejected_attempts = 0;
                } else {
                    if !actor_delivery_worker::interruptible_sleep(
                        rejection_backoff(rejected_attempts),
                        &thread_cancelled,
                    ) {
                        break;
                    }
                    deferred = batch;
                }
            } else {
                // Not ready yet (actor starting, turn in flight, group paused):
                // this is a queue position, not a retry of a failing call, so it
                // is held with the existing bounded deferred delay.
                deferred_failures = deferred_failures.saturating_add(1);
                rejected_attempts = 0;
                deferred = batch;
            }
        }
        fail_jobs(
            &deferred,
            "delivery worker stopped before runtime acceptance",
        );
        for job in receiver.try_iter() {
            fail_job(&job, "delivery worker stopped before runtime acceptance");
        }
    });
    let thread = match thread {
        Ok(thread) => Some(thread),
        Err(error) => {
            tracing::warn!(%error, "failed to start actor delivery worker");
            None
        }
    };
    DeliveryWorker {
        sender: thread.as_ref().map(|_| sender),
        cancelled,
        thread,
    }
}

fn deferred_retry_delay(failures: u32) -> std::time::Duration {
    let exponent = failures.saturating_sub(1).min(4);
    let delay = std::time::Duration::from_millis(250 * (1_u64 << exponent));
    delay.min(DEFERRED_RETRY_MAX)
}

/// Per-actor delivery breaker state. `failures` counts consecutive batches
/// that exhausted their retry budget; `open_until` is set while the breaker
/// refuses new work.
#[derive(Default)]
struct DeliveryBreaker {
    failures: u32,
    open_until: Option<Instant>,
}

fn breakers() -> &'static Mutex<HashMap<Key, DeliveryBreaker>> {
    static BREAKERS: OnceLock<Mutex<HashMap<Key, DeliveryBreaker>>> = OnceLock::new();
    BREAKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True while the actor's breaker refuses new work. A breaker whose cooldown
/// has elapsed half-opens: the next batch is attempted, and a delivery closes
/// it again.
fn breaker_blocks(key: &Key) -> bool {
    let Ok(mut map) = breakers().lock() else {
        return false;
    };
    let Some(state) = map.get_mut(key) else {
        return false;
    };
    match state.open_until {
        Some(until) if until > Instant::now() => true,
        Some(_) => {
            state.open_until = None;
            false
        }
        None => false,
    }
}

/// Record one batch that exhausted its retries. Returns true when this
/// failure opened the breaker, so the caller surfaces exactly one event per
/// open window.
fn breaker_failed(key: &Key) -> bool {
    let Ok(mut map) = breakers().lock() else {
        return false;
    };
    let state = map.entry(key.clone()).or_default();
    if state.open_until.is_some() {
        return false;
    }
    state.failures = state.failures.saturating_add(1);
    if state.failures < DELIVERY_BREAKER_FAILURES {
        return false;
    }
    state.failures = 0;
    state.open_until = Some(Instant::now() + DELIVERY_BREAKER_COOLDOWN);
    true
}

fn breaker_delivered(key: &Key) {
    if let Ok(mut map) = breakers().lock() {
        map.remove(key);
    }
}

/// Record one human-visible delivery failure in the Group ledger as the single
/// event the owner asked for ("deliver one error event to the actor and
/// stop"). `client_id` makes the notice idempotent, so a replayed worker never
/// duplicates it.
fn surface_delivery_error(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    client_id: &str,
    title: &str,
    message: &str,
) {
    let Ok(store) = GroupStore::new(home.clone()) else {
        return;
    };
    let Ok(path) = store.ledger_path(&group.group_id) else {
        return;
    };
    if matches!(
        cccc_core::ledger::find_idempotent(&path, "system.notify", "system", client_id),
        Ok(Some(_))
    ) {
        return;
    }
    let mut notice = Event::new("system.notify", &group.group_id);
    notice.by = "system".into();
    notice.scope_key = group.active_scope_key.clone();
    notice.data = Map::from_iter([
        ("client_id".to_owned(), json!(client_id)),
        ("kind".to_owned(), json!("error")),
        ("target_actor_id".to_owned(), json!(actor.id)),
        ("im_visibility".to_owned(), json!("internal")),
        ("title".to_owned(), json!(title)),
        ("message".to_owned(), json!(message)),
    ]);
    if let Err(error) = cccc_core::ledger::append(&path, &notice) {
        tracing::warn!(
            %error,
            group_id = %group.group_id,
            actor_id = %actor.id,
            "failed to surface delivery error"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scopeless_worker_records_failure_and_releases_claim() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("scopeless worker", "").expect("group");
        let actor = Actor::new("peer1");
        group.actors.push(actor.clone());
        store.save(&group).expect("save group");
        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "user".into();
        event.data = json!({"to":["peer1"],"text":"work","message_mode":"send"})
            .as_object()
            .expect("data")
            .clone();
        let job = DeliveryJob {
            home: home.clone(),
            group: group.clone(),
            actor: actor.clone(),
            event: event.clone(),
        };
        let key = (group.group_id.clone(), actor.id.clone(), event.id.clone());
        in_flight().lock().expect("claims").insert(key.clone());
        crate::ops::runtime_delivery::append_state(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            &event.id,
            delivery_transport(&home, &group, &actor),
            crate::ops::runtime_delivery::DeliveryOutcome::Claimed,
        )
        .expect("claim");
        let worker = spawn_worker(&(group.group_id.clone(), actor.id.clone()));
        worker
            .sender
            .as_ref()
            .expect("sender")
            .send(job)
            .expect("enqueue");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let state = crate::ops::runtime_delivery::latest_state(
                &home,
                &group.group_id,
                &actor.id,
                &event.id,
            )
            .expect("state");
            if state.is_some_and(|(state, _)| state == "failed") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "delivery must fail without retrying forever"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        worker.shutdown();
        assert!(!in_flight().lock().expect("claims").contains(&key));
        let events =
            ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger")).expect("events");
        let states: Vec<_> = events
            .iter()
            .filter(|event| event.kind == "runtime.delivery")
            .filter_map(|event| event.data.get("state").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(states, ["claimed", "failed"]);
    }

    #[test]
    fn configured_browser_transport_does_not_require_a_completed_pairing() {
        let temp = tempfile::tempdir().expect("temp");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("init");
        let groups = GroupStore::new(home.clone()).expect("groups");
        let group = groups.create("transport", "").expect("group");
        cccc_core::web_model_connectors::configure(&home).expect("connector");
        let mut actor = Actor::new("a");
        actor.runtime = ActorRuntime::WebModel;
        assert_eq!(
            web_model_delivery_transport(&home, &group, &actor),
            "web_model_browser"
        );
        actor
            .env
            .insert("CCCC_WEB_MODEL_DELIVERY_MODE".into(), "pull".into());
        assert_eq!(
            web_model_delivery_transport(&home, &group, &actor),
            "web_model_pull"
        );
    }

    use super::*;
    use cccc_core::{GroupStore, ledger};
    use serde_json::json;

    #[test]
    fn mail_is_stored_without_runtime_queueing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("mail", "").expect("group");
        let actor = Actor::new("peer1");
        group.actors.push(actor);
        store.save(&group).expect("save actor");
        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "user".into();
        event.data = json!({"to":["peer1"],"text":"read later","message_mode":"mail"})
            .as_object()
            .cloned()
            .expect("event data");
        let report = dispatch(&home, &group, &event);
        assert_eq!(report.state, "mail");
        assert_eq!(report.queued, 0);
        assert!(
            !in_flight()
                .lock()
                .expect("in flight")
                .iter()
                .any(|item| item.0 == group.group_id)
        );
    }

    #[test]
    fn paused_direct_delivery_is_blocked_not_reported_as_mail() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("paused direct", "").expect("group");
        group.state = GroupState::Paused;
        group.actors.push(Actor::new("peer1"));
        store.save(&group).expect("save actor");
        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "user".into();
        event.data = json!({"to":["peer1"],"text":"wait","message_mode":"send"})
            .as_object()
            .cloned()
            .expect("event data");

        let report = dispatch_to(&home, &group, &event, &group.actors, false);
        assert_eq!(report.state, "blocked");
        assert_eq!(report.queued, 0);
    }

    #[test]
    fn deferred_retry_backoff_is_bounded() {
        assert_eq!(
            deferred_retry_delay(1),
            std::time::Duration::from_millis(250)
        );
        assert_eq!(deferred_retry_delay(u32::MAX), DEFERRED_RETRY_MAX);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn worker_shutdown_cancels_blocked_terminal_submission_before_runtime_stop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let group_id = format!("g_blocked_delivery_{}", std::process::id());
        let mut actor = Actor::new("peer");
        actor.runtime = ActorRuntime::Custom;
        actor.submit = ActorSubmit::None;
        cccc_runtime::start(cccc_runtime::LaunchSpec {
            group_id: group_id.clone(),
            actor_id: actor.id.clone(),
            runner: cccc_contracts::RunnerKind::Pty,
            command: vec![
                "sh".into(),
                "-c".into(),
                "stty raw -echo; touch ready; sleep 30".into(),
            ],
            cwd: temp.path().into(),
            env: Default::default(),
            cols: 80,
            rows: 24,
        })
        .expect("terminal");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !temp.path().join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancelled);
        let group = group_id.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = submit_terminal_text(&group, &actor, &"x".repeat(1024 * 1024), &cancel);
            result_tx.send(result).expect("result receiver");
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        let blocked = !thread.is_finished();
        let worker = DeliveryWorker {
            sender: None,
            cancelled,
            thread: Some(thread),
        };
        let (done_tx, done_rx) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            worker.shutdown();
            done_tx.send(()).expect("shutdown receiver");
        });
        let stopped = done_rx.recv_timeout(std::time::Duration::from_secs(1));
        cccc_runtime::stop(&group_id, "peer").expect("cleanup");
        shutdown.join().expect("shutdown thread");
        assert!(blocked, "fixture must block input before shutdown");
        stopped.expect("delivery shutdown must finish before stopping the runtime");
        assert!(!result_rx.recv().expect("cancelled submission"));
    }

    #[test]
    fn worker_shutdown_releases_claims_as_retryable_failures() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("paused delivery", "").expect("group");
        group.state = GroupState::Paused;
        let actor = Actor::new("peer1");
        group.actors.push(actor.clone());
        store.save(&group).expect("save actor");

        let mut event = Event::new("chat.message", &group.group_id);
        event.by = "user".into();
        event.data = json!({"to":["peer1"],"text":"retry me","message_mode":"send"})
            .as_object()
            .cloned()
            .expect("event data");
        ledger::append(
            &store.ledger_path(&group.group_id).expect("ledger path"),
            &event,
        )
        .expect("append source");
        crate::ops::runtime_delivery::append_state(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            &event.id,
            delivery_transport(&home, &group, &actor),
            crate::ops::runtime_delivery::DeliveryOutcome::Claimed,
        )
        .expect("claim");

        assert!(enqueue(DeliveryJob {
            home: home.clone(),
            group: group.clone(),
            actor: actor.clone(),
            event: event.clone(),
        }));
        shutdown_actor(&group.group_id, &actor.id);

        assert_eq!(
            crate::ops::runtime_delivery::latest_state(
                &home,
                &group.group_id,
                &actor.id,
                &event.id,
            )
            .expect("latest state")
            .expect("delivery state")
            .0,
            "failed"
        );
        assert!(
            !in_flight()
                .lock()
                .expect("in flight")
                .iter()
                .any(|item| item.0 == group.group_id)
        );
    }

    /// Owner acceptance test for spec dd17cbba item 3, driven on a real
    /// captured cwflab message instead of synthetic text: a refused batch is
    /// settled Failed with exactly one surfaced error, and settling twice does
    /// not double the record or the notice.
    #[test]
    fn refused_message_settles_with_one_surfaced_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("retry budget", "").expect("group");
        let actor = Actor::new("peer1");
        group.actors.push(actor.clone());
        store.save(&group).expect("save group");

        let captured: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/real_cwflab_delivery_event.json"
        ))
        .expect("fixture json");
        let mut event: Event =
            serde_json::from_value(captured["event"].clone()).expect("captured event");
        event.group_id = group.group_id.clone();

        let job = DeliveryJob {
            home: home.clone(),
            group: group.clone(),
            actor: actor.clone(),
            event: event.clone(),
        };
        let key = (group.group_id.clone(), actor.id.clone(), event.id.clone());
        in_flight().lock().expect("claims").insert(key.clone());
        crate::ops::runtime_delivery::append_state(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            &event.id,
            delivery_transport(&home, &group, &actor),
            crate::ops::runtime_delivery::DeliveryOutcome::Claimed,
        )
        .expect("claim");

        settle_rejected_batch(
            std::slice::from_ref(&job),
            "runtime rejected delivery 3 times: refused by runtime",
            DELIVERY_REJECTION_ATTEMPTS,
        );
        assert!(!in_flight().lock().expect("claims").contains(&key));

        let path = store.ledger_path(&group.group_id).expect("ledger");
        let events = ledger::read_all(&path).expect("events");
        let failed_event = events
            .iter()
            .find(|event| {
                event.kind == "runtime.delivery" && event.data["state"] == json!("failed")
            })
            .expect("failed delivery record");
        let reason = failed_event.data["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("rejected") && reason.contains("refused by runtime"),
            "the refusal must be recorded as the reason: {reason}"
        );
        let notices: Vec<_> = events
            .iter()
            .filter(|event| event.kind == "system.notify")
            .collect();
        assert_eq!(notices.len(), 1, "exactly one surfaced error event");
        assert_eq!(notices[0].data["kind"], json!("error"));
        assert_eq!(notices[0].data["target_actor_id"], json!(actor.id));
        let failed = events
            .iter()
            .filter(|event| event.kind == "runtime.delivery")
            .filter(|event| event.data["state"] == json!("failed"))
            .count();
        assert_eq!(failed, 1, "the message is settled Failed once");

        // Re-settling the same message re-records the failure but never
        // surfaces a second error: the notice key is idempotent.
        settle_rejected_batch(
            std::slice::from_ref(&job),
            "runtime rejected delivery 3 times: refused by runtime",
            DELIVERY_REJECTION_ATTEMPTS,
        );
        let after = ledger::read_all(&path).expect("events after settle");
        assert_eq!(
            after
                .iter()
                .filter(|event| event.kind == "system.notify")
                .count(),
            1,
            "a settled message must not surface a second error"
        );
    }

    #[test]
    fn breaker_opens_after_three_failed_batches_and_a_delivery_closes_it() {
        let key = ("g_breaker".to_owned(), "peer".to_owned());
        assert!(!breaker_blocks(&key));
        assert!(!breaker_failed(&key));
        assert!(!breaker_failed(&key));
        assert!(
            breaker_failed(&key),
            "the third consecutive failed batch opens the breaker"
        );
        assert!(breaker_blocks(&key));
        breaker_delivered(&key);
        assert!(!breaker_blocks(&key), "a delivery closes the breaker");
    }

    /// Owner spec item 3: three attempts, exponential backoff, then stop.
    #[test]
    fn rejection_backoff_is_exponential_and_bounded() {
        assert_eq!(DELIVERY_REJECTION_ATTEMPTS, 3);
        assert_eq!(rejection_backoff(1), std::time::Duration::from_millis(250));
        assert_eq!(rejection_backoff(2), std::time::Duration::from_millis(500));
        assert_eq!(
            rejection_backoff(3),
            std::time::Duration::from_millis(1_000)
        );
        assert_eq!(
            rejection_backoff(u32::MAX),
            std::time::Duration::from_millis(2_000)
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "actor_delivery_start_continuity_tests.rs"]
mod start_continuity_tests;
