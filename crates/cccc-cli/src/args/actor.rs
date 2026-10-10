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
    Remove(ActorTarget),
    Start(ActorTarget),
    Stop(ActorTarget),
    Restart(ActorTarget),
    /// Resume a specific saved Claude conversation in this actor's workspace.
    /// Discard the actor's attempted session and start a fresh one.
    ///
    /// The recovery for an ACP session that cannot be resumed: attempted
    /// sessions are never replaced automatically, so a session whose data has
    /// gone missing leaves the actor down until this is run.
    NewSession(ActorTarget),
    ResumeSession {
        #[command(flatten)]
        target: ActorTarget,
        session_id: String,
    },
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
