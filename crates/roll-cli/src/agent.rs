//! `roll agent …`: connect this machine to roll code agents (docs/code/agents.md).
//!
//! `login` runs the device flow (or takes a machine token minted in the dashboard,
//! `--machine-token`) and keeps the machine token in `~/.config/roll/agent.json`
//! (0600; `ROLL_AGENT_CONFIG` names another file). `up` runs the daemon (`agent_host::Host`) with
//! the providers from its config and keeps the link to the relay up until Ctrl-C; without a login
//! file it takes the machine token from `ROLL_AGENT_TOKEN` (a roll-hosted sandbox, whose machine
//! roll made), and either way it runs without that variable (see `machine::env_login`). `status` shows
//! the login and whether roll still accepts it; `logout` revokes the machine and forgets it.
//!
//! `ROLL_AGENT_FAKE=1` is for tests only (`pnpm test:agents`): `up` offers the fake providers of
//! `agent_host::provider::fake` (`fake`, which plays a scripted turn with an approval and an edit;
//! `echo`; and `orchestrate`, an orchestrator that delegates two parts through roll's real
//! orchestrator MCP server and sums up their results) instead of the real ones, so nothing calls a
//! model.

use clap::Subcommand;

#[derive(Subcommand)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum Agent {
    /// Connect this machine to roll: shows a code to confirm in the dashboard.
    Login {
        /// What to call this machine (default: its hostname).
        #[arg(long, conflicts_with = "machine_token")]
        name: Option<String>,
        /// A machine token made in the dashboard ("Add machine"): saved as is, no code to confirm.
        #[arg(long, env = "ROLL_AGENT_TOKEN", hide_env_values = true, value_name = "rlm_…")]
        machine_token: Option<String>,
    },
    /// Run the agent daemon here (providers, threads, terminals) and keep it connected to roll.
    /// Logs go to stderr; Ctrl-C stops it.
    Up,
    /// Show this machine's login and whether roll accepts it.
    Status,
    /// Remove this machine from roll and forget its token.
    Logout,
    /// The owner's automations: triggers, filters and steps that run agents (docs/code/automations.md).
    Automation(crate::automation::Automation),
}

#[cfg(unix)]
mod machine;

#[cfg(unix)]
pub use machine::run;

/// On Windows only the owner's automations: the daemon (agent-host's PTYs and process groups)
/// runs on Linux and macOS, so there's nothing to log a Windows machine in for.
#[cfg(not(unix))]
pub async fn run(api: &str, json_out: bool, cmd: Agent) -> anyhow::Result<()> {
    match cmd {
        Agent::Automation(a) => crate::automation::run(api, json_out, a).await,
        _ => anyhow::bail!("roll agent's daemon runs on Linux and macOS (or in WSL on Windows): run `roll agent login` and `roll agent up` there"),
    }
}
