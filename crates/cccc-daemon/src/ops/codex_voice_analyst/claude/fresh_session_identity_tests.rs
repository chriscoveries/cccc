mod fresh_session_identity_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const WORKER: &str = "ca52e69a-6596-4abd-a0ec-3e8690dc70e1";
    const SAVED: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";
    const SHORT: &str = "abcdef12";
    const GENERATION: &str = "12345678abcdef0012345678abcdef00";

    struct Fixture {
        _temp: tempfile::TempDir,
        _control: ControlDirectory,
        config: PathBuf,
        workspace: PathBuf,
        executable: PathBuf,
        saved: PathBuf,
        original: Vec<u8>,
        replies: Arc<std::sync::atomic::AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl Fixture {
        fn new(pre_named: bool, force_resume: bool) -> Self {
            let temp = tempfile::tempdir().expect("fixture directory");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).expect("workspace");
            let (config, listener, control) = bind_fake_control(&temp.path().join("config"));
            std::fs::create_dir_all(config.join("projects/workspace")).expect("projects");
            std::fs::create_dir_all(config.join("jobs").join(SHORT)).expect("jobs");
            let saved = config
                .join("projects/workspace")
                .join(format!("{SAVED}.jsonl"));
            let original = format!(
                "{}\n",
                json!({"type":"custom-title", "sessionId":SAVED,
                    "customTitle":"cccc-actor-12345678"})
            )
            .into_bytes();
            std::fs::write(&saved, &original).expect("pre-named transcript");
            for (name, resumed) in [("fresh.json", false), ("resumed.json", true)] {
                cccc_core::fs::write_json(
                    &config.join(name),
                    &json!({
                        "sessionId":WORKER, "daemonShort":SHORT, "cwd":workspace,
                        "resumeSessionId":resumed.then_some(SAVED),
                        "tempo":"idle", "inFlight":{"tasks":0,"queued":0},
                        "intent":"", "outcome":null, "linkScanPath":null,
                        "linkScanOffset":0, "output":null, "tokens":null,
                    }),
                )
                .expect("startup state");
            }
            for (name, enabled) in [("pre-named", pre_named), ("force-resume", force_resume)] {
                if enabled {
                    std::fs::write(config.join(name), []).expect("fixture mode");
                }
            }
            let executable = temp.path().join("claude");
            std::fs::write(
                &executable,
                r#"#!/bin/sh
set -eu
if [ "${1-}" = '--version' ]; then
    printf '2.1.286 (Claude Code)\n'
    exit 0
fi
named=0
for arg in "$@"; do
    if [ "$arg" = '--name' ]; then named=1; fi
done
printf '%s\n' "$@" > "$CLAUDE_CONFIG_DIR/argv"
if [ -f "$CLAUDE_CONFIG_DIR/force-resume" ] ||
   { [ "$named" = 1 ] && [ -f "$CLAUDE_CONFIG_DIR/pre-named" ]; }; then
    /bin/cp "$CLAUDE_CONFIG_DIR/resumed.json" "$CLAUDE_CONFIG_DIR/jobs/abcdef12/state.json"
else
    /bin/cp "$CLAUDE_CONFIG_DIR/fresh.json" "$CLAUDE_CONFIG_DIR/jobs/abcdef12/state.json"
fi
printf 'started · abcdef12\n'
"#,
            )
            .expect("fixture executable");
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("executable permissions");
            let replies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let server_replies = Arc::clone(&replies);
            let server_config = config.clone();
            let server_workspace = workspace.clone();
            let server = tokio::spawn(async move {
                let mut stopped = false;
                loop {
                    let (stream, _) = listener.accept().await.expect("control accept");
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    stream.read_line(&mut line).await.expect("control read");
                    let request: Value = serde_json::from_str(&line).expect("control request");
                    let response = match request["op"].as_str().expect("operation") {
                        "list" => json!({"ok":true, "op":"list", "jobs":if stopped {
                            Vec::<Value>::new()
                        } else {
                            vec![json!({"short":SHORT,"sessionId":WORKER,
                                "cwd":server_workspace,"cliVersion":"2.1.286"})]
                        }}),
                        "reply" => {
                            server_replies.fetch_add(1, Ordering::SeqCst);
                            let state_path =
                                server_config.join("jobs").join(SHORT).join("state.json");
                            let mut state: Value = serde_json::from_slice(
                                &std::fs::read(&state_path).expect("job state"),
                            )
                            .expect("state JSON");
                            let id = state["resumeSessionId"].as_str().unwrap_or(WORKER);
                            let transcript = server_config
                                .join("projects/workspace")
                                .join(format!("{id}.jsonl"));
                            let records = [
                                json!({"type":"user","sessionId":id,"promptId":"first-turn",
                                    "message":{"content":request["text"]}}),
                                json!({"type":"assistant","sessionId":id,
                                    "message":{"content":[{"type":"text","text":"alive"}]}}),
                                json!({"type":"system","sessionId":id,"subtype":"turn_duration"}),
                            ];
                            use std::io::Write;
                            let mut output = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&transcript)
                                .expect("transcript");
                            for record in records {
                                writeln!(output, "{record}").expect("transcript record");
                            }
                            state["linkScanPath"] = json!(transcript);
                            cccc_core::fs::write_json(&state_path, &state)
                                .expect("publish transcript");
                            json!({"ok":true,"op":"reply"})
                        }
                        "kill" => {
                            stopped = true;
                            json!({"ok":true,"op":"kill"})
                        }
                        op => panic!("unexpected operation {op}"),
                    };
                    stream
                        .get_mut()
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .expect("control response");
                }
            });
            Self {
                _temp: temp,
                _control: control,
                config,
                workspace,
                executable,
                saved,
                original,
                replies,
                server,
            }
        }

        async fn launch(&self) -> io::Result<LaunchedClaude> {
            launch(
                command::PreparedClaude {
                    executable: self.executable.to_string_lossy().into_owned(),
                    arguments: Vec::new(),
                    launch_environment: BTreeMap::from([(
                        "CLAUDE_CONFIG_DIR".into(),
                        self.config.to_string_lossy().into_owned(),
                    )]),
                    config_dir: self.config.clone(),
                    settings_path: self.config.join("managed.json"),
                },
                &self.workspace,
                GENERATION,
                SessionPurpose::Actor,
                None,
            )
            .await
        }

        async fn first_turn(&self) {
            let launched = self.launch().await.expect("fresh startup");
            assert!(!launched.resumed);
            let mut events = launched.protocol.subscribe();
            launched
                .protocol
                .start_prompt("first-turn", "say alive")
                .await
                .expect("first prompt accepted");
            let outcome = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let event = events.recv().await.expect("managed event");
                    if event.message["method"] == "turn/completed" {
                        break Ok(());
                    }
                    if event.message["method"] == MANAGED_AGENT_DISCONNECTED_METHOD {
                        break Err(event.message);
                    }
                }
            })
            .await;
            let alive = launched.protocol.running();
            let _ = launched.protocol.close().await;
            assert!(
                matches!(outcome, Ok(Ok(()))),
                "first turn failed: {outcome:?}"
            );
            assert!(alive, "healthy first turn must leave the lane running");
            assert_eq!(self.replies.load(Ordering::SeqCst), 1);
            assert_eq!(
                std::fs::read(&self.saved).expect("saved transcript"),
                self.original
            );
        }
    }

    #[tokio::test]
    async fn fresh_launch_with_a_pre_named_transcript_survives_its_first_turn() {
        Fixture::new(true, false).first_turn().await;
    }

    #[tokio::test]
    async fn unexpected_resumed_conversation_is_rejected_before_any_prompt() {
        let fixture = Fixture::new(true, true);
        let error = match fixture.launch().await {
            Err(error) => error,
            Ok(launched) => {
                let _ = launched.protocol.close().await;
                panic!("fresh launch accepted another conversation's resume identity");
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("fresh launch"), "{error}");
        assert!(error.to_string().contains("resumed"), "{error}");
        assert_eq!(fixture.replies.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(&fixture.saved).expect("saved transcript"),
            fixture.original
        );
    }

    #[tokio::test]
    async fn fresh_launch_without_a_named_conversation_still_completes_a_turn() {
        Fixture::new(false, false).first_turn().await;
    }
}
