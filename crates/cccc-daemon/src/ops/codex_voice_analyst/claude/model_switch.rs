//! Apply model-only drift through Claude's native local-command handler.
use super::{Job, control, read_job_state};
use serde_json::Value;
use std::{io, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let original = read_job_state(config, job, &job.cwd)?;
    if confirmed(&original, job, model)? {
        return Ok(());
    }
    if !owned_launch {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Claude model switch could not be confirmed: resumed UI is not exclusively owned",
        ));
    }
    let target_label = super::cache_confirmation::label(model, job.cli_version);
    let mut screen = super::cache_confirmation::Screen::default();
    let mut confirmed_dialog = false;
    let mut stream = control::model_input(endpoint, &job.short, model, || {
        confirmed(&read_job_state(config, job, &job.cwd)?, job, model).map(|_| ())
    })
    .await?;
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(10);
    let mut output = [0_u8; 8192];
    loop {
        let state = read_job_state(config, job, &job.cwd)?;
        if confirmed(&state, job, model)? {
            return Ok(());
        }
        if !confirmed_dialog && target_label.is_some_and(|label| screen.cache_cost(label)) {
            // The current complete native repaint identifies the cache-cost
            // modal and selected requested model. Hooks/drafts never qualify.
            if confirmed(&read_job_state(config, job, &job.cwd)?, job, model)? {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            attachment.get_mut().write_all(b"\x1b_cc-d-imark;{\"kind\":\"prompt_idle\",\"nonce\":\"0123456789abcdef0123456789abcdef\"}\x1b\\").await.expect("nonce marker");
            attachment
                .get_mut()
                .write_all(&frame("❯\r\n⏵⏵ bypass permissions on (shift+tab to cycle)"))
                .await
                .expect("empty prompt");
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
            assert!(result.is_err());
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
}
