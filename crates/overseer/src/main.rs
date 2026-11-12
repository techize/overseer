mod agent;
mod attach;
mod ctl;
mod hook;
mod outer;

use clap::{Parser, Subcommand};
use overseer_daemon::DaemonConfig;
use overseer_proto::paths::{log_dir, socket_path};

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
    /// Relay a Claude Code hook event to the daemon (run by Claude, not by hand).
    Hook { event: String },
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
    // Exit 2 blocks a Claude hook, and clap prints usage errors, so on the
    // hook path a parse failure is swallowed: silent, exit 0.
    let cli = if std::env::args_os().nth(1).is_some_and(|a| a == "hook") {
        match Cli::try_parse() {
            Ok(cli) => cli,
            Err(_) => return,
        }
    } else {
        Cli::parse()
    };
    let Some(command) = cli.command else {
        if let Err(e) = gui() {
            eprintln!("overseer: {e:#}");
            std::process::exit(1);
        }
        return;
    };
    let result = match command {
        // No logger and no stderr: a hook must be silent and always exit 0.
        Cmd::Hook { event } => {
            hook::run(&event);
            return;
        }
        Cmd::InstallAgent => agent::install(),
        Cmd::UninstallAgent => agent::uninstall(),
        Cmd::Daemon { foreground: _ } => {
            init_logging("daemon.log");
            let socket = socket_path();
            tracing::info!(
                "daemon {} starting (pid {}) on {}",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                socket.display()
            );
            let home = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("/"));
            let (settings, config_err) =
                overseer_daemon::Config::load(&overseer_proto::paths::config_path(), &home);
            if let Some(e) = config_err {
                tracing::warn!("config not loaded, using defaults: {e}");
            }
            let (patterns, targets_err) = overseer_daemon::locks::targets::load_patterns(
                &overseer_proto::paths::targets_path(),
            );
            if let Some(e) = targets_err {
                tracing::warn!("targets not loaded, using the defaults: {e}");
            }
            let result = runtime().and_then(|rt| {
                rt.block_on(overseer_daemon::run(DaemonConfig {
                    socket,
                    default_command: default_command(),
                    env: overseer_daemon::EnvSource::LoginShell,
                    state_dir: overseer_proto::paths::state_dir(),
                    hook_program: std::env::current_exe()?,
                    settings,
                    patterns,
                }))
            });
            match &result {
                Ok(()) => tracing::info!("daemon stopped"),
                // M4: the daemon that was asked for is running, so this is a
                // success; a non-zero exit would make launchd respawn it.
                Err(e)
                    if e.downcast_ref::<overseer_daemon::AlreadyRunning>()
                        .is_some() =>
                {
                    tracing::info!("another daemon is already running; exiting");
                    eprintln!("overseer: another daemon is already running");
                    return;
                }
                Err(e) => tracing::error!("daemon failed: {e:#}"),
            }
            result
        }
        Cmd::Attach { n } => runtime().and_then(|rt| rt.block_on(attach::run(n))),
        Cmd::Ctl { cmd } => runtime().and_then(|rt| rt.block_on(ctl::run(cmd))),
    };
    if let Err(e) = result {
        eprintln!("overseer: {e:#}");
        std::process::exit(1);
    }
}

/// Sends tracing output to `name` in the log dir, filtered by `RUST_LOG`
/// (default `info`). Falls back to stderr if the file cannot be opened.
fn init_logging(name: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let dir = log_dir();
    let file = std::fs::create_dir_all(&dir).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(name))
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    let _ = match file {
        Ok(f) => builder.with_writer(std::sync::Mutex::new(f)).try_init(),
        Err(e) => {
            eprintln!("overseer: cannot open log in {}: {e}", dir.display());
            builder.with_writer(std::io::stderr).try_init()
        }
    };
}

/// Connects (starting the daemon if needed) and opens the window. If the
/// daemon cannot be reached, shows the error in a window (a Finder launch
/// has no terminal for stderr) and exits 1 once it is dismissed.
fn gui() -> anyhow::Result<()> {
    init_logging("gui.log");
    let rt = runtime()?;
    let client = match rt.block_on(ctl::connect()) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("cannot reach the daemon: {e:#}");
            eprintln!("overseer: {e:#}");
            overseer_gui::run_startup_error(
                format!("{e:#}"),
                overseer_gui::startup_error::advice(&log_dir()),
            );
        }
    };
    overseer_gui::run_gui(std::sync::Arc::new(client), rt.handle().clone());
    Ok(())
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}
