//! Muse runs as a plain PTY lane, but its TUI cannot be handed a
//! session id on first launch (only `muse exec --session-id` accepts one).
//! CCCC therefore gives each Muse actor a private Muse data home via
//! `XDG_DATA_HOME` (auth and settings still come from the user's config
//! home), records a pending-capture receipt at launch, and discovers the
//! session id Muse created in that private store. Later launches replay
//! `muse resume <id>` against the same receipt, so an actor restart or a
//! daemon restart returns to the same conversation. `actor.new_session`
//! removes the receipt through the generic path, which also drops the
//! pending marker, so discovery never resurrects a discarded session.

use super::{
    command_fingerprint, model_from_command, path as receipt_path, read, resume_enabled, string,
    valid_session_id, workspace_path, write,
};
use cccc_contracts::utc_now;
use cccc_core::HomeLayout;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MANAGED_RECORD_VERSION: u64 = 2;
const MANAGED_TRANSPORT: &str = "muse_pty";
/// Clock slack when matching a session directory to a launch marker.
const DISCOVERY_SLACK_MS: u64 = 2_000;

pub struct Prepared {
    pub command: Vec<String>,
    /// The recorded Muse session the launch resumes, if any.
    pub session_id: Option<String>,
}

/// Resolve the actor's Muse data home and resume state, then build the PTY
/// launch command. `environment` gains the private `XDG_DATA_HOME` default
/// unless the actor already configured one explicitly.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &mut BTreeMap<String, String>,
    cccc_executable: &Path,
) -> std::io::Result<Prepared> {
    let data_home = ensure_data_home(home, group_id, actor_id, cwd, environment)?;
    let mut session_id = None;
    if resume_enabled() {
        if let Some(mut document) =
            resume_candidate(home, group_id, actor_id, cwd, base_command, environment)?
        {
            let known = string(&document, "provider_session_id");
            let candidate = if valid_session_id(&known) {
                session_exists(&data_home, &known).then_some(known)
            } else if string(&document, "status") == "pending_capture" {
                discover_session(
                    &data_home,
                    launched_at_ms(&document).map(|at| at.saturating_sub(DISCOVERY_SLACK_MS)),
                )
            } else {
                None
            };
            if let Some(candidate) = candidate {
                document.insert("provider_session_id".into(), json!(candidate.clone()));
                document.insert("status".into(), json!("usable"));
                document.insert("resume_eligible".into(), json!(true));
                if string(&document, "captured_from").is_empty() {
                    document.insert("captured_from".into(), json!("muse_pty_discovery"));
                }
                if let Some(attempt) = super::resume_failure::begin(&mut document, candidate)? {
                    write(home, group_id, actor_id, &document)?;
                    session_id = Some(attempt.session_id);
                }
            }
        }
        if session_id.is_none() {
            record_launch_marker(home, group_id, actor_id, cwd, base_command, environment)?;
        }
    }
    Ok(Prepared {
        command: launch_command(base_command, session_id.as_deref(), cccc_executable),
        session_id,
    })
}

/// After a fresh PTY spawn, capture the session Muse just created in the
/// actor's private store so the receipt names it before the next restart.
pub fn capture_after_launch(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &BTreeMap<String, String>,
) -> std::io::Result<Option<String>> {
    if !resume_enabled() {
        return Ok(None);
    }
    let Some(document) =
        resume_candidate(home, group_id, actor_id, cwd, base_command, environment)?
    else {
        return Ok(None);
    };
    if string(&document, "status") != "pending_capture" {
        return Ok(None);
    }
    let data_home = data_home(home, group_id, actor_id, cwd, environment)?;
    let Some(session_id) = discover_session(
        &data_home,
        launched_at_ms(&document).map(|at| at.saturating_sub(DISCOVERY_SLACK_MS)),
    ) else {
        return Ok(None);
    };
    record_managed(
        home,
        group_id,
        actor_id,
        cwd,
        base_command,
        environment,
        &session_id,
        false,
    )?;
    Ok(Some(session_id))
}

/// Build the PTY argv: the user's Muse flags, an optional
/// `resume <session-id>`, and the per-launch CCCC MCP server. Muse has no
/// `mcp add` command; `-c mcpServers.cccc=...` is its documented per-run
/// settings shape, and env values must be whole-value `${VAR}` references,
/// which the launch environment already provides. A command the user shaped
/// with a subcommand or a positional prompt is never rewritten with
/// `resume`, and a user-supplied `mcpServers.cccc` is never shadowed.
#[must_use]
pub fn launch_command(
    base_command: &[String],
    resume_session_id: Option<&str>,
    cccc_executable: &Path,
) -> Vec<String> {
    let mut command = base_command.to_vec();
    if command.is_empty() {
        return command;
    }
    if let Some(session_id) = resume_session_id
        && !has_positional_or_subcommand(base_command)
    {
        command.push("resume".into());
        command.push(session_id.into());
    }
    if !user_defines_cccc_mcp(base_command) {
        command.push("-c".into());
        command.push(mcp_config_arg(cccc_executable));
    }
    command
}

