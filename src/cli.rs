//! Command line (clap).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "tmac",
    version,
    about = "Run hook-capable AI CLIs in tmux panes and type into them only when they can take input"
)]
pub struct Cli {
    /// Project of the agents (default: $TMAC_PROJECT, else the repository of
    /// the directory, like docker compose)
    #[arg(short = 'p', long, global = true, value_name = "PROJECT")]
    pub project: Option<String>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Start an AI CLI in a new tmux window and wait until it is idle
    Spawn(SpawnCli),
    /// Called by the CLI's hooks (not for direct use)
    Hook(HookCli),
    /// Type text into an agent when it can take input
    Send(SendCli),
    /// Make sure the agent passes a Stop after now, typing TEXT if it is idle
    Wake(WakeCli),
    /// Wait until the agent is in one of the states
    Wait(WaitCli),
    /// Stop tmac from typing into the agent
    Hold(HoldCli),
    /// Remove a hold
    Release(ReleaseCli),
    /// Show an agent as YAML
    Status(StatusCli),
    /// List agents
    List(ListCli),
    /// End an agent and remove its pane and record
    Kill(KillCli),
}

#[derive(Args, Debug)]
pub struct SpawnCli {
    /// Agent name (a-z 0-9 . _ -, 1-64 characters)
    pub name: String,
    /// Driver: claude or codex (default: from the command name)
    #[arg(long)]
    pub driver: Option<String>,
    /// Working directory (default: the current directory)
    #[arg(short = 'C', long)]
    pub cwd: Option<PathBuf>,
    /// Add an environment variable to the agent (KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Pass a variable of this environment to the agent
    #[arg(long = "pass-env", value_name = "KEY")]
    pub pass_env: Vec<String>,
    /// Relay a hook event to a command (EVENT=COMMAND; empty COMMAND disables)
    #[arg(long, value_name = "EVENT=COMMAND")]
    pub relay: Vec<String>,
    /// tmux session for the new window
    #[arg(long)]
    pub session: Option<String>,
    /// Seconds to hold input after a person typed in the pane (0: never)
    #[arg(long = "human-hold", value_name = "SEC")]
    pub human_hold: Option<f64>,
    /// Start even if other hooks handle Stop or UserPromptSubmit
    #[arg(long = "allow-foreign-hooks")]
    pub allow_foreign_hooks: bool,
    /// Seconds to wait until the agent is idle
    #[arg(long, value_name = "SEC")]
    pub timeout: Option<f64>,
    /// The CLI and its arguments
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct HookCli {
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub instance: Option<String>,
    pub driver: String,
    pub event: String,
}

#[derive(Args, Debug)]
pub struct SendCli {
    pub name: String,
    /// Text to type (default: standard input)
    pub text: Option<String>,
    /// Read the text from a file
    #[arg(short = 'f', long)]
    pub file: Option<PathBuf>,
    /// Seconds to wait until the agent can take input
    #[arg(long, value_name = "SEC")]
    pub timeout: Option<f64>,
    /// Fail with exit code 5 instead of waiting
    #[arg(long = "no-wait")]
    pub no_wait: bool,
}

#[derive(Args, Debug)]
pub struct WakeCli {
    pub name: String,
    /// Text to type if the agent is idle (default: standard input)
    pub text: Option<String>,
    #[arg(long, value_name = "SEC")]
    pub timeout: Option<f64>,
}

#[derive(Args, Debug)]
pub struct WaitCli {
    pub name: String,
    /// States to wait for, comma separated (default: idle)
    #[arg(long, value_name = "STATE[,STATE...]")]
    pub state: Vec<String>,
    #[arg(long, value_name = "SEC")]
    pub timeout: Option<f64>,
}

#[derive(Args, Debug)]
pub struct HoldCli {
    pub name: String,
    /// Reason of the hold (default: manual)
    #[arg(long)]
    pub key: Option<String>,
    #[arg(long)]
    pub note: Option<String>,
    /// Seconds to wait for input in progress to finish
    #[arg(long, value_name = "SEC")]
    pub timeout: Option<f64>,
}

#[derive(Args, Debug)]
pub struct ReleaseCli {
    pub name: String,
    /// Hold to remove (default: manual)
    #[arg(long, conflicts_with = "all")]
    pub key: Option<String>,
    /// Remove every hold
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct StatusCli {
    pub name: String,
    /// Print one field only (e.g. state)
    #[arg(long)]
    pub field: Option<String>,
}

#[derive(Args, Debug)]
pub struct ListCli {
    /// Print names only
    #[arg(short = 'q')]
    pub quiet: bool,
    /// List the agents of every project
    #[arg(short = 'a', long)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct KillCli {
    pub name: String,
    /// Seconds to wait for the CLI to end
    #[arg(long, value_name = "SEC")]
    pub timeout: Option<f64>,
    /// Do not end the CLI gracefully, and ignore holds
    #[arg(long)]
    pub force: bool,
}
