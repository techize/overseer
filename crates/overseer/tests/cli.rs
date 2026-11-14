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
    /// Stub `bd` folder. The daemon finds it through a login shell whose
    /// home is `home/home`, so the real `bd` is never run.
    bin: Option<PathBuf>,
}

impl Env {
    fn new() -> Env {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!("ovc-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // Keep the daemon off the real beads and Jira.
        std::fs::write(home.join("config.toml"), "beads_dir = \"/nonexistent/overseer-test-beads\"\nbrain_dir = \"/nonexistent/overseer-test-brain\"\njira_command = []\n").unwrap();
        let env = Env { home, bin: None };
        // Every spawn names the stub, so an autostart never reaches launchd.
        env.write_launchctl_stub();
        env
    }

    /// Daemon sources pointed at stub `bd` and Jira scripts.
    fn with_stub_sources() -> Env {
        use std::os::unix::fs::PermissionsExt;
        let mut env = Env::new();
        let bin = env.home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, body: &str| {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        script(
            "bd",
            r#"if [ "$1" = list ]; then cat <<'EOF'
[{"id":"ov-3","title":"DEMO-12 wire the widget","status":"open","priority":1}]
EOF
fi
"#,
        );
        script(
            "jira-stub",
            r#"cat <<'EOF'
{"issues":[{"key":"DEMO-12","fields":{"summary":"Widget wiring","status":{"name":"In Progress"}}}]}
EOF
"#,
        );
        let cfg = format!(
            "beads_dir = \"{h}\"\nbrain_dir = \"{h}\"\njira_command = [\"{b}/jira-stub\"]\nticket_pattern = \"DEMO-[0-9]+\"\n",
            h = env.home.display(),
            b = bin.display()
        );
        std::fs::write(env.home.join("config.toml"), cfg).unwrap();
        let shell_home = env.home.join("home");
        std::fs::create_dir_all(&shell_home).unwrap();
        std::fs::write(
            shell_home.join(".profile"),
            format!("export PATH=\"{}:$PATH\"\n", bin.display()),
        )
        .unwrap();
        env.bin = Some(bin);
        env
    }

    /// The 2ndBrain writer on: a throwaway vault with a bare remote, a stub
    /// `bd` that exits 3 and logs what it was given, and no backup or sync.
    fn with_brain() -> Env {
        use std::os::unix::fs::PermissionsExt;
        let env = Env::new();
        let h = &env.home;
        let git = |dir: &std::path::Path, args: &[&str]| {
            let o = StdCommand::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        let (vault, remote, beads) = (h.join("vault"), h.join("remote.git"), h.join("beads"));
        for d in [&vault, &remote, &beads] {
            std::fs::create_dir_all(d).unwrap();
        }
        git(&remote, &["init", "--bare", "-b", "main"]);
        git(&vault, &["init", "-b", "main"]);
        git(&vault, &["config", "user.name", "T"]);
        git(&vault, &["config", "user.email", "t@example.com"]);
        git(
            &vault,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        std::fs::write(vault.join("README.md"), "vault\n").unwrap();
        git(&vault, &["add", "-A"]);
        git(&vault, &["commit", "-m", "initial"]);
        git(&vault, &["push", "-u", "origin", "main"]);
        let stub = h.join("bd-stub.sh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{log}'\ncat >> '{log}.stdin'\necho bd out\necho bd err >&2\nexit 3\n",
                log = h.join("bd.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = format!(
            "beads_dir = \"{beads}\"\nbrain_dir = \"{vault}\"\njira_command = []\nbrain_writer = true\nbackup_mins = 0\ncurrent_session = \"{cs}\"\nsync_command = [\"{vault}/no-such-sync\"]\nbd_command = [\"{stub}\"]\n",
            beads = beads.display(),
            vault = vault.display(),
            cs = h.join("CURRENT_SESSION.md").display(),
            stub = stub.display(),
        );
        std::fs::write(h.join("config.toml"), cfg).unwrap();
        env
    }

    /// The launchctl stub every test uses: it logs its arguments to
    /// `launchctl.log`, exits 0 for `bootstrap` and `bootout`, and 1 for
    /// `print` and `kickstart`, so autostart falls back to spawning.
    fn launchctl_stub(&self) -> PathBuf {
        self.home.join("launchctl-stub.sh")
    }

    fn write_launchctl_stub(&self) {
        self.write_launchctl_stub_failing("print|kickstart");
    }

    /// The stub, exiting 1 for the subcommands in `failing` (a `case`
    /// pattern) and 0 for the rest.
    fn write_launchctl_stub_failing(&self, failing: &str) {
        use std::os::unix::fs::PermissionsExt;
        let stub = self.launchctl_stub();
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{log}'\ncase \"$1\" in {failing}) exit 1;; esac\nexit 0\n",
                log = self.home.join("launchctl.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `overseer <args>` for the agent commands: `HOME` is `<home>/home`,
    /// `OVERSEER_HOME` this home and launchctl the stub, so nothing reaches
    /// the real LaunchAgents folder or launchd.
    fn agent(&self, args: &[&str]) -> Command {
        self.write_launchctl_stub();
        let shell_home = self.home.join("home");
        std::fs::create_dir_all(&shell_home).unwrap();
        let mut c = Command::cargo_bin("overseer").unwrap();
        c.env("HOME", shell_home)
            .env("SHELL", "/bin/sh")
            .env("OVERSEER_HOME", &self.home)
            .env("OVERSEER_CONFIG", self.home.join("config.toml"))
            .env("OVERSEER_LAUNCHCTL", self.launchctl_stub())
            .env_remove("OVERSEER_TARGETS")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("OVERSEER_DEFAULT_COMMAND")
            .args(args)
            .timeout(Duration::from_secs(20));
        c
    }

    /// A `claude` that only sleeps, so its sessions get a session id and
    /// come back resumable after a restart.
    fn claude_stub(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("claude");
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 3600\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    /// `agent(args)` with the sleeping `claude` as the default command.
    fn agent_claude(&self, args: &[&str]) -> Command {
        let mut c = self.agent(args);
        c.env("OVERSEER_DEFAULT_COMMAND", self.claude_stub());
        c
    }

    /// `ctl(args)` with the sleeping `claude` as the default command.
    fn ctl_claude(&self, args: &[&str]) -> Command {
        let mut c = self.ctl(args);
        c.env("OVERSEER_DEFAULT_COMMAND", self.claude_stub());
        c
    }

    fn launchctl_log(&self) -> String {
        std::fs::read_to_string(self.home.join("launchctl.log")).unwrap_or_default()
    }

    fn ctl(&self, args: &[&str]) -> Command {
        let mut c = Command::cargo_bin("overseer").unwrap();
        c.env("OVERSEER_HOME", &self.home)
            .env("OVERSEER_CONFIG", self.home.join("config.toml"))
            .env("OVERSEER_LAUNCHCTL", self.launchctl_stub())
            .env("OVERSEER_DEFAULT_COMMAND", SCRIPT)
            // Run from inside a session, these would be that slot's writes.
            .env_remove("OVERSEER_SLOT");
        if self.bin.is_some() {
            c.env("HOME", self.home.join("home"))
                .env("SHELL", "/bin/sh");
        }
        c.arg("ctl").args(args).timeout(Duration::from_secs(10));
        c
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = StdCommand::new(assert_cmd::cargo::cargo_bin("overseer"))
            .env("OVERSEER_HOME", &self.home)
            .env("OVERSEER_CONFIG", self.home.join("config.toml"))
            .env("OVERSEER_LAUNCHCTL", self.launchctl_stub())
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
fn ctl_version_without_a_daemon_does_not_start_one() {
    let env = Env::new();
    let out = stdout_of(&mut env.ctl(&["version"]));
    assert!(out.lines().any(|l| l == "daemon  not running"), "{out}");
    assert!(out.starts_with("app     0.1.0 build dev"), "{out}");
    assert!(!env.home.join("overseer.sock").exists());
}

#[test]
fn ctl_version_names_the_running_daemon() {
    let env = Env::new();
    env.ctl(&["status"]).assert().success();
    let out = stdout_of(&mut env.ctl(&["version"]));
    assert!(
        out.lines()
            .any(|l| l.starts_with("daemon  0.1.0 build dev  pid ")),
        "{out}"
    );
}

#[test]
fn ctl_version_survives_a_daemon_that_predates_daemon_info() {
    let env = Env::new();
    // An old daemon reads the unknown request and hangs up.
    let listener = std::os::unix::net::UnixListener::bind(env.home.join("overseer.sock")).unwrap();
    let old = std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            let mut stream = stream.unwrap();
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
        }
    });
    let out = stdout_of(&mut env.ctl(&["version"]));
    old.join().unwrap();
    assert!(
        out.contains("daemon  running, build unknown (older than this app)"),
        "{out}"
    );
    assert!(out.contains("Restart the daemon"), "{out}");
}

#[test]
fn ctl_settings_set_and_reset() {
    let env = Env::new();
    let config = env.home.join("config.toml");
    let out = stdout_of(&mut env.ctl(&["settings"]));
    assert!(
        out.starts_with(&format!("config.toml: {}\n", config.display())),
        "{out}"
    );
    assert!(out.contains("\nLocks and worktrees\n"), "{out}");
    assert!(
        out.contains("\n  lock_idle_mins = 60  (default)\n"),
        "{out}"
    );
    assert!(
        out.contains("\n  brain_writer = false  (default)  read-only\n"),
        "{out}"
    );
    assert!(out.contains("\ntargets.toml: "), "{out}");
    let out = stdout_of(&mut env.ctl(&["settings", "set", "lock_idle_mins", "15"]));
    assert!(
        out.contains("\n  lock_idle_mins = 15  (from the file)  restart needed\n"),
        "{out}"
    );
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.ends_with("lock_idle_mins = 15\n"), "{text}");
    // The keys `Env` wrote stay.
    assert!(text.contains("jira_command = []"), "{text}");
    let out = stdout_of(&mut env.ctl(&["settings", "reset", "lock_idle_mins"]));
    assert!(
        out.contains("\n  lock_idle_mins = 60  (default)\n"),
        "{out}"
    );
    assert!(!std::fs::read_to_string(&config)
        .unwrap()
        .contains("lock_idle_mins"));
    let json = stdout_of(&mut env.ctl(&["settings", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["config_path"], config.display().to_string());
}

#[test]
fn ctl_settings_refuses_a_bad_value() {
    let env = Env::new();
    let config = env.home.join("config.toml");
    let before = std::fs::read(&config).unwrap();
    let out = env
        .ctl(&["settings", "set", "lock_idle_mins", "0"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("lock_idle_mins must be a whole number from 1 to 10080"),
        "{err}"
    );
    env.ctl(&["settings", "set", "shared_files", r#"["a.md","b/*.md"]"#])
        .assert()
        .success();
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(
        text.contains(r#"shared_files = ["a.md", "b/*.md"]"#),
        "{text}"
    );
    let out = env
        .ctl(&["settings", "set", "brain_writer", "true"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("brain_writer is read-only here; edit config.toml"),
        "{err}"
    );
    assert_ne!(std::fs::read(&config).unwrap(), before);
    assert!(!std::fs::read_to_string(&config)
        .unwrap()
        .contains("brain_writer"));
}

#[test]
fn ctl_start_send_status() {
    let env = Env::new();
    env.ctl(&["start", "2", "--cwd", "/tmp", "--label", "demo"])
        .assert()
        .success();
    env.ctl(&["send", "2", "--raw", "hello"]).assert().success();
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
        .env("OVERSEER_LAUNCHCTL", env.launchctl_stub())
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

#[test]
fn items_prints_tickets_then_indented_beads() {
    let env = Env::with_stub_sources();
    env.ctl(&["refresh"]).assert().success();
    let out = stdout_of(&mut env.ctl(&["items"]));
    let lines: Vec<&str> = out.lines().collect();
    // Source lines come first, tickets next, then beads under their ticket.
    let pos = |pre: &str| lines.iter().position(|l| l.starts_with(pre)).expect(pre);
    assert!(lines[pos("beads  ok  1 items")].contains("items"), "{out}");
    let jira = pos("jira  ok  1 items");
    let ticket = pos("DEMO-12  In Progress  -  Widget wiring");
    assert!(jira < ticket, "{out}");
    assert_eq!(
        lines[ticket], "DEMO-12  In Progress  -  Widget wiring",
        "{out}"
    );
    assert!(lines[ticket + 1].starts_with("  ov-3  "), "{out}");
    assert!(
        lines[ticket + 1].ends_with("DEMO-12 wire the widget"),
        "{out}"
    );
    let json = stdout_of(&mut env.ctl(&["items", "--json"]));
    assert!(json.contains("\"key\":\"DEMO-12\""), "{json}");
}

#[test]
fn assign_needs_confirm_exits_2() {
    let env = Env::with_stub_sources();
    env.ctl(&["refresh"]).assert().success();
    env.ctl(&["assign", "1", "DEMO-12"]).assert().success();
    let out = env
        .ctl(&["assign", "2", "ov-3"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Assign to S2 anyway?"), "{stdout}");
    env.ctl(&["assign", "2", "ov-3", "--yes"])
        .assert()
        .success();
    let out = stdout_of(&mut env.ctl(&["items"]));
    assert!(out.contains("  ov-3  open  S2  "), "{out}");
    env.ctl(&["assign", "3", "DEMO-12"]).assert().code(1);
    env.ctl(&["release", "DEMO-12"]).assert().success();
}

#[test]
fn claim_held_exits_2() {
    let env = Env::new();
    let out = stdout_of(&mut env.ctl(&["claim", "1", "repo:demo-api:main", "pushing"]));
    assert!(out.contains("repo:demo-api:main"), "{out}");
    let held = env
        .ctl(&["claim", "2", "repo:demo-api:main"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let held = String::from_utf8(held).unwrap();
    assert!(held.contains("S1"), "{held}");
}

#[test]
fn claim_error_exits_1() {
    let env = Env::new();
    env.ctl(&["claim", "9", "repo:demo-api:main"])
        .assert()
        .code(1);
}

#[test]
fn locks_lists_holder_and_waiter() {
    let env = Env::new();
    env.ctl(&["claim", "1", "repo:demo-api:main", "pushing"])
        .assert()
        .success();
    env.ctl(&["claim", "2", "repo:demo-api:main"])
        .assert()
        .code(2);
    let out = stdout_of(&mut env.ctl(&["locks"]));
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].starts_with("repo:demo-api:main  S1  claim  since "),
        "{out}"
    );
    assert!(lines[0].ends_with("for pushing"), "{out}");
    assert!(lines[1].starts_with("  waiting: S2 since "), "{out}");
    let json = stdout_of(&mut env.ctl(&["locks", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["claims"][0]["target"], "repo:demo-api:main");
    assert_eq!(v["waits"][0]["slot"], 2);
}

#[test]
fn release_two_args_is_a_target() {
    let env = Env::new();
    env.ctl(&["claim", "1", "repo:demo-api:main"])
        .assert()
        .success();
    env.ctl(&["release", "S1", "repo:demo-api:main"])
        .assert()
        .success();
    assert_eq!(stdout_of(&mut env.ctl(&["locks"])), "");
    env.ctl(&["claim", "3", "tf:demo"]).assert().success();
    env.ctl(&["release", "3", "tf:demo"]).assert().success();
    assert_eq!(stdout_of(&mut env.ctl(&["locks"])), "");
    env.ctl(&["claim", "3", "tf:demo"]).assert().success();
    env.ctl(&["force-release", "tf:demo"]).assert().success();
    let out = stdout_of(&mut env.ctl(&["locks"]));
    assert!(!out.contains("tf:demo  S3"), "{out}");
    assert!(out.contains("Jonathan released tf:demo"), "{out}");
}

#[test]
fn send_queues_a_relay() {
    let env = Env::new();
    // Nothing runs in S3: there is no session to type into.
    let out = env
        .ctl(&["send", "3", "check the DEMO-33 branch"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("S3 has no session to message."), "{err}");
    assert_eq!(stdout_of(&mut env.ctl(&["locks"])), "");
    // A session that is not Claude never reports idle, so the relay stays
    // pending.
    env.ctl(&["start", "3", "--cwd", "/tmp", "--", "/bin/sleep", "30"])
        .assert()
        .success();
    env.ctl(&["send", "3", "check the DEMO-33 branch"])
        .assert()
        .success();
    let out = stdout_of(&mut env.ctl(&["locks"]));
    let line = out.lines().find(|l| l.starts_with("relay")).expect(&out);
    assert!(line.starts_with("relay  S3  pending"), "{out}");
    assert!(line.ends_with("check the DEMO-33 branch"), "{out}");
}

fn brain_view(env: &Env) -> serde_json::Value {
    serde_json::from_str(&stdout_of(&mut env.ctl(&["brain", "--json"]))).unwrap()
}

#[test]
fn ctl_bd_passes_args_and_exit_code() {
    let env = Env::with_brain();
    let out = env
        .ctl(&["bd", "--", "create", "--body-file", "-", "--title", "x"])
        .write_stdin("heredoc body\n")
        .assert()
        .code(3)
        .get_output()
        .clone();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "bd out\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "bd err\n");
    let log = std::fs::read_to_string(env.home.join("bd.log")).unwrap();
    // The work list runs the same bd_command, so its calls are logged too.
    assert!(
        log.lines().any(|l| l == "create --body-file - --title x"),
        "{log}"
    );
    let stdin = std::fs::read_to_string(env.home.join("bd.log.stdin")).unwrap();
    assert_eq!(stdin, "heredoc body\n");
}

#[test]
fn ctl_bd_without_a_dash_never_reads_stdin() {
    use std::process::Stdio;
    let env = Env::with_brain();
    // Start the daemon first so the timing below is the bd call alone.
    env.ctl(&["brain"]).assert().success();
    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("overseer"))
        .env("OVERSEER_HOME", &env.home)
        .env("OVERSEER_CONFIG", env.home.join("config.toml"))
        .env("OVERSEER_LAUNCHCTL", env.launchctl_stub())
        .args(["ctl", "bd", "--", "list"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The pipe stays open and unwritten until the child is done.
    let _held = child.stdin.take().unwrap();
    let start = Instant::now();
    let status = wait_exit(&mut child, Duration::from_secs(5));
    assert!(start.elapsed() < Duration::from_secs(4));
    assert_eq!(status.code(), Some(3));
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
    assert_eq!(out, "bd out\n");
}

#[test]
fn ctl_commit_without_slot_is_jonathan() {
    let env = Env::with_brain();
    std::fs::write(env.home.join("vault/notes.md"), "one\n").unwrap();
    std::fs::write(env.home.join("vault/other.md"), "two\n").unwrap();
    let out = stdout_of(&mut env.ctl(&["commit", "-m", "add notes", "notes.md"]));
    assert!(out.starts_with("committed "), "{out}");
    assert!(out.trim_end().ends_with(": add notes"), "{out}");
    let v = brain_view(&env);
    assert_eq!(v["on"], true);
    assert_eq!(v["entries"][0]["from"]["type"], "You", "{v}");
    assert_eq!(v["entries"][0]["state"], "Committed");
    // A session commit is from its slot.
    std::fs::write(env.home.join("vault/more.md"), "three\n").unwrap();
    stdout_of(
        env.ctl(&["commit", "-m", "S2: more", "more.md"])
            .env("OVERSEER_SLOT", "S2"),
    );
    let v = brain_view(&env);
    assert_eq!(v["entries"][0]["from"]["type"], "Slot", "{v}");
    assert_eq!(v["entries"][0]["from"]["slot"], 2);
    // Only the named path was committed.
    let status = StdCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(env.home.join("vault"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&status.stdout), "?? other.md\n");
    let human = stdout_of(&mut env.ctl(&["brain"]));
    assert!(human.contains("2ndBrain writer is on"), "{human}");
    assert!(human.contains("0 queued  2 committed"), "{human}");
    assert!(human.contains("last backup  never"), "{human}");
}

#[test]
fn ctl_journal_and_current_session_read_stdin() {
    let env = Env::with_brain();
    let out = stdout_of(
        env.ctl(&["journal", "Work Log", "-"])
            .write_stdin("did the thing\n"),
    );
    assert!(out.starts_with("committed "), "{out}");
    let found = std::process::Command::new("grep")
        .args([
            "-r",
            "did the thing",
            env.home.join("vault/06-Daily").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(found.status.success(), "journal not written");
    // No slot, no --slot: refused.
    let err = env
        .ctl(&["current-session", "hello"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(
        String::from_utf8_lossy(&err.stderr).contains("--slot"),
        "{err:?}"
    );
    env.ctl(&["current-session", "--slot", "S2", "-"])
        .write_stdin("on DEMO-12\n")
        .assert()
        .success();
    let cs = std::fs::read_to_string(env.home.join("CURRENT_SESSION.md")).unwrap();
    assert!(
        cs.contains("<!-- overseer:S2 -->") && cs.contains("on DEMO-12"),
        "{cs}"
    );
    // A session may not write another slot's section.
    let err = env
        .ctl(&["current-session", "--slot", "S3", "not mine"])
        .env("OVERSEER_SLOT", "S6")
        .assert()
        .failure()
        .get_output()
        .clone();
    let said = String::from_utf8_lossy(&err.stderr);
    assert!(said.contains("S6") && said.contains("S3"), "{said}");
    let cs = std::fs::read_to_string(env.home.join("CURRENT_SESSION.md")).unwrap();
    assert!(!cs.contains("not mine"), "{cs}");
    // Its own slot named again is fine.
    env.ctl(&["current-session", "--slot", "6", "mine"])
        .env("OVERSEER_SLOT", "S6")
        .assert()
        .success();
}

#[test]
fn brain_commands_say_when_the_writer_is_off() {
    let env = Env::new();
    let err = env
        .ctl(&["commit", "-m", "x", "a.md"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(
        String::from_utf8_lossy(&err.stderr).contains("the 2ndBrain writer is off"),
        "{err:?}"
    );
    let out = stdout_of(&mut env.ctl(&["brain"]));
    assert!(out.contains("writer is off"), "{out}");
}

#[test]
fn plan_approve_inside_a_session_is_refused() {
    let env = Env::new();
    let out = env
        .ctl(&["plan", "approve", "1"])
        .env("OVERSEER_SLOT", "S3")
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("only Jonathan can approve or reject a plan"),
        "{stderr}"
    );
}

#[test]
fn plan_approve_as_jonathan_runs_it() {
    let env = Env::new();
    let plan =
        r#"{"summary": "one lane", "slots": [{"slot": "S3", "label": "demo", "cwd": "/tmp"}]}"#;
    let out = stdout_of(env.ctl(&["plan", "propose", "-"]).write_stdin(plan));
    assert!(out.contains("Plan 1 is waiting for Jonathan."), "{out}");
    let listed = stdout_of(&mut env.ctl(&["plan"]));
    assert!(listed.contains("Plan 1  pending"), "{listed}");
    let board = stdout_of(&mut env.ctl(&["board"]));
    assert!(
        board.contains("Plan 1 waiting for Jonathan since "),
        "{board}"
    );
    stdout_of(&mut env.ctl(&["plan", "approve", "1"]));
    let status = stdout_of(&mut env.ctl(&["status"]));
    let s3 = status.lines().find(|l| l.starts_with("S3")).unwrap();
    assert!(s3.starts_with("S3  running"), "{status}");
    let listed = stdout_of(&mut env.ctl(&["plan"]));
    assert!(listed.contains("Plan 1  approved"), "{listed}");
    assert!(listed.contains("S3: started in /tmp"), "{listed}");
    // A second approval finds it settled.
    let again = env
        .ctl(&["plan", "approve", "1"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("plan 1 is no longer pending"),
        "{again:?}"
    );
}

#[test]
fn report_needs_a_slot_and_summary_and_reject_work() {
    let env = Env::new();
    let out = env
        .ctl(&["report", "done"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("report needs a slot; run it from a session"),
        "{out:?}"
    );
    env.ctl(&["summary", "All quiet."]).assert().success();
    let board = stdout_of(&mut env.ctl(&["board"]));
    assert!(board.contains("): All quiet."), "{board}");
    let plan = r#"{"summary": "x", "slots": [{"slot": 2, "cwd": "/tmp"}]}"#;
    stdout_of(env.ctl(&["plan", "propose", "-"]).write_stdin(plan));
    let out = stdout_of(&mut env.ctl(&["plan", "reject", "1", "not now"]));
    assert_eq!(out.trim(), "Plan 1 rejected.");
}

fn uid() -> String {
    let out = StdCommand::new("id").arg("-u").output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn agent_plists(env: &Env) -> Vec<PathBuf> {
    match std::fs::read_dir(env.home.join("agent")) {
        Ok(rd) => rd.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn install_agent_in_a_custom_home_never_names_the_real_agent() {
    let env = Env::new();
    let out = stdout_of(&mut env.agent(&["install-agent", "--yes"]));
    let plists = agent_plists(&env);
    assert_eq!(plists.len(), 1, "{plists:?}");
    let plist = &plists[0];
    let name = plist.file_name().unwrap().to_string_lossy().into_owned();
    let label = name.strip_suffix(".plist").unwrap();
    assert!(label.starts_with("uk.co.techize.overseer-home-"), "{name}");
    let xml = std::fs::read_to_string(plist).unwrap();
    assert!(xml.contains("<key>OVERSEER_HOME</key>"), "{xml}");
    assert!(xml.contains(&format!("<string>{label}</string>")), "{xml}");
    assert!(
        !env.home.join("home/Library/LaunchAgents").exists(),
        "a plist went to the LaunchAgents folder"
    );
    assert!(out.contains(label), "{out}");
    let log = env.launchctl_log();
    let bootstrap = format!("bootstrap gui/{} {}/agent/", uid(), env.home.display());
    assert!(
        log.lines().any(|l| l.starts_with(&bootstrap)),
        "no {bootstrap:?} in:\n{log}"
    );
    for line in log.lines().filter(|l| l.contains("uk.co.techize.overseer")) {
        assert!(line.contains("uk.co.techize.overseer-home-"), "{line}");
    }
}

#[test]
fn uninstall_agent_in_a_custom_home_removes_its_plist() {
    let env = Env::new();
    env.agent(&["install-agent"]).assert().success();
    assert_eq!(agent_plists(&env).len(), 1);
    let out = stdout_of(&mut env.agent(&["uninstall-agent"]));
    assert!(agent_plists(&env).is_empty(), "{out}");
    assert!(out.contains("removed "), "{out}");
    for line in env
        .launchctl_log()
        .lines()
        .filter(|l| l.contains("uk.co.techize.overseer"))
    {
        assert!(line.contains("uk.co.techize.overseer-home-"), "{line}");
    }
}

/// The `S<n>` line of `ctl status`.
fn status_line(env: &Env, n: u8) -> String {
    let out = stdout_of(&mut env.ctl(&["status"]));
    let name = format!("S{n} ");
    out.lines()
        .find(|l| l.starts_with(&name))
        .unwrap_or_else(|| panic!("no S{n} in:\n{out}"))
        .to_string()
}

/// The pid on a `running  pid <p>` status line, or the daemon's in `ctl version`.
fn pid_after(text: &str, marker: &str) -> u32 {
    let rest = &text[text.find(marker).unwrap_or_else(|| panic!("{text}")) + marker.len()..];
    rest.split_whitespace().next().unwrap().parse().unwrap()
}

const WARNING_START: &str = "The daemon (build dev) has 1 running session: S1 demo lane. \
    Restarting it ends them; each comes back as resumable, and r on its card resumes it.";

/// S1 running the sleeping `claude`, labelled `demo lane`.
fn start_claude(env: &Env) -> u32 {
    let cwd = env.home.display().to_string();
    env.ctl_claude(&["start", "1", "--cwd", &cwd, "--label", "demo lane"])
        .assert()
        .success();
    let s1 = status_line(env, 1);
    assert!(s1.starts_with("S1  running  pid "), "{s1}");
    pid_after(&s1, "pid ")
}

#[test]
fn install_agent_keeps_a_daemon_with_running_sessions() {
    let env = Env::new();
    let pid = start_claude(&env);
    let out = stdout_of(
        env.agent_claude(&["install-agent"])
            .pipe_stdin("/dev/null")
            .unwrap(),
    );
    assert!(out.contains(WARNING_START), "{out}");
    assert!(
        out.contains(
            "The daemon keeps running build dev. Run overseer ctl restart when you are ready to finish the upgrade."
        ),
        "{out}"
    );
    let s1 = status_line(&env, 1);
    assert!(s1.starts_with("S1  running  pid "), "{s1}");
    assert_eq!(pid_after(&s1, "pid "), pid);
    // The stub's `print` fails, so `bootout` is never logged anyway; the
    // `bootstrap` check and the unchanged pid are the real guards.
    let log = env.launchctl_log();
    assert!(!log.contains("bootout"), "{log}");
    assert!(!log.contains("bootstrap"), "{log}");
}

#[test]
fn install_agent_yes_restarts_and_the_session_is_resumable() {
    let env = Env::new();
    start_claude(&env);
    let out = stdout_of(&mut env.agent_claude(&["install-agent", "--yes"]));
    assert!(out.contains(WARNING_START), "{out}");
    assert!(
        out.contains("Restarted the daemon (build dev to dev)."),
        "{out}"
    );
    // The stub's bootstrap starts nothing and its kickstart fails, so this
    // autostart spawns the daemon, which finds S1 in the ledger.
    let s1 = status_line(&env, 1);
    assert!(s1.starts_with("S1  resumable"), "{s1}");
}

#[test]
fn install_agent_twice_restarts_nothing() {
    let env = Env::new();
    env.ctl(&["status"]).assert().success();
    let first = stdout_of(&mut env.agent(&["install-agent"]));
    assert!(
        first.contains("Restarted the daemon (build dev to dev)."),
        "{first}"
    );
    env.ctl(&["status"]).assert().success();
    let second = stdout_of(&mut env.agent(&["install-agent"]));
    assert!(
        second.contains("The daemon already runs build dev; nothing to restart."),
        "{second}"
    );
    assert!(!second.contains("Restarted"), "{second}");
}

#[test]
fn install_agent_no_restart_keeps_an_idle_daemon() {
    let env = Env::new();
    env.ctl(&["status"]).assert().success();
    let before = pid_after(&stdout_of(&mut env.ctl(&["version"])), "  pid ");
    let out = stdout_of(&mut env.agent(&["install-agent", "--no-restart"]));
    assert!(out.contains("The daemon keeps running build dev."), "{out}");
    let after = pid_after(&stdout_of(&mut env.ctl(&["version"])), "  pid ");
    assert_eq!(before, after);
    // The stub's `print` fails, so `bootout` is never logged anyway; the
    // `bootstrap` check and the unchanged pid are the real guards.
    let log = env.launchctl_log();
    assert!(!log.contains("bootout"), "{log}");
    assert!(!log.contains("bootstrap"), "{log}");
}

#[test]
fn install_agent_with_no_daemon_starts_it() {
    let env = Env::new();
    let out = stdout_of(&mut env.agent(&["install-agent"]));
    assert!(out.contains("Started the daemon (build dev)."), "{out}");
}

#[test]
fn ctl_restart_yes_brings_the_daemon_back() {
    let env = Env::new();
    start_claude(&env);
    let before = pid_after(&stdout_of(&mut env.ctl(&["version"])), "  pid ");
    let out = stdout_of(&mut env.ctl(&["restart", "--yes"]));
    let warned = out.find(WARNING_START).unwrap_or_else(|| panic!("{out}"));
    let restarted = out
        .find("Restarted the daemon (build dev to dev).")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(warned < restarted, "{out}");
    let after = pid_after(&stdout_of(&mut env.ctl(&["version"])), "  pid ");
    assert_ne!(before, after);
    let s1 = status_line(&env, 1);
    assert!(s1.starts_with("S1  resumable"), "{s1}");
}

#[test]
fn ctl_restart_without_a_terminal_keeps_running_sessions() {
    let env = Env::new();
    let pid = start_claude(&env);
    let out = stdout_of(env.ctl(&["restart"]).pipe_stdin("/dev/null").unwrap());
    assert!(out.contains(WARNING_START), "{out}");
    assert!(
        out.contains(
            "Kept the daemon (build dev) because 1 running session would end: S1 demo lane. \
             Run overseer ctl restart --yes to restart it anyway."
        ),
        "{out}"
    );
    assert!(!out.contains("when you are ready"), "{out}");
    assert_eq!(pid_after(&status_line(&env, 1), "pid "), pid);
}

/// How a fake daemon on `<home>/overseer.sock` treats requests.
#[derive(Clone, Copy)]
enum Fake {
    /// Reads every request and never answers.
    Silent,
    /// Answers `DaemonInfo` as build `b0`, errs on `ListSlots`.
    ListErrs,
    /// Answers `DaemonInfo` as build `b0`, never answers `ListSlots`.
    ListHangs,
}

/// Serves `how` on this home's socket until the test ends. Remove the
/// socket before `Env` drops, so its `ctl shutdown` does not wait on it.
fn fake_daemon(env: &Env, how: Fake) -> PathBuf {
    use overseer_proto::{read_frame, write_frame, DaemonInfo, Frame, Request, Response};
    let socket = env.home.join("overseer.sock");
    let (ready, up) = std::sync::mpsc::channel();
    let path = socket.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = tokio::net::UnixListener::bind(&path).unwrap();
            ready.send(()).unwrap();
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    while let Ok(Some(Frame::Request { seq, req })) = read_frame(&mut s).await {
                        let resp = match (how, req) {
                            (Fake::Silent, _) => None,
                            (_, Request::DaemonInfo) => Some(Response::DaemonInfo(DaemonInfo {
                                version: "0.1.0".into(),
                                build: "b0".into(),
                                pid: 1,
                                started_at: 0,
                                exe: "/nonexistent/overseer".into(),
                            })),
                            (Fake::ListErrs, _) => Some(Response::Error {
                                message: "not now".into(),
                            }),
                            (Fake::ListHangs, _) => None,
                        };
                        match resp {
                            Some(resp) => {
                                let _ = write_frame(&mut s, &Frame::Response { seq, resp }).await;
                            }
                            None => tokio::time::sleep(Duration::from_secs(3600)).await,
                        }
                    }
                });
            }
        });
    });
    up.recv().unwrap();
    socket
}

const UNLISTED_B0: &str =
    "The daemon (build b0) did not list its sessions, so some may be running.";

#[test]
fn install_agent_keeps_a_daemon_whose_sessions_cannot_be_listed() {
    let env = Env::new();
    let socket = fake_daemon(&env, Fake::ListErrs);
    let out = stdout_of(
        env.agent(&["install-agent"])
            .pipe_stdin("/dev/null")
            .unwrap(),
    );
    std::fs::remove_file(socket).unwrap();
    assert!(out.contains(UNLISTED_B0), "{out}");
    assert!(out.contains("The daemon keeps running build b0."), "{out}");
    assert!(!out.contains("Restarted"), "{out}");
    let log = env.launchctl_log();
    assert!(!log.contains("bootstrap"), "{log}");
}

#[test]
fn install_agent_keeps_a_daemon_that_does_not_answer() {
    let env = Env::new();
    let socket = fake_daemon(&env, Fake::Silent);
    let out = stdout_of(
        env.agent(&["install-agent"])
            .pipe_stdin("/dev/null")
            .unwrap(),
    );
    std::fs::remove_file(socket).unwrap();
    assert!(
        out.contains("The daemon (build unknown) did not list its sessions"),
        "{out}"
    );
    assert!(
        out.contains("The daemon keeps running build unknown."),
        "{out}"
    );
    assert!(!out.contains("Started"), "{out}");
    let log = env.launchctl_log();
    assert!(!log.contains("bootstrap"), "{log}");
}

#[test]
fn ctl_restart_keeps_a_daemon_that_does_not_list_its_sessions() {
    let env = Env::new();
    let socket = fake_daemon(&env, Fake::ListHangs);
    let out = stdout_of(env.ctl(&["restart"]).pipe_stdin("/dev/null").unwrap());
    std::fs::remove_file(socket).unwrap();
    assert!(out.contains(UNLISTED_B0), "{out}");
    assert!(
        out.contains(
            "Kept the daemon (build b0) because its sessions could not be listed. \
             Run overseer ctl restart --yes to restart it anyway."
        ),
        "{out}"
    );
    assert!(!out.contains("Restarted"), "{out}");
}

#[test]
fn ctl_restart_kickstarts_this_homes_agent_when_it_is_loaded() {
    let env = Env::new();
    env.ctl(&["status"]).assert().success();
    env.agent(&["install-agent"]).assert().success();
    // The stub's bootstrap starts nothing, so this spawns the daemon.
    env.ctl(&["status"]).assert().success();
    // Now launchd "has" the agent: print and kickstart succeed.
    env.write_launchctl_stub_failing("none");
    let out = stdout_of(&mut env.ctl(&["restart", "--yes"]));
    assert!(
        out.contains("Restarted the daemon (build dev to dev)."),
        "{out}"
    );
    let log = env.launchctl_log();
    let kick = format!("kickstart -k gui/{}/uk.co.techize.overseer-home-", uid());
    assert!(
        log.lines().any(|l| l.starts_with(&kick)),
        "no {kick:?} in:\n{log}"
    );
}
