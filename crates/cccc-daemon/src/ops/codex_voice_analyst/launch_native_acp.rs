use super::*;
use serde_json::json;
use std::path::Path;

impl AnalystSession {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn launch_native_acp(
        home: &HomeLayout,
        binding: WorkspaceBinding,
        runtime: ActorRuntime,
        command: Vec<String>,
        env: BTreeMap<String, String>,
        resume: Option<String>,
        purpose: SessionPurpose,
        actor: Option<(&str, &str)>,
    ) -> io::Result<Self> {
        Self::launch_native_acp_inner(
            home, binding, runtime, command, env, resume, purpose, actor, None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn launch_native_acp_resume_only(
        home: &HomeLayout,
        binding: WorkspaceBinding,
        runtime: ActorRuntime,
        command: Vec<String>,
        env: BTreeMap<String, String>,
        actor: (&str, &str),
        required_id: String,
    ) -> io::Result<Self> {
        Self::launch_native_acp_inner(
            home,
            binding,
            runtime,
            command,
            env,
            None,
            SessionPurpose::Actor,
            Some(actor),
            Some(required_id),
        )
        .await
    }

    /// Read the same receipt and identity the native launcher will use, without changing either.
    pub(crate) fn native_acp_resume_id(
        home: &HomeLayout,
        config: &ActorLaunchConfig,
    ) -> io::Result<Option<String>> {
        if !Self::supports_memory_restart(config.runtime, config.runtime_mode) {
            return Ok(None);
        }
        native_acp::options(config.runtime, &config.command)?;
        let mut env = config.environment.clone();
        let identity =
            native_acp_identity(config.runtime, &config.workdir, &config.command, &mut env);
        let path = super::super::runtime_session::native_acp::receipt_path(
            home,
            &config.group_id,
            &config.actor_id,
            config.runtime,
        )?;
        super::super::runtime_session::native_acp::prepare(&path, &identity)
    }

    #[allow(clippy::too_many_arguments)]
    async fn launch_native_acp_inner(
        home: &HomeLayout,
        binding: WorkspaceBinding,
        runtime: ActorRuntime,
        command: Vec<String>,
        mut env: BTreeMap<String, String>,
        resume: Option<String>,
        purpose: SessionPurpose,
        actor: Option<(&str, &str)>,
        mut required_id: Option<String>,
    ) -> io::Result<Self> {
        let options = native_acp::options(runtime, &command)?;
        let private_values = env
            .values()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect();
        let identity = native_acp_identity(runtime, &binding.root, &command, &mut env);
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
            super::super::runtime_session::native_acp::prepare(path, &identity).map_err(
                |error| {
                    if required_id.is_some() {
                        super::super::actor_memory::blocked_resume(
                            "Could not verify the required native ACP receipt",
                        )
                    } else {
                        error
                    }
                },
            )?
        } else {
            resume
        };
        if let Some((group, actor)) = actor {
            let persistent = super::super::actor_memory::required_resume(home, group, actor)
                .map_err(|_| {
                    super::super::actor_memory::blocked_resume(
                        "Could not verify the memory-maintenance resume requirement",
                    )
                })?;
            if let Some(persistent) = persistent {
                if persistent.runtime != runtime
                    || required_id
                        .as_deref()
                        .is_some_and(|id| id != persistent.session_id)
                {
                    return Err(super::super::actor_memory::blocked_resume(
                        "The required native ACP session changed before launch",
                    ));
                }
                required_id = Some(persistent.session_id);
            }
        }
        if let Some(required) = required_id.as_deref()
            && resume.as_deref() != Some(required)
        {
            return Err(super::super::actor_memory::blocked_resume(
                "Memory restart lost the required native ACP session receipt; no new session was started",
            ));
        }
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
        // Resolve npm .cmd/.bat shims using the same PATH/PATHEXT rules as
        // runtime detection. std::process::Command handles batch-file quoting;
        // the portable-pty cmd wrapper is not appropriate for this stdio path.
        let launch_command = cccc_runtime::resolve_command_executable(&launch_command, &env);
        let (process, stdin, stdout) =
            process::spawn_piped_quiet(&launch_command, &binding.root, &env)?;
        let protocol = AcpClient::new_with_private_values(
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
            private_values,
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
        if let Some(required) = required_id.as_deref()
            && (!resumed || id != required)
        {
            protocol.close().await;
            process.stop()?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Memory restart did not load the required native ACP session",
            ));
        }
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

fn native_acp_identity(
    runtime: ActorRuntime,
    cwd: &Path,
    command: &[String],
    env: &mut BTreeMap<String, String>,
) -> Value {
    if runtime == ActorRuntime::Auggie {
        env.insert("AUGMENT_DISABLE_AUTO_UPDATE".into(), "1".into());
    }
    if runtime == ActorRuntime::Hermes {
        env.insert("HERMES_ACP_SKIP_CONFIGURED_MCP".into(), "1".into());
    }
    let command = if command.is_empty() {
        cccc_runtime::default_command(runtime)
    } else {
        command.to_vec()
    };
    super::super::runtime_session::native_acp::identity(cwd, &command, env)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_preflight_uses_native_provider_environment_policy_without_writing() {
        for (runtime, flag) in [
            (ActorRuntime::Auggie, "AUGMENT_DISABLE_AUTO_UPDATE"),
            (ActorRuntime::Hermes, "HERMES_ACP_SKIP_CONFIGURED_MCP"),
        ] {
            let temp = tempfile::tempdir().expect("fixture");
            let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
            let group = cccc_core::GroupStore::new(home.clone())
                .expect("store")
                .create("native policy", "")
                .expect("group");
            let config = ActorLaunchConfig {
                workdir: temp.path().to_path_buf(),
                group_id: group.group_id,
                actor_id: "worker".into(),
                runtime,
                runtime_mode: RuntimeMode::Acp,
                command: Vec::new(),
                environment: BTreeMap::new(),
            };
            let expected_env = BTreeMap::from([(flag.into(), "1".into())]);
            let identity = super::super::super::runtime_session::native_acp::identity(
                &config.workdir,
                &cccc_runtime::default_command(runtime),
                &expected_env,
            );
            let path = super::super::super::runtime_session::native_acp::receipt_path(
                &home,
                &config.group_id,
                &config.actor_id,
                runtime,
            )
            .expect("receipt path");
            super::super::super::runtime_session::native_acp::record(
                &path,
                &identity,
                "saved-session",
                true,
            )
            .expect("record native receipt");
            let before = std::fs::read(&path).expect("before");
            assert_eq!(
                AnalystSession::native_acp_resume_id(&home, &config)
                    .expect("preflight")
                    .as_deref(),
                Some("saved-session")
            );
            assert_eq!(
                std::fs::read(&path).expect("after"),
                before,
                "memory preflight must remain read-only"
            );
        }
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
