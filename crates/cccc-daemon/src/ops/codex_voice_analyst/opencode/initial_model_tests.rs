//! Initial-model boundary tests (corrected checkpoint v2, owner874).
//!
//! Polarity as observed on the actual stage: a configured explicit model
//! (the stage's MiMo) reaches ACP selection (`session/set_config_option`)
//! but the viewer's observed initial default is a different model (the
//! backend user/assistant BigPickle). `attach` in actual 1.18.31 has no
//! `--model` flag, so the fresh viewer defaults to the backend's config;
//! the candidate fix is a per-process merge of the effective inline config
//! (`OPENCODE_CONFIG_CONTENT`, JSONC accepted by native 1.18.31) before
//! backend spawn — never a shared config/state/credential write.
//! `MODEL-POLICY-DECISION.json` is the policy these tests encode.
//!
//! BOTH ORACLES: every merge/passthrough condition is asserted
//! independently on (a) the returned `LaunchedOpenCode.environment` AND
//! (b) the ACTUAL spawned process environment the fake dumps — equality
//! between them never substitutes for the conditions themselves. The dump
//! is allowlisted to `*_CONFIG_CONTENT` keys + `OC_FIXTURE_MARKER` (no
//! credentials), written only on the ACP phase (never on `--version`).
//!
//! Teardown: `protocol.close().await` + `process.stop()` + `!running()`
//! happen explicitly in `run_launch` BEFORE any test-body assertion — no
//! drop-lifetime assumption. Oracles are read only after teardown.
use super::*;
use std::os::unix::fs::PermissionsExt;

const FAKE_BACKEND: &str = include_str!("fake_opencode_fixture.py");
const CONFIG_KEY: &str = "OPENCODE_CONFIG_CONTENT";

struct Harness {
    /// Returned launch metadata (oracle A).
    launched_environment: BTreeMap<String, String>,
    /// ACTUAL spawned process environment as dumped by the fake (oracle B).
    spawned_environment: BTreeMap<String, String>,
    tui_command: Vec<String>,
    config_options: Vec<Value>,
    root: std::path::PathBuf,
    #[allow(dead_code)] // keeps the tempdir alive for the test body
    _temp: tempfile::TempDir,
}

/// Runs the real prepare+launch against the fake. On success: explicit
/// teardown (close protocol, stop process, assert not running), then read
/// both oracles. On failure: returns the error together with the (possibly
/// never-spawned) root so a test can assert that no backend ever ran.
async fn run_launch(
    configured_args: &[&str],
    base_environment: &BTreeMap<String, String>,
    runtime: super::super::ActorRuntime,
) -> Result<Harness, (std::io::Error, std::path::PathBuf)> {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    let executable = root.join("fake-opencode");
    std::fs::write(&executable, FAKE_BACKEND).expect("write fake backend");
    let mut permissions = std::fs::metadata(&executable)
        .expect("fake metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("fake executable");
    let mut configured: Vec<String> = vec![executable.to_string_lossy().into_owned()];
    configured.extend(configured_args.iter().map(|value| (*value).to_owned()));
    let prepared = super::prepare(&configured, base_environment, runtime)
        .map_err(|error| (error, root.clone()))?;
    let launched = super::launch(
        prepared,
        &root,
        base_environment.clone(),
        "initial-model-test",
        super::super::SessionPurpose::Actor,
        None,
        json!({}),
    )
    .await
    .map_err(|error| (error, root.clone()))?;
    // Explicit teardown BEFORE any assertion: close the ACP protocol (EOF
    // on the fake's stdin), stop the owned child, and require not-running
    // — a drop assumption is not an oracle.
    launched.protocol.close().await;
    launched
        .process
        .stop()
        .expect("stop the owned fake backend");
    assert!(
        !launched.process.running(),
        "owned fake must be stopped before any RED assertion"
    );
    let launched_environment = launched.environment.clone();
    let tui_command = launched.tui_command.clone();
    let spawned_environment: BTreeMap<String, String> = serde_json::from_slice(
        &std::fs::read(root.join("env-dump.json")).expect("env dump after teardown"),
    )
    .expect("env dump parses");
    let config_options = std::fs::read_to_string(root.join("config-options.jsonl"))
        .map(|body| {
            body.lines()
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_str::<Value>(line).expect("option json"))
                .collect()
        })
        .unwrap_or_default();
    Ok(Harness {
        launched_environment,
        spawned_environment,
        tui_command,
        config_options,
        root,
        _temp: temp,
    })
}

fn environment_with(content: Option<&str>) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    environment.insert("OC_FIXTURE_MARKER".into(), "initial-model-v2".into());
    if let Some(content) = content {
        environment.insert(CONFIG_KEY.into(), content.into());
    }
    environment
}

