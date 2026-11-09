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
        Env { home }
    }

    fn ctl(&self, args: &[&str]) -> Command {
        let mut c = Command::cargo_bin("overseer").unwrap();
        c.env("OVERSEER_HOME", &self.home)
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
