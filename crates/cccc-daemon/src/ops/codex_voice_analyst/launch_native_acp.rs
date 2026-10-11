use super::*;
use serde_json::json;

impl AnalystSession {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn launch_native_acp(
        home: &HomeLayout,
        binding: WorkspaceBinding,
        runtime: ActorRuntime,
        command: Vec<String>,
        mut env: BTreeMap<String, String>,
        resume: Option<String>,
        purpose: SessionPurpose,
        actor: Option<(&str, &str)>,
    ) -> io::Result<Self> {
        let options = native_acp::options(runtime, &command)?;
        let identity_command = if command.is_empty() {
            if runtime == ActorRuntime::Cline {
                vec!["cline".into()]
            } else {
                cccc_runtime::default_command(runtime)
            }
        } else {
            command.clone()
        };
        let identity = super::super::runtime_session::native_acp::identity(
            &binding.root,
            &identity_command,
            &env,
        );
        let cccc = super::super::codex_mcp::configure_actor_cli(&mut env);
        #[cfg(test)]
        let cccc = cccc.or_else(|| env.remove("ACP_FIXTURE_CLI").map(PathBuf::from));
        let cccc = cccc.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "CCCC executable is unavailable for ACP MCP binding",
            )
        })?;
        let (group, actor_id, profile) =
            actor.map_or(("", "user", Some("full")), |(g, a)| (g, a, None));
        let mut mcp = acp_mcp_server(
            home,
            &cccc,
            group,
            actor_id,
            task_tool_profile(purpose, profile),
        );
        add_voice_mcp_origin(&mut mcp, &env, purpose);
        let receipt_path = actor
            .map(|(g, a)| {
                super::super::runtime_session::native_acp::receipt_path(home, g, a, runtime)
            })
            .transpose()?;
        let resume = if let Some(path) = &receipt_path {
            super::super::runtime_session::native_acp::prepare(path, &identity)?
        } else {
            resume
        };
        let generation = uuid::Uuid::new_v4().simple().to_string();
        let mut launch_command = options.command.clone();
        if runtime == ActorRuntime::Copilot {
            if purpose == SessionPurpose::VoiceSecretary {
                let executable = cccc_runtime::resolve_command_executable(&launch_command, &env);
                let mut catalog = tokio::process::Command::new(&executable[0]);
                catalog
                    .args(["--no-auto-update", "mcp", "list", "--json"])
                    .current_dir(&binding.root)
                    .envs(&env)
                    .kill_on_drop(true);
                let output =
                    tokio::time::timeout(std::time::Duration::from_secs(10), catalog.output())
                        .await
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::TimedOut,
                                "Copilot MCP catalog inspection timed out",
                            )
                        })??;
                if !output.status.success() {
                    return Err(io::Error::other(
                        "Copilot MCP catalog inspection failed; no secretary prompt was sent",
                    ));
                }
                let catalog: Value = serde_json::from_slice(&output.stdout)
                    .map_err(|_| io::Error::other("Copilot returned an invalid MCP catalog"))?;
                restrict_secretary_copilot_mcp(&mut launch_command, &catalog)?;
            }
            // Process-local stdio MCP injection: the official ACP session
            // mcpServers surface currently accepts remote servers only.
            let env = mcp["env"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    Some((entry["name"].as_str()?.to_owned(), entry["value"].clone()))
                })
                .collect::<serde_json::Map<String, Value>>();
            let config = json!({"mcpServers":{"cccc":{"type":"local","command":mcp["command"],"args":mcp["args"],"env":env,"tools":["*"]}}});
            // This configuration contains only the CCCC-scoped transport
            // identity, no provider credentials and no global config writes.
            launch_command.extend([
                "--additional-mcp-config".into(),
                serde_json::to_string(&config).map_err(io::Error::other)?,
            ]);
        }
        // cline needs per-actor state isolation. Its `--data-dir` flag is
        // ignored in `--acp` mode, so the only isolation surface is the
        // process environment: a per-actor CLINE_DATA_DIR and the native
        // local backend keep persistent state isolated and RAM in the owned
        // process tree. CLINE_PROVIDER/CLINE_MODEL carry provider/model intent,
        // because cline's ACP session reads those env vars and not the flags.
        // MCP does NOT ride the ACP session/mcpServers surface: cline
        // validates that field and never launches it, so the CCCC entry is
        // written to the per-actor settings file at
        // <CLINE_DATA_DIR>/settings/cline_mcp_settings.json instead.
        if runtime == ActorRuntime::Cline {
            prepare_cline_isolation(
                home,
                &mut env,
                actor,
                options.provider.as_deref(),
                options.model.as_deref(),
            );
            write_cline_mcp_settings(&env, &mcp)?;
        }
        // Resolve npm .cmd/.bat shims using the same PATH/PATHEXT rules as
        // runtime detection. std::process::Command handles batch-file quoting;
        // the portable-pty cmd wrapper is not appropriate for this stdio path.
        let launch_command = cccc_runtime::resolve_command_executable(&launch_command, &env);
        let (process, stdin, stdout) =
            process::spawn_piped_quiet(&launch_command, &binding.root, &env)?;
        let protocol = AcpClient::new(
            stdin,
            stdout,
            generation.clone(),
            native_acp::name(runtime),
            if options.unrestricted {
                acp::PermissionPolicy::AllowOnce
            } else {
                acp::PermissionPolicy::Interactive
            },
            acp::PromptCompletion::ResponseWithActivityReceipt,
        )?;
        let initialized = native_acp::initialize(
            &protocol,
            runtime,
            &binding.root,
            resume.as_deref(),
            mcp,
            &options,
        )
        .await;
        let (id, resumed) = match initialized {
            Ok(result) => result,
            Err(error) => {
                protocol.close().await;
                process.stop()?;
                // A resume failure already carries its own diagnosis and names
                // the recovery. Re-wrapping it as an initialization failure
                // would discard exactly the information that makes it
                // actionable, so pass that one through unchanged.
                let message = if error.to_string().contains(native_acp::RESUME_RECOVERY_HINT) {
                    error.to_string()
                } else {
                    format!(
                        "{} ACP initialization failed; check native CLI login and launch options. Attempted sessions are never replaced automatically: {error}",
                        native_acp::name(runtime)
                    )
                };
                return Err(io::Error::new(error.kind(), message));
            }
        };
        if purpose != SessionPurpose::VoiceSecretary {
            let receipt = if let Some(path) = receipt_path {
                if let Err(error) = super::super::runtime_session::native_acp::record(
                    &path, &identity, &id, resumed,
                ) {
                    protocol.close().await;
                    process.stop()?;
                    return Err(error);
                }
                super::super::runtime_session::native_acp::PromptReceipt::actor(
                    path,
                    id.clone(),
                    resumed,
                )
            } else {
                super::super::runtime_session::native_acp::PromptReceipt::voice(
                    home,
                    id.clone(),
                    resumed,
                )
            };
            protocol.set_prompt_receipt(receipt);
        }
        Ok(Self {
            #[cfg(test)]
            binding,
            generation,
            runtime,
            endpoint: String::new(),
            thread_id: id,
            remote_tui_prefix: Vec::new(),
            environment: env,
            protocol: ManagedProtocol::Acp(protocol),
            process: Some(Arc::new(process)),
            auxiliary_processes: Vec::new(),
            native_tui_command: None,
            cleanup_paths: Vec::new(),
            thread_resumed: resumed,
            delegations: tokio::sync::Mutex::new(HashMap::new()),
        })
    }
}