fn get(environment: &BTreeMap<String, String>, key: &str) -> Option<&str> {
    environment.get(key).map(String::as_str)
}

/// Asserts the merge conditions on ONE oracle. Called separately for the
/// returned env and the actual spawned env — the conditions themselves are
/// checked twice, independently; equality is never used as a substitute.
fn assert_merged_config(environment: &BTreeMap<String, String>, model: &str) {
    let content = environment
        .get(CONFIG_KEY)
        .expect("config content present in this oracle");
    // Canonical JSON output is acceptable for process env (policy), even
    // when the input was JSONC.
    let merged: Value = serde_json::from_str(content).expect("merge output is canonical JSON");
    assert_eq!(
        merged["model"].as_str(),
        Some(model),
        "configured model must be the fresh viewer initial default"
    );
    assert_eq!(
        merged["theme"].as_str(),
        Some("dark"),
        "unrelated value survives the merge"
    );
    assert!(
        merged["provider"]["keep"]["models"]["a"]["name"].as_str() == Some("A"),
        "nested provider values survive the merge: {merged}"
    );
}

/// Case 1 — RED predicted: JSONC inline config (comment + trailing comma,
/// both proven accepted by native 1.18.31) must merge so the configured
/// model wins the fresh viewer default while every existing value is
/// retained. Today nothing merges, so the spawned config still carries the
/// old model. Root names the actual RED.
#[tokio::test]
async fn jsonc_input_merges_and_retains_values_on_both_oracles() {
    let jsonc = r#"{
        // inline comment accepted by native 1.18.31
        "theme": "dark",
        "provider": {"keep": {"models": {"a": {"name": "A"}}}},
    }"#;
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(jsonc)),
        super::super::ActorRuntime::Opencode,
    )
    .await
    .expect("launch against the fake backend");
    assert_merged_config(&harness.launched_environment, "probe/second");
    assert_merged_config(&harness.spawned_environment, "probe/second");
}

/// Case 2 — RED predicted: keyless plain-JSON content gains only the model.
#[tokio::test]
async fn keyless_content_gains_model_without_disturbing_values() {
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(r#"{"theme":"dark"}"#)),
        super::super::ActorRuntime::Opencode,
    )
    .await
    .expect("launch against the fake backend");
    for oracle in [&harness.launched_environment, &harness.spawned_environment] {
        let merged: Value = serde_json::from_str(oracle.get(CONFIG_KEY).expect("content"))
            .expect("canonical JSON output");
        assert_eq!(merged["model"].as_str(), Some("probe/second"));
        assert_eq!(merged["theme"].as_str(), Some("dark"));
    }
}

/// Case 3 — control: without an explicit model, opaque actor content (even
/// malformed) passes through byte-identically to native validation.
#[tokio::test]
async fn without_explicit_model_opaque_content_passes_unchanged() {
    let harness = run_launch(
        &[],
        &environment_with(Some(r#"{"theme":"dark"}"#)),
        super::super::ActorRuntime::Opencode,
    )
    .await
    .expect("launch against the fake backend");
    for oracle in [&harness.launched_environment, &harness.spawned_environment] {
        assert_eq!(
            get(oracle, CONFIG_KEY),
            Some(r#"{"theme":"dark"}"#),
            "no explicit model, no rewrite"
        );
    }
}

/// Case 4 — RED predicted, per MODEL-POLICY-DECISION: explicit model +
/// invalid/non-object effective config must FAIL BEFORE backend spawn with
/// a static diagnostic. The absence of env-dump.json proves no backend ever
/// ran. Today nothing validates, so launch succeeds.
#[tokio::test]
async fn explicit_model_with_invalid_config_fails_before_backend_spawn() {
    let outcome = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some("{not json")),
        super::super::ActorRuntime::Opencode,
    )
    .await;
    let (error, root) = match outcome {
        Ok(_) => panic!("invalid config + explicit model must fail before spawn"),
        Err(captured) => captured,
    };
    assert!(
        !root.join("env-dump.json").exists(),
        "no backend may have been spawned (spawn error was: {error})"
    );
}

/// Case 5 — control: without an explicit model, malformed content passes
/// unchanged (policy item 3); native validation owns its rejection.
#[tokio::test]
async fn invalid_config_without_model_reaches_the_backend_unchanged() {
    let invalid = "{not json";
    let harness = run_launch(
        &[],
        &environment_with(Some(invalid)),
        super::super::ActorRuntime::Opencode,
    )
    .await
    .expect("launch with opaque invalid content");
    for oracle in [&harness.launched_environment, &harness.spawned_environment] {
        assert_eq!(get(oracle, CONFIG_KEY), Some(invalid));
    }
}

/// Case 6 — control: Kilo unchanged by the OpenCode merge (policy item 3).
#[tokio::test]
async fn kilo_runtime_config_content_is_untouched() {
    let kilo_content = r#"{"model":"user/mimo"}"#;
    let mut environment = environment_with(None);
    environment.insert("KILO_CONFIG_CONTENT".into(), kilo_content.into());
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment,
        super::super::ActorRuntime::Kilo,
    )
    .await
    .expect("launch kilo against the fake backend");
    for oracle in [&harness.launched_environment, &harness.spawned_environment] {
        assert_eq!(
            get(oracle, "KILO_CONFIG_CONTENT"),
            Some(kilo_content),
            "Kilo content untouched by the OpenCode model merge"
        );
    }
    // The OpenCode key must not be INJECTED for Kilo; the spawned oracle may
    // still show an ambient value (launch never env_clears), so only an
    // ambient-free run may assert absence.
    if std::env::var(CONFIG_KEY).is_err() {
        assert_eq!(
            get(&harness.spawned_environment, CONFIG_KEY),
            None,
            "OpenCode key must not appear for the Kilo runtime"
        );
    }
}