fn mcp_config_arg(cccc_executable: &Path) -> String {
    let server = json!({
        "command": cccc_executable.to_string_lossy(),
        "args": ["mcp"],
        "env": {
            "CCCC_HOME": "${CCCC_HOME}",
            "CCCC_GROUP_ID": "${CCCC_GROUP_ID}",
            "CCCC_ACTOR_ID": "${CCCC_ACTOR_ID}",
        },
    });
    format!("mcpServers.cccc={server}")
}

/// Root flags that consume the following token as their value (`muse --help`).
/// `--worktree` takes an optional value, so a following token stays ambiguous
/// and the command is conservatively treated as user-shaped.
const VALUE_FLAGS: &[&str] = &[
    "--agents",
    "--provider",
    "--preset",
    "--model",
    "--contributor-mode",
    "-c",
    "--config",
    "--reasoning-effort",
    "--effort",
    "--base-url",
    "--image",
    "--workspace",
    "--worktree-base",
    "--worktree-existing",
    "--permission-profile",
    "--approval-mode",
    "--approval-judge",
    "--echo-delay-ms",
    "--sandbox-network",
];

fn has_positional_or_subcommand(command: &[String]) -> bool {
    let mut index = 1;
    while let Some(token) = command.get(index) {
        index += 1;
        if token == "--" {
            return true;
        }
        if !token.starts_with('-') || token == "-" {
            return true;
        }
        if !token.contains('=') && VALUE_FLAGS.contains(&token.as_str()) {
            index += 1;
        }
    }
    false
}

fn user_defines_cccc_mcp(command: &[String]) -> bool {
    let mut index = 1;
    while let Some(token) = command.get(index) {
        index += 1;
        let value = if matches!(token.as_str(), "-c" | "--config") {
            command.get(index).map(String::as_str)
        } else {
            token
                .strip_prefix("--config=")
                .or_else(|| token.strip_prefix("-c="))
        };
        if value.is_some_and(|value| {
            value == "mcpServers.cccc"
                || value.starts_with("mcpServers.cccc=")
                || value.starts_with("mcpServers.cccc.")
        }) {
            return true;
        }
        if matches!(token.as_str(), "-c" | "--config") {
            index += 1;
        }
    }
    false
}

fn ensure_data_home(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    environment: &mut BTreeMap<String, String>,
) -> std::io::Result<PathBuf> {
    let configured = environment
        .get("XDG_DATA_HOME")
        .is_some_and(|value| !value.trim().is_empty());
    if !configured {
        let default = default_data_home(home, group_id, actor_id)?;
        environment.insert(
            "XDG_DATA_HOME".into(),
            default.to_string_lossy().into_owned(),
        );
    }
    let data_home = data_home(home, group_id, actor_id, cwd, environment)?;
    std::fs::create_dir_all(&data_home)?;
    Ok(data_home)
}

fn data_home(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
) -> std::io::Result<PathBuf> {
    if let Some(value) = environment
        .get("XDG_DATA_HOME")
        .filter(|value| !value.trim().is_empty())
    {
        let path = PathBuf::from(value);
        return Ok(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        });
    }
    default_data_home(home, group_id, actor_id)
}

fn default_data_home(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
) -> std::io::Result<PathBuf> {
    // The receipt path validates the actor id and resolves the group state
    // directory: <state>/runtime_sessions/<actor>.json.
    let receipt = receipt_path(home, group_id, actor_id)?;
    let state_dir = receipt
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| std::io::Error::other("runtime session receipt has no state directory"))?;
    Ok(state_dir.join("muse").join(actor_id).join("data"))
}

fn sessions_root(data_home: &Path) -> PathBuf {
    data_home.join("muse").join("sessions")
}

