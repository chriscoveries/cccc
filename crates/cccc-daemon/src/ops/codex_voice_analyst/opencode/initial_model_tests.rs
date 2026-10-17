//! Initial-model boundary tests (tests-only checkpoint, owner874 contract).
//!
//! The defect under test: a configured explicit OpenCode model reaches ACP
//! selection (`session/set_config_option`) but NOT the fresh viewer's initial
//! default — `attach` in actual 1.18.31 has no `--model` flag, so the viewer
//! defaults to whatever the backend's config says (observed: the user's MiMo
//! instead of the configured BigPickle). The candidate fix direction is a
//! per-process merge of the configured model into the spawned backend's
//! `{prefix}_CONFIG_CONTENT` (never a shared config/state/credential write).
//!
//! These tests drive the REAL `prepare` + `launch` path against the isolated
//! fake backend (`fake_opencode_fixture.py`): fake version probe, real
//! basic-auth HTTP enforcement, SSE `/event`, stdio ACP — and they capture the
//! ACTUAL spawned process environment the fake dumps, plus the recorded
//! `session/set_config_option` calls, not a future helper's return value.
//! Owned processes are stopped (launched dropped) before any RED assertion.
use super::*;
use std::os::unix::fs::PermissionsExt;

const FAKE_BACKEND: &str = include_str!("fake_opencode_fixture.py");

struct Harness {
    launched_environment: BTreeMap<String, String>,
    tui_command: Vec<String>,
    config_options: Vec<Value>,
    #[allow(dead_code)] // keeps the tempdir alive for the test body
    _temp: tempfile::TempDir,
}

/// Runs the real prepare+launch against the fake, captures the ACTUAL spawned
/// env (dumped by the fake on startup) and the ACP config-option records,
/// then stops the owned child before returning (cleanup before RED asserts).
async fn run_launch(
    configured_args: &[&str],
    base_environment: &BTreeMap<String, String>,
    runtime: super::super::ActorRuntime,
) -> Harness {
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
    let prepared =
        super::prepare(&configured, base_environment, runtime).expect("prepare the real way");
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
    .expect("launch against the fake backend");
    let launched_environment = launched.environment.clone();
    let tui_command = launched.tui_command.clone();
    drop(launched); // stop the owned backend before any assertion
    let env_dump: BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(root.join("env-dump.json")).expect("env dump"))
            .expect("env dump parses");
    let config_options = std::fs::read_to_string(root.join("config-options.jsonl"))
        .map(|body| {
            body.lines()
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_str::<Value>(line).expect("option json"))
                .collect()
        })
        .unwrap_or_default();
    Harness {
        launched_environment,
        tui_command,
        config_options,
        _temp: temp,
    }
}

fn environment_with(content: Option<&str>) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    environment.insert("HOME".into(), "/tmp/oc-initial-model".into());
    if let Some(content) = content {
        environment.insert("OPENCODE_CONFIG_CONTENT".into(), content.into());
    }
    environment
}

fn configured_content() -> &'static str {
    r#"{"model":"user/mimo","theme":"dark","provider":{"keep":{"models":{"a":{"name":"A"}}}}}"#
}

/// RED prediction: the configured model must reach the spawned backend's
/// OPENCODE_CONFIG_CONTENT (merged, unrelated keys preserved) so the fresh
/// viewer's initial default follows it. Today nothing merges — the spawned
/// env carries the actor content unchanged (or nothing at all), so the
/// viewer defaults to the user config model. Root names the actual RED.
#[tokio::test]
async fn configured_model_reaches_the_spawned_backend_config_content() {
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(configured_content())),
        super::super::ActorRuntime::Opencode,
    )
    .await;
    let merged: Value = serde_json::from_str(
        harness
            .launched_environment
            .get("OPENCODE_CONFIG_CONTENT")
            .expect("config content present in spawned env"),
    )
    .expect("merged config content is valid JSON");
    assert_eq!(
        merged["model"].as_str(),
        Some("probe/second"),
        "configured model must be the fresh viewer default"
    );
    assert_eq!(
        merged["theme"].as_str(),
        Some("dark"),
        "unrelated JSON keys survive the merge"
    );
    assert!(merged["provider"].is_object(), "provider block survives");
}

