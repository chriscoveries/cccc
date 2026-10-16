//! Apply model-only drift through Claude's native local-command handler.
use super::{Job, control, read_job_state};
use serde_json::{Value, json};
use std::{io, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A compatibility decision made before native model input, not a failed
/// provider resume. Only the direct type permits the caller's fresh fallback;
/// a cleanup failure wrapping it must never permit another launch.
#[derive(Debug)]
struct RequiresFresh {
    reason: &'static str,
    transition_id: Option<String>,
}
impl std::fmt::Display for RequiresFresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Claude model configuration requires a fresh session: {}",
            self.reason
        )
    }
}
impl std::error::Error for RequiresFresh {}

pub(super) fn requires_fresh(reason: &'static str) -> io::Error {
    io::Error::other(RequiresFresh {
        reason,
        transition_id: None,
    })
}
pub(super) fn requires_fresh_in_transition(reason: &'static str, transition_id: &str) -> io::Error {
    io::Error::other(RequiresFresh {
        reason,
        transition_id: Some(transition_id.to_owned()),
    })
}
pub(super) fn fresh_transition_id(error: &io::Error) -> Option<&str> {
    error
        .get_ref()?
        .downcast_ref::<RequiresFresh>()?
        .transition_id
        .as_deref()
}
pub(super) fn is_requires_fresh(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<RequiresFresh>())
}
pub(super) fn require_adapter(model: &str, version: super::command::Version) -> io::Result<()> {
    let model = if model.is_empty() { "default" } else { model };
    control::validate_model(model)?;
    if super::cache_confirmation::label(model, version).is_none() {
        return Err(requires_fresh(
            "native model dialog compatibility is unknown",
        ));
    }
    Ok(())
}

fn confirmed(state: &Value, job: &Job, model: &str) -> io::Result<bool> {
    if state.get("resumeSessionId").and_then(Value::as_str) != Some(job.session_id.as_str()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Claude model switch could not be confirmed: resume identity changed",
        ));
    }
    let cwd = state.get("cwd").and_then(Value::as_str).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Claude model switch could not be confirmed: workspace missing",
        )
    })?;
    if Path::new(cwd).canonicalize()? != job.cwd.canonicalize()? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Claude model switch could not be confirmed: workspace changed",
        ));
    }
    let Some(flags) = state.get("respawnFlags").and_then(Value::as_array) else {
        return Ok(false);
    };
    let actual = flags.iter().enumerate().find_map(|(index, flag)| {
        let flag = flag.as_str()?;
        if flag == "--model" || flag == "-m" {
            flags.get(index + 1)?.as_str()
        } else {
            flag.strip_prefix("--model=")
        }
    });
    Ok(actual == Some(model) || model == "default" && actual.is_none())
}

