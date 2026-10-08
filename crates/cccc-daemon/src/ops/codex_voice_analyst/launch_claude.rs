use super::*;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::io;

impl AnalystSession {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn launch_claude(
        home: &HomeLayout,
        binding: WorkspaceBinding,
        command: Vec<String>,
        mut environment: BTreeMap<String, String>,
        requested_session_id: Option<String>,
        purpose: SessionPurpose,
        actor: Option<(&str, &str)>,
    ) -> io::Result<Self> {
        let generation = uuid::Uuid::new_v4().simple().to_string();
        let detach =
            purpose == SessionPurpose::Actor && cccc_core::settings::detach_claude_on_exit(home)?;
        if detach && !super::super::runtime_session::resume_enabled() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Claude detach requires CCCC_RUNTIME_RESUME to be enabled",
            ));
        }
        let cccc =
            super::super::codex_mcp::configure_actor_cli(&mut environment).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "CCCC executable is unavailable for Claude MCP binding",
                )
            })?;
        environment.insert(
            "CCCC_HOME".into(),
            home.root().to_string_lossy().into_owned(),
        );
        let (group_id, actor_id, tool_profile, settings_key) = actor.map_or_else(
            || ("", "user", Some("full"), "voice-analyst".to_owned()),
            |(group_id, actor_id)| {
                (
                    group_id,
                    actor_id,
                    None,
                    format!("actor:{group_id}:{actor_id}"),
                )
            },
        );
        environment.insert("CCCC_GROUP_ID".into(), group_id.into());
        environment.insert("CCCC_ACTOR_ID".into(), actor_id.into());
        if let Some(tool_profile) = tool_profile {
            environment.insert("CCCC_MCP_TOOL_PROFILE".into(), tool_profile.into());
        }
        let mut mcp_environment = serde_json::Map::from_iter([
            (
                "CCCC_HOME".into(),
                json!(home.root().to_string_lossy().into_owned()),
            ),
            ("CCCC_GROUP_ID".into(), json!(group_id)),
            ("CCCC_ACTOR_ID".into(), json!(actor_id)),
        ]);
        if let Some(tool_profile) = tool_profile {
            mcp_environment.insert("CCCC_MCP_TOOL_PROFILE".into(), json!(tool_profile));
        }
        if actor.is_none()
            && let Some(origin) = environment.get(cccc_core::voice_notifications::ORIGIN_ENV)
        {
            mcp_environment.insert(
                cccc_core::voice_notifications::ORIGIN_ENV.into(),
                json!(origin),
            );
        }
        let mcp_server = json!({
            "command":cccc,
            "args":["mcp"],
            "env":mcp_environment,
        });
        let session_command = command.clone();
        if detach && let Some((group_id, actor_id)) = actor {
            super::super::runtime_session::claude_ownership::check_launch(
                home, group_id, actor_id,
            )?;
        }
        let resume_attempt = if let Some((group_id, actor_id)) = actor {
            super::super::runtime_session::prepare_claude_managed_session(
                home,
                group_id,
                actor_id,
                &binding.root,
                &session_command,
                &environment,
            )?
        } else {
            None
        };
        let resume_session_id = resume_attempt
            .as_ref()
            .map(|attempt| attempt.session_id.as_str())
            .or(requested_session_id.as_deref());
        if detach
            && resume_session_id.is_none()
            && let Some((group_id, actor_id)) = actor
            && let Some(saved) = super::super::runtime_session::snapshot(home, group_id, actor_id)?
            && saved.get("transport").and_then(Value::as_str) == Some("claude_agent_view")
            && saved
                .get("provider_session_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
        {
            return Err(io::Error::other(super::super::runtime_session::ClaudeResumeBlocked(
                "Claude detach cannot replace a saved session whose launch identity no longer matches; restore its configuration or choose New Session".into(),
            )));
        }
        if detach
            && let Some((group_id, actor_id)) = actor
            && let Some(id) = resume_session_id
        {
            super::super::runtime_session::claude_ownership::record(
                home,
                group_id,
                actor_id,
                &claude::claude_config_dir(&environment)?,
                &binding.root,
                id,
                false,
            )?;
        }
        let result = match claude::prepare(
            home,
            &command,
            &environment,
            &binding.root,
            &settings_key,
            purpose,
            mcp_server,
        ) {
            Ok(mut prepared) => {
                if detach && let Some((group_id, actor_id)) = actor {
                    prepared.launch_owner = Some(
                        super::super::runtime_session::claude_ownership::LaunchOwnership {
                            home: home.clone(),
                            group_id: group_id.into(),
                            actor_id: actor_id.into(),
                        },
                    );
                }
                // Transcript validation holds a large future. Keep it off the
                // managed worker's stack, including unoptimized native builds.
                Box::pin(claude::launch(
                    prepared,
                    &binding.root,
                    &generation,
                    purpose,
                    resume_session_id,
                ))
                .await
            }
            Err(error) => Err(error),
        };
        let launched = match result {
            Ok(launched) => launched,
            Err(error) => {
                if detach && let Some((group_id, actor_id)) = actor {
                    super::super::runtime_session::claude_ownership::check_launch(
                        home, group_id, actor_id,
                    )?;
                }
                if let Some((group_id, actor_id)) = actor
                    && let Some(attempt) = resume_attempt.as_ref()
                    && let Some((diagnostic, blocked)) = claude::resume_diagnostic(&error)
                {
                    super::super::runtime_session::record_claude_resume_failure(
                        home, group_id, actor_id, attempt, diagnostic, blocked,
                    )?;
                    return Err(if blocked {
                        io::Error::other(super::super::runtime_session::ClaudeResumeBlocked(
                            diagnostic.into(),
                        ))
                    } else {
                        io::Error::new(error.kind(), diagnostic)
                    });
                }
                return Err(error);
            }
        };
        if detach && let Some((group_id, actor_id)) = actor {
            // Record original ownership before publishing the session/viewer.
            if let Err(error) = super::super::runtime_session::claude_ownership::record(
                home,
                group_id,
                actor_id,
                &claude::claude_config_dir(&environment)?,
                &binding.root,
                &launched.session_id,
                true,
            ) {
                if launched.resumed {
                    launched.protocol.detach().await;
                } else {
                    launched.protocol.close().await?;
                }
                return Err(error);
            }
        }
        if let Some((group_id, actor_id)) = actor
            && let Err(error) = super::super::runtime_session::record_claude_managed_session(
                home,
                group_id,
                actor_id,
                &binding.root,
                &session_command,
                &environment,
                &launched.session_id,
                launched.resumed,
            )
        {
            if detach {
                if launched.resumed {
                    launched.protocol.detach().await;
                } else if let Err(cleanup) = launched.protocol.close().await {
                    return Err(io::Error::other(format!(
                        "failed to persist Claude session: {error}; exact-job rollback also failed: {cleanup}"
                    )));
                }
                if !launched.resumed {
                    super::super::runtime_session::claude_ownership::remove(
                        home, group_id, actor_id,
                    )?;
                }
                return Err(error);
            }
            tracing::warn!(%error, %group_id, %actor_id, "failed to persist Claude managed session");
        }
        if detach && let Some((group_id, actor_id)) = actor {
            if let Err(error) = super::super::runtime_session::claude_ownership::record(
                home,
                group_id,
                actor_id,
                &claude::claude_config_dir(&environment)?,
                &binding.root,
                &launched.session_id,
                false,
            ) {
                if launched.resumed {
                    launched.protocol.detach().await;
                } else {
                    launched.protocol.close().await?;
                    super::super::runtime_session::claude_ownership::remove(
                        home, group_id, actor_id,
                    )?;
                }
                return Err(error);
            }
        }
        Ok(Self {
            #[cfg(test)]
            binding,
            generation,
            runtime: cccc_contracts::ActorRuntime::Claude,
            endpoint: String::new(),
            thread_id: launched.session_id,
            remote_tui_prefix: Vec::new(),
            environment: launched.environment,
            protocol: ManagedProtocol::Claude(launched.protocol),
            process: None,
            auxiliary_processes: Vec::new(),
            native_tui_command: Some(launched.tui_command),
            cleanup_paths: launched.cleanup_paths,
            thread_resumed: launched.resumed,
            delegations: tokio::sync::Mutex::new(HashMap::new()),
        })
    }
}