/// Case 7 — control: the viewer argv never gains a --model flag (attach in
/// actual 1.18.31 has none); the initial default travels via backend
/// config, not argv.
#[tokio::test]
async fn viewer_command_never_gains_a_model_flag() {
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(r#"{"theme":"dark"}"#)),
        super::super::ActorRuntime::Opencode,
    )
    .await
    .expect("launch against the fake backend");
    assert!(
        !harness.tui_command.iter().any(|arg| arg.contains("model")),
        "attach argv must stay flag-clean: {:?}",
        harness.tui_command
    );
    assert!(
        harness.tui_command.iter().any(|arg| arg == "attach"),
        "the viewer command is still an attach"
    );
}

/// Case 8 — control, QUALIFIED: the initial session-level selection still
/// happens (the fix is additive). This control does NOT prove that later
/// explicit model choices stay free — an initial set_config_option says
/// nothing about subsequent requests. Required follow-up after any fix: a
/// real later explicit-model request and a live integration check that the
/// later choice is not repinned. Pinned here as an open gate, not a claim.
#[tokio::test]
async fn session_level_acp_model_selection_still_happens() {
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(r#"{"theme":"dark"}"#)),
        super::super::ActorRuntime::Opencode,
    )
    .await
    .expect("launch against the fake backend");
    assert!(
        harness.config_options.iter().any(|option| {
            option["configId"] == json!("model") && option["value"] == json!("probe/second")
        }),
        "set_config_option(model) must still be sent: {:?}",
        harness.config_options
    );
}

/// Case 9 — control: inherited inline config. launch() uses Command.envs
/// WITHOUT env_clear, so when the actor/base map has no config key the
/// child inherits whatever the surrounding process carries — that effective
/// content must be preserved (policy item 4). The oracle is a read-only
/// std::env::var of this test process (no set_var, no global env race):
/// the ACTUAL spawned env must show exactly the inherited value, while the
/// returned launch metadata legitimately knows only the explicit map.
#[tokio::test]
async fn inherited_config_content_survives_when_actor_key_is_absent() {
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(None),
        super::super::ActorRuntime::Opencode,
    )
    .await
    .expect("launch against the fake backend");
    match std::env::var(CONFIG_KEY) {
        Ok(inherited) => assert_eq!(
            get(&harness.spawned_environment, CONFIG_KEY),
            Some(inherited.as_str()),
            "the inherited inline config must reach the spawned backend unchanged"
        ),
        Err(_) => assert_eq!(
            get(&harness.spawned_environment, CONFIG_KEY),
            None,
            "nothing inherited, nothing injected at spawn level"
        ),
    }
    assert!(
        harness.launched_environment.get(CONFIG_KEY).is_none_or(
            |value| std::env::var(CONFIG_KEY).is_ok_and(|inherited| *value == inherited)
        ),
        "returned metadata only carries the explicit map"
    );
}

/// Case 10 — fixture integrity: the fixture's SSE keepalive source must use
/// single-backslash python escapes (real 0x0A bytes at runtime), never a
/// double-escaped literal backslash-n that would emit a broken stream.
#[test]
fn fixture_source_has_no_double_escaped_newlines() {
    assert!(
        FAKE_BACKEND.contains(r#"b": keepalive\n\n""#),
        "SSE keepalive must be a single-escaped python bytes literal"
    );
    assert!(
        !FAKE_BACKEND.contains(r#"keepalive\\n"#),
        "double-escaped newline in the fixture source"
    );
}
