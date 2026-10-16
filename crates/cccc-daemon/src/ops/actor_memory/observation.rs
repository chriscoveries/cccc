//! Attribution for measurement, never authority to signal an observed PID.
use super::{claude_observation, sampling};
use cccc_contracts::{Actor, ActorRuntime};
use cccc_core::{GroupDoc, HomeLayout};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Identity {
    Managed {
        generation: String,
        runtime: ActorRuntime,
        session_id: String,
        roots: Vec<u32>,
        claude: Option<claude_observation::Binding>,
    },
    Terminal {
        pid: u32,
        started_at: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Observation {
    pub identity: Identity,
    pub total_kib: u64,
    pub counted: usize,
    pub basis: &'static str,
}

fn identity(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
) -> Result<Option<(Identity, &'static str)>, &'static str> {
    let managed =
        super::super::local_headless::memory::observe_managed(home, &group.group_id, &actor.id)
            .map_err(|_| "Managed runtime registry cannot be read for memory detection")?;
    if let Some(managed) = managed {
        if !managed.running && managed.roots.is_empty() && managed.runtime != ActorRuntime::Claude {
            return Ok(None);
        }
        let mut roots = managed.roots;
        let (claude, basis) = if managed.runtime == ActorRuntime::Claude {
            let cwd = super::super::actor_runtime::working_directory(group, actor).map_err(
                |_| "Claude current workspace cannot be matched to native metadata (T649)",
            )?;
            let binding = claude_observation::observe(&managed.environment, &managed.command, &managed.session_id, &cwd)
                .map_err(|_| "Claude 2.1.293 roster/session/process linkage is unavailable or stale; provider memory is unmeasured (T649)")?;
            roots = vec![binding.host_pid, binding.repl_pid];
            (Some(binding), "claude_2_1_293_observed_host_and_repl")
        } else {
            (None, "managed_owned_process_roots")
        };
        roots.sort_unstable();
        roots.dedup();
        if roots.is_empty() {
            return Err(
                "Runtime exposes no attributable per-actor process root; memory detection is unavailable",
            );
        }
        return Ok(Some((
            Identity::Managed {
                generation: managed.generation,
                runtime: managed.runtime,
                session_id: managed.session_id,
                roots,
                claude,
            },
            basis,
        )));
    }
    if let Ok(status) = cccc_runtime::status(&group.group_id, &actor.id)
        && status.running
    {
        if let Some(pid) = status.pid {
            return Ok(Some((
                Identity::Terminal {
                    pid,
                    started_at: status.started_at,
                },
                "owned_terminal_process_tree",
            )));
        }
        return Err("Running terminal registry has no measurable process root");
    }
    if super::super::actor_runtime_status::resolve(group, actor).running {
        Err(
            "Running runtime has no attributable per-actor process root; external provider memory is unmeasured",
        )
    } else {
        Ok(None)
    }
}

pub(crate) fn observe(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
) -> Result<Option<Observation>, &'static str> {
    let Some((before, basis)) = identity(home, group, actor)? else {
        return Ok(None);
    };
    let roots = match &before {
        Identity::Managed { roots, .. } => roots.clone(),
        Identity::Terminal { pid, .. } => vec![*pid],
    };
    // Coordinated parent prerequisite: one platform-bounded observed union.
    // Darwin performs a single shared-budget walk; Linux deduplicates roots.
    let (total_kib, counted) = sampling::roots_rss_kib(&roots);
    if counted == 0 {
        return Err("Process RSS+swap/footprint sample is unreadable; memory usage is unknown");
    }
    if identity(home, group, actor)?.as_ref().map(|(id, _)| id) != Some(&before) {
        return Err(
            "Runtime generation or observed process creation metadata changed during memory sampling",
        );
    }
    Ok(Some(Observation {
        identity: before,
        total_kib,
        counted,
        basis,
    }))
}
