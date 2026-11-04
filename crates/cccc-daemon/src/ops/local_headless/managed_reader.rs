use super::{Session, managed_runtime, output};
use serde_json::Value;
use std::io;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

pub(super) fn spawn(
    session: Arc<Session>,
    mut events: tokio::sync::broadcast::Receiver<super::super::codex_voice_analyst::AnalystEvent>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name(format!(
            "cccc-managed-agent-out:{}:{}",
            session.group_id, session.actor_id
        ))
        .spawn(move || loop {
            match managed_runtime().block_on(events.recv()) {
                Ok(event) => {
                    let disconnected = event.message.get("method").and_then(Value::as_str)
                        == Some(
                            super::super::codex_voice_analyst::MANAGED_AGENT_DISCONNECTED_METHOD,
                        );
                    if disconnected && event.message["params"]["expected"] != true {
                        let reason = event
                            .message
                            .pointer("/params/reason")
                            .and_then(Value::as_str)
                            .unwrap_or("managed Agent disconnected");
                        output::emit(
                            &session,
                            "headless.session.disconnected",
                            serde_json::Map::from_iter([(
                                "reason".into(),
                                Value::String(reason.to_owned()),
                            )]),
                        );
                    }
                    output::handle_message(&session, event.message);
                    if disconnected {
                        stop_after_provider_exit(&session);
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        group_id = %session.group_id,
                        actor_id = %session.actor_id,
                        "managed Actor event reader fell behind; stopping the unreplayable session"
                    );
                    stop_after_reader_end(&session, ReaderEnd::Lagged);
                    break;
                }
                Err(RecvError::Closed) => {
                    stop_after_reader_end(&session, ReaderEnd::Closed);
                    break;
                }
            }
        })?;
    Ok(())
}

pub(super) fn stop_after_provider_exit(session: &Session) {
    // Observe the provider before stopping it; the stop itself must not be
    // mistaken for evidence that the provider had independently exited.
    let status = session.managed.provider_exit_status();
    let reason = if status.is_some() {
        "process_exit"
    } else {
        "provider_disconnected"
    };
    let exit_code = status
        .as_ref()
        .and_then(|exit| exit.code())
        .and_then(|code| u32::try_from(code).ok());
    let exit_detail = status.map(|exit| exit.to_string());
    record_managed_stop_if_first(
        session.stop_after_process_exit(),
        &session.home,
        &session.group_id,
        &session.actor_id,
        reason,
        exit_code,
        exit_detail.as_deref(),
    );
}

#[derive(Clone, Copy)]
enum ReaderEnd {
    Lagged,
    Closed,
}

impl ReaderEnd {
    fn reason(self) -> &'static str {
        match self {
            Self::Lagged => "reader_lagged",
            Self::Closed => "reader_closed",
        }
    }
}

fn stop_after_reader_end(session: &Session, end: ReaderEnd) {
    record_managed_stop_if_first(
        session.stop_after_process_exit(),
        &session.home,
        &session.group_id,
        &session.actor_id,
        end.reason(),
        None,
        None,
    );
}

#[cfg(test)]
pub(crate) async fn verify_claude_reader_release(
    managed: Arc<super::super::codex_voice_analyst::AnalystSession>,
    corrupt_transcript: Option<&std::path::Path>,
    reject_control: &std::sync::atomic::AtomicBool,
) {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    let temp = tempfile::tempdir().expect("reader home");
    let home = cccc_core::HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = cccc_core::GroupStore::new(home.clone()).expect("store");
    let group = store.create("reader release", "").expect("group");
    store
        .mutate(&group.group_id, |doc| {
            let mut actor = cccc_contracts::Actor::new("claude-reader");
            actor.runtime = cccc_contracts::ActorRuntime::Claude;
            doc.actors.push(actor);
            Ok(())
        })
        .expect("actor");
    let session = Arc::new(Session {
        home,
        group_id: group.group_id.clone(),
        actor_id: "claude-reader".into(),
        workspace: Default::default(),
        managed: Arc::clone(&managed),
        has_terminal: AtomicBool::new(false),
        viewer: Mutex::new(None),
        status: Mutex::new(super::HeadlessStatus {
            status: "idle".into(),
            task_id: None,
            updated_at: String::new(),
            pid: None,
            reason: None,
        }),
        stopped: AtomicBool::new(false),
        stop_lock: Mutex::new(()),
        startup_prompt: Mutex::new(None),
        active_turn: Mutex::new(None),
    });
    let weak = Arc::downgrade(&session);
    let receiver = managed.subscribe();
    if let Some(path) = corrupt_transcript {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("transcript");
        file.write_all(b"not-json\n").expect("corrupt transcript");
        tokio::time::timeout(Duration::from_secs(2), async {
            while managed.process_running() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("transcript observer exits");
        // The fake provider job is still alive. A rejected control request
        // must not retire it locally or prevent a later successful stop.
        reject_control.store(true, Ordering::Release);
        let prematurely_stopped = tokio::task::spawn_blocking({
            let session = Arc::clone(&session);
            move || session.stop_after_process_exit()
        })
        .await
        .expect("failed stop task");
        reject_control.store(false, Ordering::Release);
        assert!(!prematurely_stopped);
        assert!(!session.stopped.load(Ordering::Acquire));
        assert_eq!(session.status.lock().expect("state").status, "error");
    }
    spawn(Arc::clone(&session), receiver).expect("spawn managed reader");
    if corrupt_transcript.is_some() {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.stopped.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("observer failure must actually stop the job");
    }
    let stopped = tokio::task::spawn_blocking({
        let session = Arc::clone(&session);
        move || session.stop()
    })
    .await
    .expect("stop task")
    .expect("stop session");
    assert_eq!(stopped, corrupt_transcript.is_none());
    assert!(session.stopped.load(Ordering::Acquire));
    drop(session);
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("normal close must release reader and Session while client still exists");
    // Keep the protocol sender alive throughout this assertion; channel closure
    // from dropping the client must not mask a reader leak.
    assert!(!managed.process_running());
    if corrupt_transcript.is_some() {
        let events =
            cccc_core::ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger path"))
                .expect("ledger");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "actor.stop")
                .count(),
            1
        );
    }
}