fn restrict_secretary_copilot_mcp(command: &mut Vec<String>, catalog: &Value) -> io::Result<()> {
    let servers = catalog["mcpServers"]
        .as_object()
        .ok_or_else(|| io::Error::other("Copilot returned an unsupported MCP catalog"))?;
    for name in servers.keys().filter(|name| name.as_str() != "cccc") {
        command.extend(["--disable-mcp-server".into(), name.clone()]);
    }
    if !command.iter().any(|arg| arg == "--disable-builtin-mcps") {
        command.push("--disable-builtin-mcps".into());
    }
    command.extend(["--enable-mcp-server".into(), "cccc".into()]);
    if !command.iter().any(|arg| arg == "--no-custom-instructions") {
        command.push("--no-custom-instructions".into());
    }
    Ok(())
}

/// Per-actor state isolation for cline. The `--data-dir` flag is ignored in
/// `--acp` mode, so the environment is the only isolation surface: cline
/// resolves every persistent path from `CLINE_DATA_DIR` (the `oe()` accessor),
/// and the native local backend keeps execution in the managed process tree
/// rather than a detached hub. No shared hub endpoint is used. The
/// provider/model intent is passed through as `CLINE_PROVIDER`/`CLINE_MODEL`
/// because cline's ACP session reads those env vars, not the `-P`/`-m` flags
/// (measured on 3.0.70: with `-P` but no env every prompt went to the default
/// provider). MCP configuration is written to the per-actor settings file
/// separately — cline's ACP `session/new` mcpServers field is validated and
/// then ignored, so it never launches a child.
fn prepare_cline_isolation(
    home: &HomeLayout,
    env: &mut BTreeMap<String, String>,
    actor: Option<(&str, &str)>,
    provider: Option<&str>,
    model: Option<&str>,
) {
    let (group, actor_id) = actor.unwrap_or(("", "user"));
    // A stable per-actor directory under the CCCC home; never shared between
    // two actors, and never the process-global ~/.cline default.
    let data_dir = home.root().join("cline").join(if group.is_empty() {
        actor_id.to_owned()
    } else {
        format!("{group}--{actor_id}")
    });
    env.insert(
        "CLINE_DATA_DIR".into(),
        data_dir.to_string_lossy().into_owned(),
    );
    // A finite port hash cannot isolate arbitrarily many actors. Local mode
    // avoids both port collisions and detached, unattributed hub processes.
    env.insert("CLINE_SESSION_BACKEND_MODE".into(), "local".into());
    env.retain(|key, _| !key.starts_with("CLINE_HUB_"));
    env.insert("CLINE_NO_INTERACTIVE".into(), "1".into());
    // Only set provider/model when the card actually declared one; otherwise
    // cline falls back to its own restored auth, which is the correct default.
    if let Some(provider) = provider {
        env.insert("CLINE_PROVIDER".into(), provider.to_owned());
    }
    if let Some(model) = model {
        env.insert("CLINE_MODEL".into(), model.to_owned());
    }
}

