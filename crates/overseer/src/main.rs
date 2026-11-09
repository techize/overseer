mod agent;
mod attach;
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
    /// Mirror one slot in this terminal. Ctrl-] detaches.
    Attach { n: overseer_proto::SlotId },
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
        if let Err(e) = gui() {
            eprintln!("overseer: {e:#}");
            std::process::exit(1);
        }
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
        Cmd::Attach { n } => runtime().and_then(|rt| rt.block_on(attach::run(n))),
        Cmd::Ctl { cmd } => runtime().and_then(|rt| rt.block_on(ctl::run(cmd))),
    };
    if let Err(e) = result {
        eprintln!("overseer: {e:#}");
        std::process::exit(1);
    }
}

/// Connects (starting the daemon if needed) and opens the window.
fn gui() -> anyhow::Result<()> {
    let rt = runtime()?;
    let client = rt.block_on(ctl::connect())?;
    overseer_gui::run_gui(std::sync::Arc::new(client), rt.handle().clone());
    Ok(())
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}