fn record_managed_stop_if_first(
    first_stop: bool,
    home: &cccc_core::HomeLayout,
    group_id: &str,
    actor_id: &str,
    reason: &str,
    exit_code: Option<u32>,
    exit_detail: Option<&str>,
) {
    if !first_stop {
        return;
    }
    if let Err(error) = super::super::actor_runtime::record_managed_stop(
        home,
        group_id,
        actor_id,
        exit_code,
        exit_detail,
        reason,
    ) {
        tracing::warn!(
            ?error,
            group_id,
            actor_id,
            "failed to record managed Actor provider exit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{ReaderEnd, record_managed_stop_if_first};
    use cccc_contracts::{Actor, ActorRuntime};
    use cccc_core::{GroupStore, HomeLayout, ledger};

    #[test]
    fn provider_disconnect_records_only_the_first_system_actor_stop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("managed exit", "").expect("group");
        store
            .mutate(&group.group_id, |document| {
                let mut actor = Actor::new("opencode-1");
                actor.runtime = ActorRuntime::Opencode;
                document.actors.push(actor);
                Ok(())
            })
            .expect("actor");

        record_managed_stop_if_first(
            true,
            &home,
            &group.group_id,
            "opencode-1",
            "provider_disconnected",
            None,
            None,
        );
        record_managed_stop_if_first(
            false,
            &home,
            &group.group_id,
            "opencode-1",
            "provider_disconnected",
            None,
            None,
        );

        let path = store.ledger_path(&group.group_id).expect("ledger path");
        let events = ledger::read_all(&path).expect("ledger");
        let stopped = events
            .iter()
            .filter(|event| event.kind == "actor.stop")
            .collect::<Vec<_>>();
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].by, "system");
        assert_eq!(stopped[0].data["actor_id"], "opencode-1");
        assert_eq!(stopped[0].data["reason"], "provider_disconnected");
        assert_eq!(stopped[0].data["source"], "managed_reader");
        assert!(stopped[0].data["exit_code"].is_null());
        assert!(!stopped[0].data.contains_key("exit_detail"));
    }

    #[test]
    fn captured_reader_stops_do_not_invent_process_exits() {
        let capture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/managed_stop_capture.json"
        ))
        .expect("captured stop events");
        let reader_stops = capture["reader_stops"].as_array().expect("reader stops");
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("managed stop capture", "").expect("group");
        store
            .mutate(&group.group_id, |document| {
                document.actors.push(Actor::new("worker"));
                Ok(())
            })
            .expect("actor");
        let reasons = [
            ReaderEnd::Closed.reason(),
            ReaderEnd::Lagged.reason(),
            "provider_disconnected",
        ];
        assert_eq!(
            reasons,
            ["reader_closed", "reader_lagged", "provider_disconnected"]
        );
        assert_eq!(reader_stops.len(), reasons.len());
        for (captured, reason) in reader_stops.iter().zip(reasons) {
            assert_eq!(captured["data"]["reason"], "process_exit");
            assert!(captured["data"]["exit_code"].is_null());
            record_managed_stop_if_first(
                true,
                &home,
                &group.group_id,
                "worker",
                reason,
                None,
                None,
            );
        }
        let actual = &capture["runtime_stop"]["data"];
        let code = actual["exit_code"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok());
        let detail = actual["exit_detail"].as_str();
        record_managed_stop_if_first(
            true,
            &home,
            &group.group_id,
            "worker",
            "process_exit",
            code,
            detail,
        );
        let events =
            ledger::read_all(&store.ledger_path(&group.group_id).expect("path")).expect("ledger");
        let stops = events
            .iter()
            .filter(|event| event.kind == "actor.stop")
            .collect::<Vec<_>>();
        assert_eq!(stops.len(), 4);
        for (stop, reason) in stops.iter().zip(reasons) {
            assert_eq!(stop.data["reason"], reason);
            assert!(stop.data["exit_code"].is_null());
        }
        assert_eq!(stops[3].data["reason"], "process_exit");
        assert_eq!(stops[3].data["exit_code"], actual["exit_code"]);
        assert_eq!(stops[3].data["exit_detail"], actual["exit_detail"]);
    }
}