/// cline's ACP `session/new` mcpServers field is inert (validated, stored on
/// the session object, never launched), so the only surface that produces a
/// CCCC MCP child is the per-actor `cline_mcp_settings.json`. This writes the
/// CCCC entry into `<CLINE_DATA_DIR>/settings/cline_mcp_settings.json`,
/// merging so a card's other servers survive, in cline's own
/// `mcpServers.<name>.transport` shape. The entry carries the CCCC-scoped
/// transport identity (home/group/actor), never provider credentials.
fn write_cline_mcp_settings(env: &BTreeMap<String, String>, mcp: &Value) -> io::Result<()> {
    let Some(data_dir) = env.get("CLINE_DATA_DIR") else {
        return Ok(());
    };
    let path = std::path::Path::new(data_dir).join("settings/cline_mcp_settings.json");
    let mut document = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error),
    };
    let servers = document
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Cline MCP settings must be an object",
            )
        })?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    let servers = servers.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Cline mcpServers must be an object",
        )
    })?;
    let environment = mcp["env"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| Some((entry["name"].as_str()?.to_owned(), entry["value"].clone())))
        .collect::<serde_json::Map<String, Value>>();
    servers.insert(
        "cccc".into(),
        json!({
            "transport": {
                "type": "stdio",
                "command": mcp["command"],
                "args": mcp["args"],
                "env": environment,
            },
            "disabled": false,
        }),
    );
    cccc_core::fs::write_json(&path, &document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn cline_local_backend_overrides_configured_foreign_hub() {
        let temp = tempfile::tempdir().expect("isolated home");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("foreign hub port");
        let mut env = BTreeMap::from([
            ("CLINE_SESSION_BACKEND_MODE".into(), "remote".into()),
            (
                "CLINE_HUB_PORT".into(),
                listener.local_addr().expect("port").port().to_string(),
            ),
            ("CLINE_HUB_HOST".into(), "foreign.example".into()),
            ("CLINE_HUB_PATHNAME".into(), "/foreign".into()),
            (
                "CLINE_HUB_ADDRESS".into(),
                "http://foreign.example/hub".into(),
            ),
            (
                "CLINE_HUB_DISCOVERY_PATH".into(),
                "/foreign/discovery.json".into(),
            ),
        ]);
        prepare_cline_isolation(&home, &mut env, Some(("group", "alpha")), None, None);
        assert_eq!(
            env.get("CLINE_SESSION_BACKEND_MODE").map(String::as_str),
            Some("local")
        );
        assert!(
            env.keys().all(|key| !key.starts_with("CLINE_HUB_")),
            "managed lane must not retain a configured hub endpoint"
        );
        let mut beta = BTreeMap::new();
        prepare_cline_isolation(&home, &mut beta, Some(("group", "beta")), None, None);
        assert_ne!(env.get("CLINE_DATA_DIR"), beta.get("CLINE_DATA_DIR"));
        assert_eq!(
            beta.get("CLINE_SESSION_BACKEND_MODE").map(String::as_str),
            Some("local")
        );
    }

    #[test]
    fn cline_isolation_is_per_actor_and_stable() {
        let home = HomeLayout::from_path("/tmp/cccc-cline-test").expect("home layout");
        let mut a = BTreeMap::new();
        let mut b = BTreeMap::new();
        prepare_cline_isolation(&home, &mut a, Some(("g", "actor_a")), None, None);
        prepare_cline_isolation(&home, &mut b, Some(("g", "actor_b")), None, None);
        let dir_a = a.get("CLINE_DATA_DIR").expect("actor a data dir");
        let dir_b = b.get("CLINE_DATA_DIR").expect("actor b data dir");
        assert_ne!(dir_a, dir_b, "two actors must not share a data dir");
        assert_eq!(
            a.get("CLINE_SESSION_BACKEND_MODE").map(String::as_str),
            Some("local")
        );
        assert_eq!(
            b.get("CLINE_SESSION_BACKEND_MODE").map(String::as_str),
            Some("local")
        );
        assert!(!a.contains_key("CLINE_HUB_PORT"));
        assert!(!b.contains_key("CLINE_HUB_PORT"));
        // Stable for the same actor across relaunches.
        let mut again = BTreeMap::new();
        prepare_cline_isolation(&home, &mut again, Some(("g", "actor_a")), None, None);
        assert_eq!(again.get("CLINE_DATA_DIR"), Some(dir_a));
        assert_eq!(
            again.get("CLINE_SESSION_BACKEND_MODE"),
            a.get("CLINE_SESSION_BACKEND_MODE")
        );
    }

    #[test]
    fn cline_provider_and_model_reach_the_launch_env_not_only_the_flags() {
        // cline's ACP session reads CLINE_PROVIDER/CLINE_MODEL from the
        // environment; the -P/-m flags do not feed session creation on
        // 3.0.70. A card that declares them must get them in the env.
        let home = HomeLayout::from_path("/tmp/cccc-cline-test").expect("home layout");
        let mut env = BTreeMap::new();
        prepare_cline_isolation(
            &home,
            &mut env,
            Some(("g", "actor_a")),
            Some("openai-compatible"),
            Some("stepfun5-free"),
        );
        assert_eq!(
            env.get("CLINE_PROVIDER").map(String::as_str),
            Some("openai-compatible")
        );
        assert_eq!(
            env.get("CLINE_MODEL").map(String::as_str),
            Some("stepfun5-free")
        );
        // A card that declares neither must not force cline off its own
        // restored auth: the keys are absent, not empty.
        let mut bare = BTreeMap::new();
        prepare_cline_isolation(&home, &mut bare, Some(("g", "actor_a")), None, None);
        assert!(!bare.contains_key("CLINE_PROVIDER"));
        assert!(!bare.contains_key("CLINE_MODEL"));
    }

    #[test]
    fn cline_mcp_settings_writes_the_cccc_entry_in_clines_own_shape() {
        // cline's ACP session/new mcpServers field is inert, so the settings
        // file is the only surface that launches a CCCC MCP child. It must
        // land under the per-actor data dir and merge, not clobber.
        let home = HomeLayout::from_path("/tmp/cccc-cline-settings-test").expect("home layout");
        let mut env = BTreeMap::new();
        prepare_cline_isolation(&home, &mut env, Some(("g", "actor_a")), None, None);
        let data_dir = env.get("CLINE_DATA_DIR").expect("data dir").clone();
        let settings = std::path::Path::new(&data_dir).join("settings/cline_mcp_settings.json");
        // A pre-existing card server must survive the write.
        std::fs::create_dir_all(settings.parent().expect("settings parent")).expect("mkdir");
        std::fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "mcpServers": {"card-extra": {"transport": {"type": "stdio", "command": "/bin/true"}}}
            }))
            .expect("encode"),
        )
        .expect("seed");
        let mcp = json!({
            "name": "cccc",
            "command": "/opt/cccc/bin/cccc",
            "args": ["mcp"],
            "env": [
                {"name": "CCCC_HOME", "value": home.root().to_string_lossy()},
                {"name": "CCCC_GROUP_ID", "value": "g"},
                {"name": "CCCC_ACTOR_ID", "value": "actor_a"},
            ],
        });
        write_cline_mcp_settings(&env, &mcp).expect("write settings");
        let document: Value =
            serde_json::from_slice(&std::fs::read(&settings).expect("read back")).expect("parse");
        let entry = &document["mcpServers"]["cccc"];
        assert_eq!(entry["disabled"], json!(false));
        assert_eq!(entry["transport"]["type"], json!("stdio"));
        assert_eq!(entry["transport"]["command"], json!("/opt/cccc/bin/cccc"));
        assert_eq!(entry["transport"]["args"], json!(["mcp"]));
        assert_eq!(entry["transport"]["env"]["CCCC_ACTOR_ID"], json!("actor_a"));
        // The card's own server is preserved.
        assert_eq!(
            document["mcpServers"]["card-extra"]["transport"]["command"],
            json!("/bin/true")
        );
        // Two actors never share a settings file.
        let mut other = BTreeMap::new();
        prepare_cline_isolation(&home, &mut other, Some(("g", "actor_b")), None, None);
        assert_ne!(other.get("CLINE_DATA_DIR"), Some(&data_dir));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn cline_isolation_removes_configured_hub_overrides() {
        let home = HomeLayout::from_path("/tmp/cccc-cline-test").expect("home layout");
        let mut env = BTreeMap::new();
        env.insert("CLINE_HUB_HOST".into(), "10.0.0.1".into());
        env.insert("CLINE_HUB_PATHNAME".into(), "/leak".into());
        prepare_cline_isolation(&home, &mut env, Some(("g", "actor_a")), None, None);
        assert!(!env.contains_key("CLINE_HUB_HOST"));
        assert!(!env.contains_key("CLINE_HUB_PATHNAME"));
        assert_eq!(
            env.get("CLINE_NO_INTERACTIVE").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn cline_mcp_settings_preserves_invalid_existing_data() {
        let temp = tempfile::tempdir().expect("home");
        let home = HomeLayout::from_path(temp.path()).expect("layout");
        let mut env = BTreeMap::new();
        prepare_cline_isolation(&home, &mut env, Some(("group", "subject")), None, None);
        let path = std::path::Path::new(env.get("CLINE_DATA_DIR").expect("directory"))
            .join("settings/cline_mcp_settings.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("settings");
        for bytes in [
            b"{".as_slice(),
            b"[]",
            b"{\"mcpServers\":[],\"retained\":true}",
        ] {
            std::fs::write(&path, bytes).expect("seed invalid settings");
            assert!(
                write_cline_mcp_settings(&env, &json!({"command":"/bin/true","args":[],"env":[]}))
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).expect("unchanged settings"), bytes);
        }
    }

    #[test]
    #[cfg(unix)]
    fn cline_mcp_settings_preserves_unreadable_existing_data() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("home");
        let mut env = BTreeMap::new();
        env.insert(
            "CLINE_DATA_DIR".into(),
            temp.path().to_string_lossy().into_owned(),
        );
        let path = temp.path().join("settings/cline_mcp_settings.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("settings");
        let retained = b"{\"mcpServers\":{\"private-server\":{}}}";
        std::fs::write(&path, retained).expect("seed settings");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200))
            .expect("write only");
        // Root bypasses the read bit; this fixture runs as ghrunner on claw.
        let unreadable = std::fs::read(&path).is_err();
        let result = if unreadable {
            Some(write_cline_mcp_settings(
                &env,
                &json!({"command":"/bin/true","args":[],"env":[]}),
            ))
        } else {
            None
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("restore read access");
        if let Some(result) = result {
            assert!(result.is_err());
        }
        assert_eq!(std::fs::read(&path).expect("retained data"), retained);
    }

    #[test]
    fn secretary_disables_inherited_copilot_servers_without_changing_yolo() {
        let mut command = vec!["copilot".into(), "--allow-all".into(), "--acp".into()];
        restrict_secretary_copilot_mcp(
            &mut command,
            &json!({"mcpServers":{"cccc":{},"user-extra":{},"plugin-extra":{}}}),
        )
        .expect("restrict catalog");
        assert!(command.iter().any(|arg| arg == "--allow-all"));
        assert!(
            command
                .windows(2)
                .any(|args| args == ["--disable-mcp-server", "user-extra"])
        );
        assert!(
            command
                .windows(2)
                .any(|args| args == ["--disable-mcp-server", "plugin-extra"])
        );
        assert!(
            !command
                .windows(2)
                .any(|args| args == ["--disable-mcp-server", "cccc"])
        );
        assert!(restrict_secretary_copilot_mcp(&mut command, &json!({})).is_err());
    }
}
