use super::*;

// Exercises the real OpenCode ACP/SSE bridge with isolated local models and
// noReply submissions (the same committed user-message shape as native TUI).
// Repeat fresh subscriptions: an immediately submitted first message used to
// disappear through the lazily subscribed global endpoint.
// No provider credentials or paid inference are required.
#[tokio::test]
async fn live_opencode_submitted_model_reaches_acp_when_enabled() {
    if std::env::var("CCCC_OPENCODE_MODEL_SYNC_LIVE").as_deref() != Ok("1") {
        return;
    }
    for _ in 0..3 {
        submitted_model_reaches_acp("opencode", "OPENCODE").await;
    }
}

#[tokio::test]
async fn live_kilo_submitted_model_reaches_acp_when_enabled() {
    if std::env::var("CCCC_KILO_MODEL_SYNC_LIVE").as_deref() != Ok("1") {
        return;
    }
    for _ in 0..3 {
        submitted_model_reaches_acp("kilo", "KILO").await;
    }
}

// Uses the real native backend, but only local fixture models and noReply.
// The production merge must change session/new's initial default while later
// explicit native submissions remain free to select another model. This is
// native ACP/HTTP evidence, not a claim about interactive TUI key handling.
#[tokio::test]
async fn live_opencode_merged_default_and_later_choice_when_enabled() {
    if std::env::var("CCCC_OPENCODE_INITIAL_MODEL_LIVE").as_deref() != Ok("1") {
        return;
    }
    submitted_model_with_initial_default("opencode", "OPENCODE", Some("cccc-probe/second")).await;
    eprintln!("CCCC_NATIVE_INITIAL_MODEL_AND_LATER_CHOICES_VERIFIED");
}

async fn submitted_model_reaches_acp(runtime: &'static str, prefix: &str) {
    submitted_model_with_initial_default(runtime, prefix, None).await;
}

async fn submitted_model_with_initial_default(
    runtime: &'static str,
    prefix: &str,
    initial_model: Option<&str>,
) {
    let temp = tempfile::tempdir().expect("isolated home");
    let root = temp.path();
    let port = lifecycle::reserve_loopback_port().expect("port");
    let endpoint = format!("http://127.0.0.1:{port}");
    let password = uuid::Uuid::new_v4().to_string();
    let executable =
        std::env::var(format!("CCCC_{prefix}_EXECUTABLE")).unwrap_or_else(|_| runtime.into());
    let mut environment = BTreeMap::from([
        ("HOME".into(), root.to_string_lossy().into_owned()),
        ("XDG_CONFIG_HOME".into(), root.join("config").to_string_lossy().into_owned()),
        ("XDG_DATA_HOME".into(), root.join("data").to_string_lossy().into_owned()),
        ("XDG_STATE_HOME".into(), root.join("state").to_string_lossy().into_owned()),
        ("XDG_CACHE_HOME".into(), root.join("cache").to_string_lossy().into_owned()),
        (format!("{prefix}_DB"), root.join("provider.db").to_string_lossy().into_owned()),
        (format!("{prefix}_SERVER_USERNAME"), runtime.into()),
        (format!("{prefix}_SERVER_PASSWORD"), password.clone()),
        (format!("{prefix}_NO_DAEMON"), "1".into()),
        (format!("{prefix}_CONFIG_CONTENT"), json!({
            "model":"cccc-probe/first",
            "provider":{"cccc-probe":{
                "npm":"@ai-sdk/openai-compatible",
                "options":{"baseURL":"http://127.0.0.1:9/v1","apiKey":"local-test"},
                "models":{
                    "first":{"name":"First"},
                    "second":{"name":"Second","variants":{"default":{},"high":{"reasoningEffort":"high"}}}
                }
            }}
        }).to_string()),
    ]);
    if let Some(model) = initial_model {
        assert_eq!(runtime, "opencode");
        initial_config::apply(&mut environment, Some(model))
            .expect("production initial config merge");
    }
    let command = vec![
        executable,
        "acp".into(),
        "--hostname".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "--cwd".into(),
        root.to_string_lossy().into_owned(),
    ];
    let (owner, stdin, stdout) =
        process::spawn_piped(&command, root, &environment, "model-sync-test")
            .expect("start OpenCode");
    let owner = Arc::new(owner);
    lifecycle::wait_for_authenticated_backend(&endpoint, runtime, &password, Arc::clone(&owner))
        .await
        .expect("authenticated backend");
    let protocol = AcpClient::new(
        stdin,
        stdout,
        "model-sync-test".into(),
        runtime,
        PermissionPolicy::Reject,
        PromptCompletion::SessionEvents,
    )
    .expect("ACP client");
    protocol
        .request(
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
            Duration::from_secs(20),
        )
        .await
        .expect("initialize");
    let created = protocol
        .request(
            "session/new",
            json!({"cwd":root,"mcpServers":[]}),
            Duration::from_secs(30),
        )
        .await
        .expect("session");
    let id = created["sessionId"].as_str().expect("session id");
    let mode = option(&created, "mode").expect("current native mode");
    assert_eq!(
        option(&created, "model"),
        Some(initial_model.unwrap_or("cccc-probe/first"))
    );
    lifecycle::attach(&protocol, &endpoint, runtime, &password, id, root)
        .await
        .expect("SSE bridge");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP client");
    if let Some(model) = initial_model {
        let config: Value = client
            .get(format!("{endpoint}/config"))
            .basic_auth(runtime, Some(&password))
            .send()
            .await
            .expect("read native initial config")
            .error_for_status()
            .expect("native config response")
            .json()
            .await
            .expect("native config JSON");
        assert_eq!(config["model"].as_str(), Some(model));
    }

    for (model, variant) in [
        ("second", "high"),
        ("first", "default"),
        ("second", "default"),
    ] {
        let submitted = client
            .post(format!("{endpoint}/session/{id}/message"))
            .basic_auth(runtime, Some(&password))
            .json(&json!({
                "model":{"providerID":"cccc-probe","modelID":model},
                "variant":variant,
                "noReply":true,
                "parts":[{"type":"text","text":format!("selected {model}/{variant}")}]
            }))
            .send()
            .await
            .expect("submit native message");
        let status = submitted.status();
        let body = submitted.text().await.expect("submission response");
        assert!(status.is_success(), "native submission {status}: {body}");
        let expected = format!("cccc-probe/{model}");
        let mut last_options = None;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                // Setting mode returns all current options without changing the model.
                let config = protocol
                    .request(
                        "session/set_config_option",
                        json!({"sessionId":id,"configId":"mode","value":mode}),
                        Duration::from_secs(5),
                    )
                    .await
                    .expect("inspect current ACP model");
                last_options = Some((option(&config, "model").map(str::to_owned), option(&config, "effort").map(str::to_owned)));
                if option(&config, "model") == Some(expected.as_str())
                    && (model == "first" || option(&config, "effort") == Some(variant))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("submitted model/variant must reach ACP: expected={expected}/{variant}, observed={last_options:?}"));
    }
    protocol.close().await;
    owner.stop().expect("stop isolated OpenCode");
}

fn option<'a>(config: &'a Value, id: &str) -> Option<&'a str> {
    config
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|option| option["id"] == id)?["currentValue"]
        .as_str()
}