/// Session directories live at `sessions/<yyyy>/<mm>/<dd>/<session-id>/`.
/// The store is private to one actor, so the newest entry is unambiguous.
fn session_dirs(data_home: &Path) -> Vec<(String, u64, u64)> {
    let mut found = Vec::new();
    let root = sessions_root(data_home);
    let mut stack = vec![(root, 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if depth == 3 {
                if valid_session_id(&name)
                    && let Ok(metadata) = entry.metadata()
                {
                    let activity = metadata.modified().unwrap_or(UNIX_EPOCH);
                    let created = metadata.created().unwrap_or(activity);
                    found.push((name, unix_ms(created), unix_ms(activity)));
                }
            } else if (depth == 0 && name.len() == 4 && name.bytes().all(|b| b.is_ascii_digit()))
                || (depth > 0 && name.len() == 2 && name.bytes().all(|b| b.is_ascii_digit()))
            {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    found
}

fn session_exists(data_home: &Path, session_id: &str) -> bool {
    session_dirs(data_home)
        .iter()
        .any(|(id, _, _)| id == session_id)
}

fn discover_session(data_home: &Path, not_before_ms: Option<u64>) -> Option<String> {
    session_dirs(data_home)
        .into_iter()
        .filter(|(_, created, _)| not_before_ms.is_none_or(|bound| *created >= bound))
        .max_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)))
        .map(|(id, _, _)| id)
}

fn launched_at_ms(document: &Map<String, Value>) -> Option<u64> {
    document.get("launched_at_unix_ms").and_then(Value::as_u64)
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn now_unix_ms() -> u64 {
    unix_ms(SystemTime::now())
}

/// The stored receipt, only when it still describes this launch exactly.
fn resume_candidate(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &BTreeMap<String, String>,
) -> std::io::Result<Option<Map<String, Value>>> {
    let Ok(document) = read(home, group_id, actor_id) else {
        return Ok(None);
    };
    if document.get("v").and_then(Value::as_u64) != Some(MANAGED_RECORD_VERSION)
        || string(&document, "kind") != "runtime_session"
        || string(&document, "transport") != MANAGED_TRANSPORT
        || string(&document, "runtime") != "muse"
        || string(&document, "workspace_path") != workspace_path(cwd)
        || string(&document, "command_fingerprint") != command_fingerprint(base_command)
        || string(&document, "model") != model_from_command(base_command)
        || string(&document, "identity_fingerprint")
            != identity_fingerprint(base_command, environment)
        || !string(&document, "provider_thread_id").is_empty()
    {
        return Ok(None);
    }
    Ok(Some(document))
}

fn record_launch_marker(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    let mut document = base_document(group_id, actor_id, cwd, base_command, environment);
    document.insert("provider_session_id".into(), json!(""));
    document.insert("captured_from".into(), json!(""));
    document.insert("status".into(), json!("pending_capture"));
    document.insert("resume_eligible".into(), json!(false));
    document.insert("launched_at_unix_ms".into(), json!(now_unix_ms()));
    write(home, group_id, actor_id, &document)
}

#[allow(clippy::too_many_arguments)]
pub fn record_managed(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &BTreeMap<String, String>,
    session_id: &str,
    resumed: bool,
) -> std::io::Result<()> {
    if !resume_enabled() {
        return Ok(());
    }
    if !valid_session_id(session_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Muse session id",
        ));
    }
    let mut document = base_document(group_id, actor_id, cwd, base_command, environment);
    document.insert("provider_session_id".into(), json!(session_id));
    document.insert(
        "captured_from".into(),
        json!(if resumed {
            "muse_pty_resume"
        } else {
            "muse_pty_discovery"
        }),
    );
    document.insert("status".into(), json!("usable"));
    document.insert("resume_eligible".into(), json!(true));
    document.insert("launched_at_unix_ms".into(), json!(now_unix_ms()));
    write(home, group_id, actor_id, &document)
}

fn base_document(
    group_id: &str,
    actor_id: &str,
    cwd: &Path,
    base_command: &[String],
    environment: &BTreeMap<String, String>,
) -> Map<String, Value> {
    Map::from_iter([
        ("v".into(), json!(MANAGED_RECORD_VERSION)),
        ("kind".into(), json!("runtime_session")),
        ("transport".into(), json!(MANAGED_TRANSPORT)),
        ("group_id".into(), json!(group_id)),
        ("actor_id".into(), json!(actor_id)),
        ("runtime".into(), json!("muse")),
        ("workspace_path".into(), json!(workspace_path(cwd))),
        (
            "command_fingerprint".into(),
            json!(command_fingerprint(base_command)),
        ),
        ("model".into(), json!(model_from_command(base_command))),
        (
            "identity_fingerprint".into(),
            json!(identity_fingerprint(base_command, environment)),
        ),
        ("provider_thread_id".into(), json!("")),
        ("resume_command_hint".into(), json!("")),
        ("last_seen_at".into(), json!(utc_now())),
        ("last_resume_attempt_at".into(), json!("")),
        ("last_resume_attempt_id".into(), json!("")),
        ("last_resume_error".into(), json!("")),
        ("failure_count".into(), json!(0)),
        ("updated_at".into(), json!(utc_now())),
    ])
}

