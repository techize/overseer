use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use assert_cmd::Command;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fake-claude.sh");
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Isolated OVERSEER_HOME; shuts the daemon down on drop.
struct Env {
    home: PathBuf,
}

impl Env {
    fn new() -> Env {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!("ovc-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // Keep the daemon off the real beads and Jira.
        std::fs::write(home.join("config.toml"), "beads_dir = \"/nonexistent/overseer-test-beads\"\nbrain_dir = \"/nonexistent/overseer-test-brain\"\njira_command = []\n").unwrap();
        Env { home }
    }

    fn ctl(&self, args: &[&str]) -> Command {
        let mut c = Command::cargo_bin("overseer").unwrap();
        c.env("OVERSEER_HOME", &self.home)
            .env("OVERSEER_CONFIG", self.home.join("config.toml"))
            .env("OVERSEER_DEFAULT_COMMAND", SCRIPT)
            .arg("ctl")
            .args(args)
            .timeout(Duration::from_secs(10));
        c
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = StdCommand::new(assert_cmd::cargo::cargo_bin("overseer"))
            .env("OVERSEER_HOME", &self.home)
            .env("OVERSEER_CONFIG", self.home.join("config.toml"))
            .args(["ctl", "shutdown"])
            .output();
        std::thread::sleep(Duration::from_millis(200));
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn stdout_of(c: &mut Command) -> String {
    let out = c.assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

#[test]
fn ctl_status_autostarts_daemon() {
    let env = Env::new();
    let out = stdout_of(&mut env.ctl(&["status"]));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 7, "{out}");
    for (i, l) in lines.iter().enumerate() {
        assert_eq!(*l, format!("S{i}  empty"));
    }
}

#[test]
fn ctl_start_send_status() {
    let env = Env::new();
    env.ctl(&["start", "2", "--cwd", "/tmp", "--label", "demo"])
        .assert()
        .success();
    env.ctl(&["send", "2", "hello"]).assert().success();
    let out = stdout_of(&mut env.ctl(&["status"]));
    let s2 = out.lines().find(|l| l.starts_with("S2")).unwrap();
    assert!(s2.starts_with("S2  running"), "{out}");
    assert!(s2.ends_with("demo"), "{out}");
}

#[test]
fn ctl_unreachable_message() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    std::fs::set_permissions(&env.home, std::fs::Permissions::from_mode(0o500)).unwrap();
    let t = Instant::now();
    let out = env.ctl(&["status"]).assert().failure().get_output().clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not reach the Overseer daemon"),
        "{stderr}"
    );
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    std::fs::set_permissions(&env.home, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn attach_empty_slot_message() {
    let env = Env::new();
    let mut c = Command::cargo_bin("overseer").unwrap();
    let out = c
        .env("OVERSEER_HOME", &env.home)
        .env("OVERSEER_CONFIG", env.home.join("config.toml"))
        .env("OVERSEER_DEFAULT_COMMAND", SCRIPT)
        .args(["attach", "3"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("S3 is empty. Start it with: overseer ctl start 3 --cwd <dir>"),
        "{stderr}"
    );
}

#[test]
fn daemon_logs_and_exits_zero_on_shutdown() {
    let env = Env::new();
    let mut daemon = StdCommand::new(assert_cmd::cargo::cargo_bin("overseer"))
        .env("OVERSEER_HOME", &env.home)
        .env("OVERSEER_CONFIG", env.home.join("config.toml"))
        .env("OVERSEER_DEFAULT_COMMAND", SCRIPT)
        .args(["daemon", "--foreground"])
        .spawn()
        .unwrap();
    // Wait for the socket so ctl does not autostart a second daemon.
    let up = Instant::now() + Duration::from_secs(5);
    while !env.home.join("overseer.sock").exists() {
        assert!(Instant::now() < up, "daemon did not bind");
        std::thread::sleep(Duration::from_millis(20));
    }
    env.ctl(&["start", "1", "--cwd", "/tmp"]).assert().success();
    env.ctl(&["shutdown"]).assert().success();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = daemon.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "daemon did not exit");
        std::thread::sleep(Duration::from_millis(50));
    };
    // launchd keeps a cleanly exited daemon down, so this must be 0.
    assert_eq!(status.code(), Some(0));
    let log = std::fs::read_to_string(env.home.join("logs/daemon.log")).unwrap();
    assert!(log.contains("starting"), "{log}");
    assert!(log.contains("S1 started"), "{log}");
    assert!(log.contains("daemon stopped"), "{log}");
}

/// Spawns `overseer daemon --foreground` for `env` with extra variables and
/// waits for its socket.
fn spawn_daemon(env: &Env, vars: &[(&str, &str)]) -> std::process::Child {
    let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin("overseer"));
    cmd.env("OVERSEER_HOME", &env.home)
        .env("OVERSEER_CONFIG", env.home.join("config.toml"))
        .env("OVERSEER_DEFAULT_COMMAND", SCRIPT)
        .args(["daemon", "--foreground"]);
    for (k, v) in vars {
        cmd.env(k, v);
    }
    let child = cmd.spawn().unwrap();
    let up = Instant::now() + Duration::from_secs(5);
    while !env.home.join("overseer.sock").exists() {
        assert!(Instant::now() < up, "daemon did not bind");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

fn wait_exit(child: &mut std::process::Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        assert!(Instant::now() < deadline, "process did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn second_daemon_exits_zero_and_leaves_the_first_running() {
    let env = Env::new();
    let mut first = spawn_daemon(&env, &[]);
    let mut second = StdCommand::new(assert_cmd::cargo::cargo_bin("overseer"))
        .env("OVERSEER_HOME", &env.home)
        .env("OVERSEER_CONFIG", env.home.join("config.toml"))
        .args(["daemon", "--foreground"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // launchd's KeepAlive {SuccessfulExit=false} respawns non-zero exits.
    let status = wait_exit(&mut second, Duration::from_secs(5));
    assert_eq!(status.code(), Some(0));
    let mut err = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut err).unwrap();
    assert!(err.contains("already running"), "{err}");
    assert_eq!(stdout_of(&mut env.ctl(&["status"])).lines().count(), 7);
    assert!(first.try_wait().unwrap().is_none(), "first daemon died");
    env.ctl(&["shutdown"]).assert().success();
    wait_exit(&mut first, Duration::from_secs(5));
}

#[test]
fn ctl_start_rejects_a_missing_cwd() {
    let env = Env::new();
    let missing = env.home.join("no-such-dir");
    let out = env
        .ctl(&["start", "1", "--cwd", missing.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist or is not a directory"),
        "{stderr}"
    );
    let status = stdout_of(&mut env.ctl(&["status"]));
    assert!(status.contains("S1  empty"), "{status}");
}

#[test]
fn sessions_get_the_login_shell_env_without_parent_session_vars() {
    let env = Env::new();
    // A home of our own, so the login shell reads this .profile and not
    // the real one.
    let home = env.home.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".profile"),
        "export FROM_PROFILE=yes\nexport CLAUDE_CODE_OAUTH_TOKEN=from-rc\n",
    )
    .unwrap();
    let mut daemon = spawn_daemon(
        &env,
        &[
            ("HOME", home.to_str().unwrap()),
            ("SHELL", "/bin/sh"),
            // As if started from inside a Claude Code session.
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "cli"),
            ("CLAUDE_CODE_SESSION_ID", "parent-session"),
            ("CLAUDE_CODE_SSE_PORT", "4242"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "parent-token"),
            // User configuration that must survive.
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("ANTHROPIC_MODEL", "some-model"),
        ],
    );
    let out = env.home.join("slot-env.txt");
    let script = format!("env > {}.tmp && mv {0}.tmp {0}; sleep 5", out.display());
    env.ctl(&[
        "start", "1", "--cwd", "/tmp", "--", "/bin/sh", "-c", &script,
    ])
    .assert()
    .success();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !out.exists() {
        assert!(Instant::now() < deadline, "slot did not write its env");
        std::thread::sleep(Duration::from_millis(50));
    }
    let text = std::fs::read_to_string(&out).unwrap();
    let vars: std::collections::HashMap<&str, &str> =
        text.lines().filter_map(|l| l.split_once('=')).collect();
    assert_eq!(
        vars.get("FROM_PROFILE"),
        Some(&"yes"),
        "login shell env not used"
    );
    assert_eq!(vars.get("OVERSEER_SLOT"), Some(&"S1"));
    assert_eq!(vars.get("CLAUDE_CODE_USE_BEDROCK"), Some(&"1"));
    assert_eq!(vars.get("ANTHROPIC_MODEL"), Some(&"some-model"));
    // The parent's token is gone; the one the user's rc sets is kept.
    assert_eq!(vars.get("CLAUDE_CODE_OAUTH_TOKEN"), Some(&"from-rc"));
    for gone in [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_SSE_PORT",
    ] {
        assert!(!vars.contains_key(gone), "{gone} leaked into the slot");
    }
    assert!(
        vars.contains_key("LANG") || vars.contains_key("LC_ALL"),
        "no locale"
    );
    env.ctl(&["shutdown"]).assert().success();
    wait_exit(&mut daemon, Duration::from_secs(5));
    let log = std::fs::read_to_string(env.home.join("logs/daemon.log")).unwrap();
    assert!(log.contains("login shell:"), "{log}");
    // Counts only, never values.
    assert!(
        !log.contains("from-rc") && !log.contains("parent-token"),
        "{log}"
    );
}
