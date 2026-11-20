//! scripts/uninstall.sh against a temporary home, with a fake `overseer` and
//! a fake `launchctl` first on PATH. Nothing here can reach the real state
//! dir, agent or app: HOME and OVERSEER_HOME are temporary and the app
//! directory under them is empty.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

struct Setup {
    root: tempfile::TempDir,
}

impl Setup {
    fn new() -> Setup {
        let s = Setup {
            root: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(s.holders()).unwrap();
        std::fs::create_dir_all(s.root.path().join("bin")).unwrap();
        s
    }

    fn holders(&self) -> PathBuf {
        self.root.path().join("home/holders")
    }

    fn write_exec(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.root.path().join("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A pid file for `slot` naming `pid`, and the exit file its holder left.
    fn holder(&self, slot: u8, pid: u32) {
        std::fs::write(
            self.holders().join(format!("S{slot}.pid")),
            format!("{{\"holder\":{pid},\"child\":{pid},\"version\":1}}"),
        )
        .unwrap();
        std::fs::write(self.holders().join(format!("S{slot}.exit")), "{}").unwrap();
    }

    fn run(&self) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/uninstall.sh");
        let home = self.root.path().join("home");
        let path = format!("{}:/usr/bin:/bin", self.root.path().join("bin").display());
        Command::new("/bin/sh")
            .arg(script)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root.path())
            .env("OVERSEER_HOME", &home)
            .env("OVERSEER_LAUNCHCTL", self.root.path().join("bin/launchctl"))
            .output()
            .unwrap()
    }
}

/// A `sleep 60` this test started; killed and reaped when dropped, so a
/// failing test leaves nothing behind.
struct Sleeper(Child);

impl Sleeper {
    fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sleeper() -> Sleeper {
    Sleeper(
        Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn a_refused_stop_all_falls_back_to_the_pid_files_and_clears_exit_files() {
    let s = Setup::new();
    s.write_exec("launchctl", "exit 0");
    // stop-all refuses; `hold --end` removes the pid file, as a holder does.
    s.write_exec(
        "overseer",
        r#"case "$1 $2" in
"ctl stop-all") echo "could not end S2; still running" >&2; exit 1 ;;
"hold --end") rm -f "$3"; exit 0 ;;
esac
exit 0"#,
    );
    s.holder(1, 999_999);
    s.holder(2, 999_998);
    let out = s.run();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{out:?}");
    assert!(err.contains("could not end S2"), "{err}");
    // Everything ended: holders/ is gone.
    assert!(!s.holders().exists(), "{err}");
}

#[test]
fn a_holder_still_running_keeps_holders_but_not_the_exit_files_of_the_ended() {
    let s = Setup::new();
    s.write_exec("launchctl", "exit 0");
    let live = sleeper();
    // S1 ends; S2's `hold --end` leaves it running and says so.
    s.write_exec(
        "overseer",
        r#"case "$1 $2" in
"ctl stop-all") echo "no daemon is running" >&2; exit 1 ;;
"hold --end") case "$3" in *S1.pid) rm -f "$3"; exit 0 ;; esac; exit 1 ;;
esac
exit 0"#,
    );
    s.holder(1, 999_999);
    s.holder(2, live.id());
    let out = s.run();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    drop(live);
    assert!(err.contains("still running: S2"), "{err}");
    assert!(s.holders().join("S2.pid").exists());
    assert!(s.holders().join("S2.exit").exists());
    assert!(
        !s.holders().join("S1.exit").exists(),
        "exit file of S1 kept"
    );
}

#[test]
fn a_successful_stop_all_still_sweeps_what_is_left() {
    let s = Setup::new();
    s.write_exec("launchctl", "exit 0");
    s.write_exec(
        "overseer",
        r#"case "$1 $2" in
"ctl stop-all") exit 0 ;;
"hold --end") rm -f "$3"; exit 0 ;;
esac
exit 0"#,
    );
    s.holder(1, 999_999);
    let out = s.run();
    assert!(out.status.success(), "{out:?}");
    assert!(!s.holders().exists());
}

/// A fake `overseer` whose `ctl stop-all` finds no daemon and whose
/// `hold --end` is the real binary's, identity check and all.
fn real_hold_end(s: &Setup) {
    s.write_exec(
        "overseer",
        &format!(
            r#"case "$1 $2" in
"ctl stop-all") echo "no daemon is running" >&2; exit 1 ;;
"hold --end") exec "{}" hold --end "$3" ;;
esac
exit 0"#,
            env!("CARGO_BIN_EXE_overseer")
        ),
    );
}

/// After a reboot a pid file's holder pid can belong to some other live
/// process. It is not a holder: nothing is signalled, the session is not
/// reported as still running, and `holders/` goes.
#[test]
fn a_stale_pid_file_naming_another_process_is_not_still_running() {
    let s = Setup::new();
    s.write_exec("launchctl", "exit 0");
    real_hold_end(&s);
    let mut other = sleeper();
    s.holder(1, other.id());
    let out = s.run();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    let signalled = other.0.try_wait().unwrap();
    drop(other);
    assert!(signalled.is_none(), "the other process was signalled");
    assert!(!err.contains("still running"), "{err}");
    assert!(!s.holders().exists(), "{err}");
}

/// A live process whose identity cannot be read may be the holder: it is
/// never signalled, and it keeps `holders/` as still running.
#[test]
fn a_holder_that_cannot_be_identified_is_still_running() {
    let s = Setup::new();
    s.write_exec("launchctl", "exit 0");
    real_hold_end(&s);
    // Killed but not reaped: alive to a signal check, not identifiable.
    let mut zombie = sleeper();
    zombie.0.kill().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    s.holder(1, zombie.id());
    let out = s.run();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    drop(zombie);
    assert!(err.contains("still running: S1"), "{err}");
    assert!(s.holders().join("S1.pid").exists());
    assert!(s.holders().join("S1.exit").exists());
}
