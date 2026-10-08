use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct ActorArgs {
    #[command(subcommand)]
    pub action: ActorAction,
}

#[derive(Debug, Subcommand)]
pub enum ActorAction {
    List {
        #[arg(long = "group")]
        group_id: Option<String>,
    },
    Add {
        actor_id: String,
        #[arg(long, default_value = "")]
        title: String,
        #[arg(long, default_value = "codex")]
        runtime: String,
        #[arg(long, default_value = "default", value_parser = ["default", "acp"])]
        runtime_mode: String,
        #[arg(long, default_value = "")]
        command: String,
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(long, default_value = "")]
        scope: String,
        #[arg(long, default_value = "enter")]
        submit: String,
        #[arg(long = "group")]
        group_id: Option<String>,
        #[arg(long, default_value = "user")]
        by: String,
    },
    /// Inspect an uncertain Claude launch; acknowledge to clear only an unidentified fence.
    ReconcileClaude {
        actor_id: String,
        #[arg(long = "group")]
        group_id: Option<String>,
        /// I inspected the original Claude configuration and accept leaving its jobs running.
        #[arg(long)]
        acknowledge: bool,
    },
    Remove(ActorTarget),
    Start(ActorTarget),
    Stop(ActorTarget),
    Restart(ActorTarget),
    Update {
        actor_id: String,
        #[arg(long = "group")]
        group_id: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        runtime: Option<String>,
        #[arg(long, value_parser = ["default", "acp"])]
        runtime_mode: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        command: Option<String>,
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(long)]
        submit: Option<String>,
        #[arg(long)]
        enabled: Option<bool>,
        #[arg(long, default_value = "user")]
        by: String,
    },
    Secrets {
        actor_id: String,
        #[arg(long = "group")]
        group_id: Option<String>,
        #[arg(long = "set")]
        set: Vec<String>,
        #[arg(long = "unset")]
        unset: Vec<String>,
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        keys: bool,
        #[arg(long)]
        restart: bool,
        #[arg(long, default_value = "user")]
        by: String,
    },
}

#[derive(Debug, Args)]
pub struct ActorTarget {
    pub actor_id: String,
    #[arg(long = "group")]
    pub group_id: Option<String>,
    #[arg(long, default_value = "user")]
    pub by: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn reconcile_claude_requires_explicit_acknowledgment_flag() {
        for acknowledged in [false, true] {
            let mut args = vec![
                "cccc",
                "actor",
                "reconcile-claude",
                "worker",
                "--group",
                "g_one",
            ];
            if acknowledged {
                args.push("--acknowledge");
            }
            let cli = crate::args::Cli::try_parse_from(args).expect("reconciliation command");
            let Some(crate::args::CommandKind::Actor(ActorArgs {
                action:
                    ActorAction::ReconcileClaude {
                        actor_id,
                        group_id,
                        acknowledge,
                    },
            })) = cli.command
            else {
                panic!("wrong command");
            };
            assert_eq!(actor_id, "worker");
            assert_eq!(group_id.as_deref(), Some("g_one"));
            assert_eq!(acknowledge, acknowledged);
        }
    }
}
