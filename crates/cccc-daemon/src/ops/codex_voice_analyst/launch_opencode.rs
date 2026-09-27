use super::*;
use std::collections::{BTreeMap, HashMap};
use std::io;

impl AnalystSession {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn launch_opencode(
        runtime: ActorRuntime,
        home: &HomeLayout,
        binding: WorkspaceBinding,
        command: Vec<String>,
        mut env: BTreeMap<String, String>,
        requested_session_id: Option<String>,
        purpose: SessionPurpose,
        actor: Option<(&str, &str)>,
    ) -> io::Result<Self> {
        let generation = uuid::Uuid::new_v4().simple().to_string();
        let cccc = super::super::codex_mcp::configure_actor_cli(&mut env).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "CCCC executable is unavailable for OpenCode/Kilo MCP binding",
            )
        })?;
        env.insert(
            "CCCC_HOME".into(),
            home.root().to_string_lossy().into_owned(),
        );
        let (group_id, actor_id, tool_profile) = actor
            .map_or(("", "user", Some("full")), |(group_id, actor_id)| {
                (group_id, actor_id, None)
            });
        env.insert("CCCC_GROUP_ID".into(), group_id.into());
        env.insert("CCCC_ACTOR_ID".into(), actor_id.into());
        if let Some(profile) = tool_profile {
            env.insert("CCCC_MCP_TOOL_PROFILE".into(), profile.into());
        }
        let mut mcp_server = acp_mcp_server(home, &cccc, group_id, actor_id, tool_profile);
        add_voice_mcp_origin(&mut mcp_server, &env, purpose);
        let session_command = command.clone();
        opencode::prepare(&command, &env, runtime)?;
        let resume_session_id = if let Some((group_id, actor_id)) = actor {
            super::super::runtime_session::prepare_opencode_managed_session(
                runtime,
                home,
                group_id,
                actor_id,
                &binding.root,
                &session_command,
                &env,
            )?
        } else {
            requested_session_id
        };
        // RS-1: a failed ACP resume poisons that session id and gets one fresh attempt.
        let fallback_context = actor.map(|(group_id, actor_id)| resume_fallback::FallbackContext {
            home,
            group_id,
            actor_id,
            runtime: if runtime == ActorRuntime::Kilo {
                "kilo"
            } else {
                "opencode"
            },
        });
        let mut fallback = resume_fallback::ResumeFallback::new(resume_session_id);
        let mut resume_failure: Option<(String, String)> = None;
        let mut last_error: Option<io::Error> = None;
        let launched = loop {
            let resume_id = match fallback.next() {
                resume_fallback::Next::Launch { resume_id } => resume_id,
                resume_fallback::Next::Stop => {
                    return Err(last_error.unwrap_or_else(|| {
                        io::Error::other("OpenCode/Kilo launch refused without an error")
                    }));
                }
            };
            let attempt = opencode::prepare(&command, &env, runtime)?;
            match opencode::launch(
                attempt,
                &binding.root,
                env.clone(),
                &generation,
                purpose,
                resume_id.as_deref(),
                mcp_server.clone(),
            )
            .await
            {
                Ok(launched) => break launched,
                Err(error) => {
                    let failure = fallback.failed(resume_id.as_deref());
                    if let (Some(context), Some(failed_id)) =
                        (fallback_context, failure.invalidate_id())
                    {
                        let error_text = error.to_string();
                        resume_failure = Some((failed_id.to_owned(), error_text.clone()));
                        resume_fallback::invalidate_receipt(
                            |failed_id, reason| {
                                super::super::runtime_session::fail_opencode_managed_session(
                                    runtime,
                                    home,
                                    context.group_id,
                                    context.actor_id,
                                    failed_id,
                                    reason,
                                )
                            },
                            failed_id,
                            &error_text,
                        );
                    }
                    if failure.may_retry_fresh() {
                        tracing::warn!(
                            %error, ?runtime,
                            "OpenCode/Kilo managed resume failed; retrying with a fresh session"
                        );
                        last_error = Some(error);
                        continue;
                    }
                    if let (Some(context), Some((failed_id, error_text))) =
                        (fallback_context, resume_failure)
                    {
                        context.record_resume_failure(
                            &failed_id,
                            &error_text,
                            Some(&error.to_string()),
                        );
                    }
                    return Err(error);
                }
            }
        };
        if let Some((group_id, actor_id)) = actor
            && let Err(error) = super::super::runtime_session::record_opencode_managed_session(
                runtime,
                home,
                group_id,
                actor_id,
                &binding.root,
                &session_command,
                &launched.environment,
                &launched.session_id,
                launched.resumed,
            )
        {
            tracing::warn!(%error, %group_id, %actor_id, ?runtime, "failed to persist managed ACP session");
        }
        Ok(Self {
            #[cfg(test)]
            binding,
            generation,
            runtime,
            endpoint: String::new(),
            thread_id: launched.session_id,
            remote_tui_prefix: Vec::new(),
            environment: launched.environment,
            protocol: ManagedProtocol::Acp(launched.protocol),
            process: Some(launched.process),
            auxiliary_processes: Vec::new(),
            native_tui_command: Some(launched.tui_command),
            cleanup_paths: Vec::new(),
            thread_resumed: launched.resumed,
            delegations: tokio::sync::Mutex::new(HashMap::new()),
        })
    }
}