/// Same RED from the other direction: existing content WITHOUT a model key
/// gains only the model, byte-preserving everything else.
#[tokio::test]
async fn merge_adds_model_without_disturbing_keyless_content() {
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(r#"{"theme":"dark"}"#)),
        super::super::ActorRuntime::Opencode,
    )
    .await;
    let merged: Value = serde_json::from_str(
        harness
            .launched_environment
            .get("OPENCODE_CONFIG_CONTENT")
            .expect("config content present"),
    )
    .expect("valid JSON");
    assert_eq!(merged["model"].as_str(), Some("probe/second"));
    assert_eq!(merged["theme"].as_str(), Some("dark"));
}

/// Control (GREEN today and after): no explicit model means the existing
/// content is passed through UNCHANGED — the fix must be keyed on the
/// configured model only, never a default-injection.
#[tokio::test]
async fn without_explicit_model_existing_content_is_untouched() {
    let harness = run_launch(
        &[],
        &environment_with(Some(configured_content())),
        super::super::ActorRuntime::Opencode,
    )
    .await;
    assert_eq!(
        harness
            .launched_environment
            .get("OPENCODE_CONFIG_CONTENT")
            .map(String::as_str),
        Some(configured_content()),
        "no explicit model, no rewrite"
    );
}

/// Control (GREEN): invalid existing content is NOT silently overwritten —
/// today nothing parses it, so it passes through byte-identically. This pins
/// the current no-silent-overwrite behavior; what a FUTURE merge does with
/// invalid JSON (refuse vs preserve-and-skip) is unresolved scope that must
/// be agreed with root BEFORE any policy is added — see PLAN.md.
#[tokio::test]
async fn invalid_config_content_is_never_silently_overwritten() {
    let invalid = "{not json";
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(invalid)),
        super::super::ActorRuntime::Opencode,
    )
    .await;
    assert_eq!(
        harness
            .launched_environment
            .get("OPENCODE_CONFIG_CONTENT")
            .map(String::as_str),
        Some(invalid),
        "invalid content must pass through untouched until root defines policy"
    );
}

/// Control (GREEN): Kilo is preserved — its config key is KILO_CONFIG_CONTENT
/// and the merge contract covers OpenCode only.
#[tokio::test]
async fn kilo_runtime_config_content_is_untouched() {
    let kilo_content = r#"{"model":"user/mimo"}"#;
    let mut environment = BTreeMap::new();
    environment.insert("HOME".into(), "/tmp/oc-initial-model-kilo".into());
    environment.insert("KILO_CONFIG_CONTENT".into(), kilo_content.into());
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment,
        super::super::ActorRuntime::Kilo,
    )
    .await;
    assert_eq!(
        harness
            .launched_environment
            .get("KILO_CONFIG_CONTENT")
            .map(String::as_str),
        Some(kilo_content),
        "Kilo content untouched by the OpenCode model merge"
    );
}

/// Control (GREEN): the viewer command never gains a --model flag — attach in
/// actual 1.18.31 has none, and inventing one would break every launch. The
/// initial default travels via backend config, not argv.
#[tokio::test]
async fn viewer_command_never_gains_a_model_flag() {
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(configured_content())),
        super::super::ActorRuntime::Opencode,
    )
    .await;
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

/// Control (GREEN): the session-level ACP selection still happens — the fix
/// is additive (viewer default), not a replacement of the existing path.
#[tokio::test]
async fn session_level_acp_model_selection_still_happens() {
    let harness = run_launch(
        &["--model", "probe/second"],
        &environment_with(Some(configured_content())),
        super::super::ActorRuntime::Opencode,
    )
    .await;
    assert!(
        harness.config_options.iter().any(|option| {
            option["configId"] == json!("model") && option["value"] == json!("probe/second")
        }),
        "set_config_option(model) must still be sent: {:?}",
        harness.config_options
    );
}
