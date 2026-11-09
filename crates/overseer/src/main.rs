mod agent;
mod ctl;

use clap::{Parser, Subcommand};
use overseer_daemon::DaemonConfig;
use overseer_proto::paths::socket_path;

#[derive(Parser)]
#[command(name = "overseer", about = "Host Claude Code sessions in one window")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon that owns the sessions.
    Daemon {
        /// Stay attached to this process (what the LaunchAgent uses). The
        /// daemon never forks itself, so this is accepted for clarity.
        #[arg(long)]
        foreground: bool,
    },
    /// Talk to the daemon.
    Ctl {
        #[command(subcommand)]
        cmd: ctl::CtlCmd,
    },
    /// Install the LaunchAgent that keeps the daemon running.
    InstallAgent,
    /// Remove the LaunchAgent.
    UninstallAgent,
}

fn default_command() -> Vec<String> {
    match std::env::var("OVERSEER_DEFAULT_COMMAND") {
        Ok(v) if !v.trim().is_empty() => v.split_whitespace().map(String::from).collect(),
        _ => vec!["claude".to_string()],
    }
}

fn main() {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        overseer_gui::run_gui_spike();
        return;
    };
    let result = match command {
        Cmd::InstallAgent => agent::install(),
        Cmd::UninstallAgent => agent::uninstall(),
        Cmd::Daemon { foreground: _ } => runtime().and_then(|rt| {
            rt.block_on(overseer_daemon::run(DaemonConfig {
                socket: socket_path(),
                default_command: default_command(),
            }))
        }),
        Cmd::Ctl { cmd } => runtime().and_then(|rt| rt.block_on(ctl::run(cmd))),
    };
    if let Err(e) = result {
        eprintln!("overseer: {e:#}");
        std::process::exit(1);
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}