pub(super) async fn apply(
    config: &Path,
    endpoint: &control::Endpoint,
    job: &Job,
    model: &str,
    owned_launch: bool,
) -> io::Result<()> {
    let model = if model.is_empty() { "default" } else { model };
    control::validate_model(model)?;
    let transition_id = uuid::Uuid::new_v4().simple().to_string();
    super::model_trace::emit(
        "claude_model_transition_started",
        json!({"transition_id":transition_id,"short":job.short,"owned_launch":owned_launch}),
    );
    let original = read_job_state(config, job, &job.cwd)?;
    if confirmed(&original, job, model)? {
        super::model_trace::emit(
            "claude_model_persisted_confirmed",
            json!({"transition_id":transition_id,"short":job.short,"configured_model_confirmed":true,"model_input_sent":false}),
        );
        return Ok(());
    }
    if !owned_launch {
        return Err(requires_fresh(
            "surviving model drift has no exclusive UI owner",
        ));
    }
    require_adapter(model, job.cli_version)?;
    let target_label = super::cache_confirmation::label(model, job.cli_version);
    let mut screen = super::cache_confirmation::Screen::with_trace("post_input", &transition_id);
    let mut confirmed_dialog = false;
    let mut stream = control::model_input_with_trace(
        endpoint,
        &job.short,
        model,
        || confirmed(&read_job_state(config, job, &job.cwd)?, job, model).map(|_| ()),
        &transition_id,
    )
    .await?;
    let result: io::Result<()> = async {
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(10);
    let mut output = [0_u8; 8192];
    loop {
        let state = read_job_state(config, job, &job.cwd)?;
        if confirmed(&state, job, model)? {
            super::model_trace::emit("claude_model_persisted_confirmed", json!({"transition_id":transition_id,"short":job.short,"configured_model_confirmed":true,"model_input_sent":true}));
            return Ok(());
        }
        if !confirmed_dialog && target_label.is_some_and(|label| screen.cache_cost(label)) {
            // The current complete native repaint identifies the cache-cost
            // modal and selected requested model. Hooks/drafts never qualify.
            if confirmed(&read_job_state(config, job, &job.cwd)?, job, model)? {
                super::model_trace::emit("claude_model_persisted_confirmed", json!({"transition_id":transition_id,"short":job.short,"configured_model_confirmed":true,"model_input_sent":true}));
                return Ok(());
            }
            stream.write_all(b"\r").await?;
            stream.flush().await?;
            confirmed_dialog = true;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Claude model switch could not be confirmed: persisted model did not change",
            ));
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
            read = stream.read(&mut output) => {
                let count = read?;
                screen.feed(&output[..count])?;
                if count == 0 { return Err(io::Error::new(io::ErrorKind::BrokenPipe,
                    "Claude model switch could not be confirmed: native attachment closed")); }
            }
        }
    }
    }.await;
    if result.is_err() {
        super::model_trace::emit(
            "claude_model_post_input_failed",
            json!({"transition_id":transition_id,"short":job.short,"model_input_sent":true,"fresh_fallback_allowed":false}),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_adapter_is_a_pre_input_fresh_decision() {
        for (model, version) in [
            ("opus", (2, 1, 259)),
            ("opus", (2, 1, 261)),
            ("opus", (2, 1, 294)),
            ("haiku", (2, 1, 293)),
            ("default", (2, 1, 293)),
        ] {
            let error = require_adapter(model, version).expect_err("unproven adapter");
            assert!(is_requires_fresh(&error));
        }
        assert!(require_adapter("opus", (2, 1, 293)).is_ok());
        assert!(!is_requires_fresh(
            &require_adapter("opus\r/resume", (2, 1, 293))
                .expect_err("invalid model input remains a real error")
        ));
    }
    #[test]
    fn cleanup_failure_never_allows_a_fresh_fallback() {
        let error = super::super::with_optional_cleanup_error(
            requires_fresh("fixture unsupported adapter"),
            Some(io::Error::other("fixture cleanup could not confirm stop")),
        );
        assert!(!is_requires_fresh(&error));
    }

    #[test]
    fn model_input_cannot_inject_terminal_commands() {
        for input in [
            "opus\r/resume",
            "opus\n",
            "opus\u{1b}",
            "opus\u{0}",
            "opus --other",
            "",
        ] {
            assert!(control::validate_model(input).is_err());
        }
        for input in ["opus", "claude-opus-5-5", "sonnet[1m]", "default"] {
            assert!(control::validate_model(input).is_ok());
        }
    }

    #[test]
    fn confirmation_requires_persisted_model_saved_identity_and_workspace() {
        let directory = tempfile::tempdir().expect("fixture operation");
        let job = Job {
            short: "01234567".into(),
            session_id: "saved".into(),
            cwd: directory.path().to_path_buf(),
            cli_version: (2, 1, 293),
        };
        let mut state = json!({"resumeSessionId":"saved", "cwd":directory.path(),
            "respawnFlags":["--model", "sonnet"]});
        assert!(!confirmed(&state, &job, "opus").expect("fixture operation"));
        state["respawnFlags"] = json!(["--model", "opus"]);
        assert!(confirmed(&state, &job, "opus").expect("fixture operation"));
        state["resumeSessionId"] = json!("different");
        assert!(confirmed(&state, &job, "opus").is_err());
        state["resumeSessionId"] = json!("saved");
        let other = tempfile::tempdir().expect("fixture operation");
        state["cwd"] = json!(other.path());
        assert!(confirmed(&state, &job, "opus").is_err());
    }
}

#[cfg(all(test, unix))]
mod wiring_tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn frame(text: &str) -> Vec<u8> {
        format!("\x1b[?2026h\x1b[2J\x1b[H{text}\x1b[?2026l").into_bytes()
    }
    const MODAL: &str = "Switch model?\r\nYour next response will be slower and use more tokens\r\nThis conversation is cached for the current model. Switching to Opus 5.5 means the full history gets re-read on\r\nyour next message.\r\n❯ 1. Yes, switch to Opus 5.5\r\n  2. No, go back";

    async fn exchange(screen: Vec<u8>, confirm: bool, change_identity: bool) {
        exchange_with_history(screen, confirm, change_identity, Vec::new()).await;
    }

    async fn exchange_with_history(
        screen: Vec<u8>,
        confirm: bool,
        change_identity: bool,
        readiness_replay: Vec<u8>,
    ) {
        let temp = tempfile::tempdir().expect("fixture home");
        let config = temp.path().to_path_buf();
        let job = Job {
            short: "01234567".into(),
            session_id: "saved".into(),
            cwd: config.clone(),
            cli_version: (2, 1, 293),
        };
        let state_path = config.join("jobs/01234567/state.json");
        std::fs::create_dir_all(state_path.parent().expect("job path")).expect("job directory");
        let state = json!({"sessionId":"saved","resumeSessionId":"saved","daemonShort":"01234567",
            "cwd":config,"respawnFlags":["--model","sonnet"]});
        std::fs::write(&state_path, serde_json::to_vec(&state).expect("state JSON"))
            .expect("native state");
        let key = config.join("control.key");
        std::fs::write(&key, "0123456789abcdef0123456789abcdef").expect("key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("key mode");
        let address = config.join("control.sock");
        let listener = tokio::net::UnixListener::bind(&address).expect("native socket");
        let endpoint = control::Endpoint::fixture(address.to_string_lossy().into_owned(), key);
        let server = tokio::spawn(async move {
            let (attachment, _) = listener.accept().await.expect("attachment");
            let mut attachment = BufReader::new(attachment);
            let mut line = String::new();
            attachment
                .read_line(&mut line)
                .await
                .expect("attach request");
            let request: Value = serde_json::from_str(&line).expect("request JSON");
            assert_eq!(request["op"], "attach");
            attachment.get_mut().write_all(b"{\"ok\":true,\"op\":\"attach\",\"imarkNonce\":\"0123456789abcdef0123456789abcdef\"}\n").await.expect("ack");
            let probes = tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.expect("has connection");
                    let mut stream = BufReader::new(stream);
                    let mut request = String::new();
                    stream.read_line(&mut request).await.expect("has request");
                    stream
                        .get_mut()
                        .write_all(b"{\"ok\":true,\"op\":\"has\",\"alive\":true,\"ready\":true}\n")
                        .await
                        .expect("has ack");
                }
            });
            attachment
                .get_mut()
                .write_all(&readiness_replay)
                .await
                .expect("old native replay");
            attachment.get_mut().write_all(b"\x1b_cc-d-imark;{\"kind\":\"prompt_idle\",\"nonce\":\"0123456789abcdef0123456789abcdef\"}\x1b\\").await.expect("nonce marker");
            attachment
                .get_mut()
                .write_all(&frame("❯\r\n⏵⏵ bypass permissions on (shift+tab to cycle)"))
                .await
                .expect("empty prompt");

            let mut command = [0_u8; 12];
            attachment
                .read_exact(&mut command)
                .await
                .expect("one model command");
            assert_eq!(&command, b"/model opus\r");
            let mut state = state;
            if change_identity {
                state["resumeSessionId"] = json!("foreign");
                std::fs::write(
                    &state_path,
                    serde_json::to_vec(&state).expect("changed JSON"),
                )
                .expect("changed identity");
            }
            attachment
                .get_mut()
                .write_all(&screen)
                .await
                .expect("native result repaint");
            let mut byte = [0_u8; 1];
            if confirm {
                tokio::time::timeout(Duration::from_secs(1), attachment.read_exact(&mut byte))
                    .await
                    .expect("targeted confirm deadline")
                    .expect("targeted confirm");
                assert_eq!(&byte, b"\r");
            }
            let extra =
                tokio::time::timeout(Duration::from_millis(250), attachment.read(&mut byte)).await;
            assert!(
                extra.is_err() || matches!(extra, Ok(Ok(0))),
                "no confirmation byte for rejected screen, no duplicate confirmation while flags lag"
            );
            if !change_identity {
                state["respawnFlags"] = json!(["--model", "opus"]);
                std::fs::write(&state_path, serde_json::to_vec(&state).expect("model JSON"))
                    .expect("persist model");
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            probes.abort();
        });
        let result = apply(&config, &endpoint, &job, "opus", true).await;
        if change_identity {
            let error = result.expect_err("changed resume identity must fail");
            assert!(
                !is_requires_fresh(&error),
                "actual attempt failure cannot start fresh"
            );
            assert!(
                super::super::resume_failure::diagnostic(&error)
                    .is_some_and(|(_, blocked)| blocked)
            );
        } else {
            result.expect("persisted requested model");
        }
        server.await.expect("native fixture");
    }

    #[tokio::test]
    async fn only_current_cache_confirmation_sends_one_enter() {
        exchange(frame(MODAL), true, false).await;
    }
    #[tokio::test]
    async fn hook_cancel_stale_and_delayed_flags_send_no_confirmation() {
        for text in [
            MODAL.replace(
                "Your next response will be slower and use more tokens",
                "A PreModelSwitch hook asked you to confirm",
            ),
            MODAL
                .replace("❯ 1. Yes", "  1. Yes")
                .replace("  2. No", "❯ 2. No"),
            "Model switched; durable flags are delayed\r\n❯".into(),
        ] {
            exchange(frame(&text), false, false).await;
        }
        let mut stale = frame(MODAL);
        stale.extend(frame("❯\r\n⏵⏵ bypass permissions on"));
        exchange(stale, false, false).await;
    }
    #[tokio::test]
    async fn identity_change_before_cache_confirmation_sends_no_enter() {
        exchange(frame(MODAL), false, true).await;
    }

    #[tokio::test]
    async fn old_native_replay_over_128k_does_not_block_model_input() {
        // The helper adds the real nonce marker and current complete empty
        // prompt after this replay. Its has responder is already running.
        exchange_with_history(frame(MODAL), true, false, vec![b'x'; 384 * 1024]).await;
    }

    #[tokio::test]
    async fn model_response_replay_over_128k_can_confirm_the_current_dialog() {
        let mut replay = vec![b'x'; 384 * 1024];
        replay.extend(frame(MODAL));
        exchange(replay, true, false).await;
    }

    #[test]
    fn opt_in_model_trace_captures_actual_eviction_and_confirmation_with_one_identity_join() {
        const TEST: &str = "ops::codex_voice_analyst::claude::model_switch::wiring_tests::opt_in_model_trace_captures_actual_eviction_and_confirmation_with_one_identity_join";
        const CHILD: &str = "CCCC_TEST_MODEL_TRACE_CAPTURE";
        const PREFIX: &str = "[cccc-claude-model] ";
        if std::env::var(CHILD).as_deref() == Ok("1") {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fixture runtime")
                .block_on(exchange_with_history(
                    frame(MODAL),
                    true,
                    false,
                    vec![b'x'; 384 * 1024],
                ));
            return;
        }
        for enabled in [true, false] {
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test binary"));
            child.args([TEST, "--exact", "--nocapture"]).env(CHILD, "1");
            if enabled {
                child.env("CCCC_CLAUDE_MODEL_TRACE", "1");
            } else {
                child.env_remove("CCCC_CLAUDE_MODEL_TRACE");
            }
            let output = child.output().expect("capture subprocess");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            let records: Vec<Value> = stderr
                .lines()
                .filter_map(|line| line.strip_prefix(PREFIX))
                .map(|line| {
                    assert!(PREFIX.len() + line.len() < 4096);
                    serde_json::from_str(line).expect("bounded metadata JSON")
                })
                .collect();
            if !enabled {
                assert!(records.is_empty(), "metadata capture is default silent");
                continue;
            }
            assert!(
                !records.is_empty(),
                "opt-in must work without a tracing subscriber"
            );
            let start = records
                .iter()
                .find(|record| record["event"] == "claude_model_transition_started")
                .expect("start metadata");
            let transition = start["transition_id"].as_str().expect("transition ID");
            assert_eq!(transition.len(), 32);
            assert_eq!(start["short"], "01234567");
            assert!(
                records
                    .iter()
                    .all(|record| record["transition_id"] == transition)
            );
            let eviction = records
                .iter()
                .find(|record| {
                    record["event"] == "claude_model_screen_evicted"
                        && record["stage"] == "pre_input"
                })
                .expect("actual pre-input screen eviction");
            assert!(eviction["received_bytes"].as_u64().expect("received") > 128 * 1024);
            assert_eq!(eviction["max_output_bytes"], 128 * 1024);
            assert!(eviction["retained_from"].as_u64().expect("evicted prefix") > 0);
            assert_eq!(eviction["anchor_known"], false);
            let recovery = records
                .iter()
                .find(|record| record["event"] == "claude_model_screen_recovered")
                .expect("reconstruction evidence");
            assert_eq!(recovery["screen_id"], eviction["screen_id"]);
            assert_eq!(recovery["complete_frame"], true);
            assert!(records.iter().any(|record| record["event"]
                == "claude_model_persisted_confirmed"
                && record["configured_model_confirmed"] == true));
            for record in &records {
                let object = record.as_object().expect("metadata object");
                for forbidden in [
                    "target_model",
                    "model",
                    "screen",
                    "prompt",
                    "account",
                    "auth",
                    "nonce",
                    "attachId",
                    "environment",
                ] {
                    assert!(
                        !object.contains_key(forbidden),
                        "metadata must omit {forbidden}"
                    );
                }
            }
            assert!(!stderr.contains("Switch model?"));
            assert!(!stderr.contains("Opus 5.5"));
            assert!(!stderr.contains("This conversation is cached"));
        }
    }
}