fn identity_fingerprint(command: &[String], environment: &BTreeMap<String, String>) -> String {
    cccc_core::codex_voice_settings::ResolvedAgentRuntime {
        runtime: cccc_contracts::ActorRuntime::Muse,
        runtime_mode: cccc_contracts::RuntimeMode::default(),
        command: command.to_vec(),
        environment: environment.clone(),
    }
    .identity_fingerprint()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_core::GroupStore;

    const SESSION_ID: &str = "01a13489-c7a8-7700-8f63-d7836f58ba3f";
    const OTHER_SESSION_ID: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn launch_command_appends_resume_and_mcp_to_flag_only_commands() {
        let built = launch_command(
            &argv(&["muse", "--yolo", "--model", "muse-wonder-full-1.0"]),
            Some(SESSION_ID),
            Path::new("/opt/cccc"),
        );
        assert_eq!(
            built[..4],
            argv(&["muse", "--yolo", "--model", "muse-wonder-full-1.0"])
        );
        assert_eq!(built[4], "resume");
        assert_eq!(built[5], SESSION_ID);
        assert_eq!(built[6], "-c");
        let config: Value =
            serde_json::from_str(built[7].strip_prefix("mcpServers.cccc=").expect("prefix"))
                .expect("mcp server json");
        assert_eq!(config["command"], "/opt/cccc");
        assert_eq!(config["args"], json!(["mcp"]));
        assert_eq!(config["env"]["CCCC_GROUP_ID"], "${CCCC_GROUP_ID}");
        assert_eq!(config["env"]["CCCC_ACTOR_ID"], "${CCCC_ACTOR_ID}");
        assert_eq!(config["env"]["CCCC_HOME"], "${CCCC_HOME}");
    }

    #[test]
    fn launch_command_without_resume_still_injects_mcp() {
        let built = launch_command(&argv(&["muse", "--yolo"]), None, Path::new("/opt/cccc"));
        assert!(!built.iter().any(|part| part == "resume"));
        assert!(
            built
                .last()
                .is_some_and(|last| last.starts_with("mcpServers.cccc="))
        );
    }

    #[test]
    fn launch_command_never_rewrites_user_shaped_commands() {
        for base in [
            argv(&["muse", "exec", "--session-id", OTHER_SESSION_ID, "hi"]),
            argv(&["muse", "resume", OTHER_SESSION_ID]),
            argv(&["muse", "--yolo", "fix the build"]),
            argv(&["muse", "--", "prompt with - dashes"]),
        ] {
            let built = launch_command(&base, Some(SESSION_ID), Path::new("/opt/cccc"));
            assert_eq!(&built[..base.len()], &base[..], "base altered: {base:?}");
            assert!(
                !built.iter().any(|part| part == SESSION_ID),
                "resume id leaked into {base:?}"
            );
        }
    }

    #[test]
    fn launch_command_respects_a_user_defined_cccc_mcp_server() {
        let base = argv(&[
            "muse",
            "--yolo",
            "-c",
            "mcpServers.cccc.command=\"/custom/cccc\"",
        ]);
        let built = launch_command(&base, None, Path::new("/opt/cccc"));
        assert_eq!(built, base);
        let inline = argv(&["muse", "--config=mcpServers.cccc={}"]);
        assert_eq!(
            launch_command(&inline, None, Path::new("/opt/cccc")),
            inline
        );
    }

    #[test]
    fn flag_values_are_not_mistaken_for_prompts() {
        assert!(!has_positional_or_subcommand(&argv(&[
            "muse",
            "--provider",
            "meta",
            "--model=model-1",
            "-c",
            "tui.x=true",
            "--yolo",
        ])));
        assert!(has_positional_or_subcommand(&argv(&[
            "muse",
            "--provider",
            "meta",
            "hello"
        ])));
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        home: HomeLayout,
        group_id: String,
        workspace: PathBuf,
        data_home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
            home.initialize().expect("initialize");
            let group = GroupStore::new(home.clone())
                .expect("store")
                .create("Muse receipt", "")
                .expect("group");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).expect("workspace");
            let data_home = temp.path().join("xdg-data");
            std::fs::create_dir_all(sessions_root(&data_home).join("2099/01/02"))
                .expect("session store");
            Self {
                _temp: temp,
                home,
                group_id: group.group_id,
                workspace,
                data_home,
            }
        }

        fn environment(&self) -> BTreeMap<String, String> {
            BTreeMap::from([(
                "XDG_DATA_HOME".into(),
                self.data_home.to_string_lossy().into_owned(),
            )])
        }

        fn create_session(&self, session_id: &str) {
            std::fs::create_dir_all(
                sessions_root(&self.data_home)
                    .join("2099/01/02")
                    .join(session_id),
            )
            .expect("session dir");
        }
    }

    fn command() -> Vec<String> {
        argv(&["muse", "--yolo", "--model", "muse-wonder-full-1.0"])
    }

    #[test]
    fn fresh_prepare_marks_pending_then_capture_and_resume_round_trip() {
        let fixture = Fixture::new();
        let mut environment = fixture.environment();
        let first = prepare(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &mut environment,
            Path::new("/opt/cccc"),
        )
        .expect("first prepare");
        assert_eq!(first.session_id, None);
        let stored = read(&fixture.home, &fixture.group_id, "muse-1").expect("marker");
        assert_eq!(stored["status"], "pending_capture");
        assert_eq!(stored["runtime"], "muse");
        assert_eq!(stored["transport"], MANAGED_TRANSPORT);
        assert!(stored.get("runner").is_none());

        fixture.create_session(SESSION_ID);
        let captured = capture_after_launch(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &environment,
        )
        .expect("capture");
        assert_eq!(captured.as_deref(), Some(SESSION_ID));

        let second = prepare(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &mut environment,
            Path::new("/opt/cccc"),
        )
        .expect("resume prepare");
        assert_eq!(second.session_id.as_deref(), Some(SESSION_ID));
        assert!(
            second
                .command
                .windows(2)
                .any(|pair| pair == ["resume", SESSION_ID])
        );
    }

    #[test]
    fn lost_session_store_and_identity_changes_block_resume() {
        let fixture = Fixture::new();
        let mut environment = fixture.environment();
        prepare(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &mut environment,
            Path::new("/opt/cccc"),
        )
        .expect("first prepare");
        fixture.create_session(SESSION_ID);
        capture_after_launch(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &environment,
        )
        .expect("capture");

        // A different data home is a different Muse identity/store.
        let mut changed = fixture.environment();
        changed.insert(
            "XDG_DATA_HOME".into(),
            fixture
                .data_home
                .join("elsewhere")
                .to_string_lossy()
                .into_owned(),
        );
        let prepared = prepare(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &mut changed,
            Path::new("/opt/cccc"),
        )
        .expect("changed identity prepare");
        assert_eq!(prepared.session_id, None);

        // The receipt still names the session, but its store entry is gone.
        let fixture = Fixture::new();
        let mut environment = fixture.environment();
        prepare(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &mut environment,
            Path::new("/opt/cccc"),
        )
        .expect("marker");
        fixture.create_session(SESSION_ID);
        capture_after_launch(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &environment,
        )
        .expect("capture");
        std::fs::remove_dir_all(
            sessions_root(&fixture.data_home)
                .join("2099/01/02")
                .join(SESSION_ID),
        )
        .expect("drop session");
        let prepared = prepare(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &mut environment,
            Path::new("/opt/cccc"),
        )
        .expect("missing session prepare");
        assert_eq!(prepared.session_id, None);
    }

    #[test]
    fn pending_discovery_ignores_sessions_from_before_the_launch() {
        let fixture = Fixture::new();
        fixture.create_session(OTHER_SESSION_ID);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut environment = fixture.environment();
        prepare(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &mut environment,
            Path::new("/opt/cccc"),
        )
        .expect("marker");
        // The pre-existing session predates this launch marker (minus slack
        // it could still be inside the window, so move the marker forward).
        let mut document = read(&fixture.home, &fixture.group_id, "muse-1").expect("marker");
        document.insert("launched_at_unix_ms".into(), json!(now_unix_ms() + 60_000));
        write(&fixture.home, &fixture.group_id, "muse-1", &document).expect("rewrite marker");
        let captured = capture_after_launch(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &environment,
        )
        .expect("capture");
        assert_eq!(captured, None);
    }

    #[test]
    fn prepare_assigns_a_private_data_home_by_default() {
        let fixture = Fixture::new();
        let mut environment = BTreeMap::new();
        prepare(
            &fixture.home,
            &fixture.group_id,
            "muse-1",
            &fixture.workspace,
            &command(),
            &mut environment,
            Path::new("/opt/cccc"),
        )
        .expect("prepare");
        let assigned = environment.get("XDG_DATA_HOME").expect("assigned");
        assert!(assigned.ends_with("muse/muse-1/data"), "{assigned}");
        assert!(Path::new(assigned).is_dir());
    }
}
