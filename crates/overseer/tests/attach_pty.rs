//! Drives `overseer attach 1` inside a real pty with a temp OVERSEER_HOME and
//! the fake-claude script.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fake-claude.sh");
const RESTORE: &str = "\x1b[?25h";

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Home {
        let h = std::env::temp_dir().join(format!("ovpty-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&h).unwrap();
        // Keep the daemon off the real beads and Jira.
        std::fs::write(h.join("config.toml"), "beads_dir = \"/nonexistent/overseer-test-beads\"\nbrain_dir = \"/nonexistent/overseer-test-brain\"\njira_command = []\n").unwrap();
        Home(h)
    }
    fn ctl(&self, args: &[&str]) -> String {
        let out = StdCommand::new(assert_cmd::cargo::cargo_bin("overseer"))
            .env("OVERSEER_HOME", &self.0)
            .env("OVERSEER_CONFIG", self.0.join("config.toml"))
            .env("OVERSEER_DEFAULT_COMMAND", SCRIPT)
            .arg("ctl")
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.ctl(&["shutdown"]);
        std::thread::sleep(Duration::from_millis(200));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Attached {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    seen: String,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Attached {
    fn start(home: &Home) -> Attached {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(assert_cmd::cargo::cargo_bin("overseer"));
        cmd.args(["attach", "1"]);
        cmd.env("OVERSEER_HOME", &home.0);
        cmd.env("OVERSEER_CONFIG", home.0.join("config.toml"));
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Attached {
            child,
            writer,
            rx,
            seen: String::new(),
            _master: pair.master,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn wait_for(&mut self, needle: &str) {
        let end = Instant::now() + Duration::from_secs(8);
        while !self.seen.contains(needle) {
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(d) => self.seen.push_str(&String::from_utf8_lossy(&d)),
                Err(_) => panic!("never saw {needle:?}; got {:?}", self.seen),
            }
        }
    }

    fn wait_exit(&mut self) -> u32 {
        let end = Instant::now() + Duration::from_secs(8);
        loop {
            while let Ok(d) = self.rx.try_recv() {
                self.seen.push_str(&String::from_utf8_lossy(&d));
            }
            if let Some(st) = self.child.try_wait().unwrap() {
                // Pick up output flushed just before exit.
                std::thread::sleep(Duration::from_millis(100));
                while let Ok(d) = self.rx.try_recv() {
                    self.seen.push_str(&String::from_utf8_lossy(&d));
                }
                return st.exit_code();
            }
            assert!(Instant::now() < end, "attach did not exit: {:?}", self.seen);
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn started(tag: &str) -> Home {
    let home = Home::new(tag);
    home.ctl(&["start", "1", "--cwd", "/tmp", "--", SCRIPT]);
    home
}

#[test]
fn attach_round_trip_and_detach() {
    let home = started("detach");
    let mut a = Attached::start(&home);
    a.wait_for("READY");
    a.send(b"hi\r");
    a.wait_for("ECHO:hi");
    a.send(b"\x1d");
    assert_eq!(a.wait_exit(), 0, "{:?}", a.seen);
    assert!(
        a.seen
            .contains("Detached from S1. The session is still running."),
        "{:?}",
        a.seen
    );
    assert!(
        a.seen.contains(RESTORE),
        "no terminal restore: {:?}",
        a.seen
    );
    let status = home.ctl(&["status"]);
    assert!(status.contains("S1  running"), "{status}");
}

#[test]
fn attach_slot_exit() {
    let home = started("exit");
    let mut a = Attached::start(&home);
    a.wait_for("READY");
    a.send(b"quit\r");
    assert_eq!(a.wait_exit(), 0, "{:?}", a.seen);
    assert!(a.seen.contains("S1 exited with code 3."), "{:?}", a.seen);
    assert!(
        a.seen.contains(RESTORE),
        "no terminal restore: {:?}",
        a.seen
    );
}
