//! The holder process: one PTY served over a Unix socket.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use overseer_daemon::holder::{
    exit_path, pid_path, read_frame, socket_path, write_frame, ExitFile, Frame, PidFile,
};
use overseer_daemon::pty::SpawnSpec;

const HOLDER: &str = env!("CARGO_BIN_EXE_overseer-test-holder");
const SLOT: u8 = 2;
const RUN: &str = "demo-run";
const WAIT: Duration = Duration::from_secs(5);

/// A running holder. Dropping it ends the holder and the child's process
/// group, so no test leaves processes behind, even when it fails.
struct Holder {
    proc: Child,
    state: tempfile::TempDir,
}

impl Holder {
    fn start(script: &str) -> Holder {
        // Unix socket paths are short on macOS: keep the state dir in /tmp.
        let state = tempfile::Builder::new()
            .prefix("ovh")
            .tempdir_in("/tmp")
            .unwrap();
        let spec = SpawnSpec {
            slot: SLOT,
            label: "demo".into(),
            cwd: PathBuf::from("/tmp"),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            cols: 80,
            rows: 24,
            run: RUN.into(),
        };
        let mut proc = Command::new(HOLDER)
            .arg("hold")
            .arg(state.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = proc.stdin.take().unwrap();
        stdin
            .write_all(&serde_json::to_vec(&spec).unwrap())
            .unwrap();
        drop(stdin);
        let h = Holder { proc, state };
        h.wait_for(|| h.pid_path().exists(), "pid file");
        h
    }

    fn dir(&self) -> &Path {
        self.state.path()
    }
    fn socket(&self) -> PathBuf {
        socket_path(self.dir(), SLOT)
    }
    fn pid_path(&self) -> PathBuf {
        pid_path(self.dir(), SLOT)
    }
    fn exit_path(&self) -> PathBuf {
        exit_path(self.dir(), SLOT)
    }
    fn pid_file(&self) -> PidFile {
        serde_json::from_slice(&std::fs::read(self.pid_path()).unwrap()).unwrap()
    }

    fn connect(&self) -> UnixStream {
        let s = UnixStream::connect(self.socket()).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        s
    }

    fn wait_for(&self, mut f: impl FnMut() -> bool, what: &str) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the holder process to exit and returns its exit code.
    fn wait_exit(&mut self, within: Duration) -> Option<i32> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.proc.try_wait().unwrap() {
                return status.code();
            }
            assert!(start.elapsed() < within, "holder did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if let Ok(bytes) = std::fs::read(self.pid_path()) {
            if let Ok(p) = serde_json::from_slice::<PidFile>(&bytes) {
                // The child leads its own session and process group.
                unsafe {
                    libc::kill(-(p.child as i32), libc::SIGKILL);
                    libc::kill(p.child as i32, libc::SIGKILL);
                }
            }
        }
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

fn frame(s: &mut UnixStream) -> Frame {
    read_frame(s).unwrap().expect("connection closed")
}

/// Reads frames until the Output seen so far contains `needle`.
fn output_until(s: &mut UnixStream, needle: &str) -> String {
    let mut seen = Vec::new();
    loop {
        match frame(s) {
            Frame::Output(b) => seen.extend(b),
            Frame::Exit { .. } => panic!("exited before {needle:?}"),
            _ => {}
        }
        let text = String::from_utf8_lossy(&seen).into_owned();
        if text.contains(needle) {
            return text;
        }
    }
}

/// Reads Replay frames up to the empty one that closes the replay; returns
/// the replayed bytes.
fn replay(s: &mut UnixStream) -> Vec<u8> {
    let mut seen = Vec::new();
    loop {
        match frame(s) {
            Frame::Replay(b) if b.is_empty() => return seen,
            Frame::Replay(b) => seen.extend(b),
            other => panic!("expected Replay, got {other:?}"),
        }
    }
}

/// Reads past Hello and the replay; returns the replayed bytes as text.
fn skip_greeting(s: &mut UnixStream) -> String {
    assert!(matches!(frame(s), Frame::Hello(_)));
    String::from_utf8_lossy(&replay(s)).into_owned()
}

fn until_exit(s: &mut UnixStream) -> Option<i32> {
    loop {
        if let Frame::Exit { code, .. } = frame(s) {
            return code;
        }
    }
}

#[test]
fn hello_then_replay_then_output() {
    let h = Holder::start("printf before; sleep 1; printf after; sleep 30");
    std::thread::sleep(Duration::from_millis(500));
    let mut s = h.connect();
    match frame(&mut s) {
        Frame::Hello(hello) => {
            assert_eq!(hello.version, 1);
            assert_eq!(hello.slot, SLOT);
            assert_eq!(hello.run, RUN);
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    assert!(String::from_utf8_lossy(&replay(&mut s)).contains("before"));
    output_until(&mut s, "after");
}

#[test]
fn an_empty_ring_replays_one_empty_frame() {
    let h = Holder::start("sleep 30");
    let mut s = h.connect();
    assert!(matches!(frame(&mut s), Frame::Hello(_)));
    assert_eq!(frame(&mut s), Frame::Replay(Vec::new()));
}

#[test]
fn a_slow_reader_gets_every_byte_of_a_burst() {
    let h = Holder::start("sleep 0.5; seq 1 200000; echo BURST-END; read l; seq 1 300000; exit 3");
    let mut s = h.connect();
    let mut seen = skip_greeting(&mut s).into_bytes();
    // Not reading for a while lets the queue fill, so the holder must wait.
    std::thread::sleep(Duration::from_millis(1500));
    let mut frames = 0u32;
    // Only the tail can hold the end marker; scanning it all would be slow.
    while !seen.ends_with(b"BURST-END\r\n") {
        match frame(&mut s) {
            Frame::Output(b) => seen.extend(b),
            other => panic!("expected Output, got {other:?}"),
        }
        frames += 1;
        if frames.is_multiple_of(2000) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let text = String::from_utf8_lossy(&seen).replace('\r', "");
    let mut want: String = (1..=200000).map(|n| format!("{n}\n")).collect();
    want.push_str("BURST-END\n");
    assert!(text == want, "burst lost or reordered bytes");

    // A second burst with nobody reading must not block the child.
    write_frame(&mut s, &Frame::Input(b"go\n".to_vec())).unwrap();
    drop(s);
    h.wait_for(|| h.exit_path().exists(), "exit file");
    let exit: ExitFile = serde_json::from_slice(&std::fs::read(h.exit_path()).unwrap()).unwrap();
    assert_eq!(exit.code, Some(3));
}

#[test]
fn input_reaches_the_child() {
    let h = Holder::start("read l; echo got:$l; sleep 30");
    let mut s = h.connect();
    skip_greeting(&mut s);
    write_frame(&mut s, &Frame::Input(b"hi\n".to_vec())).unwrap();
    output_until(&mut s, "got:hi");
}

#[test]
fn output_while_disconnected_is_replayed() {
    let h = Holder::start("sleep 1; printf later; sleep 30");
    let mut s = h.connect();
    let replay = skip_greeting(&mut s);
    assert!(!replay.contains("later"));
    drop(s);
    std::thread::sleep(Duration::from_millis(1500));
    let mut s = h.connect();
    assert!(skip_greeting(&mut s).contains("later"));
}

#[test]
fn a_second_connection_replaces_the_first() {
    let h = Holder::start("sleep 30");
    let mut first = h.connect();
    skip_greeting(&mut first);
    // Set before the replacement: macOS refuses socket options on a socket
    // that has been shut down.
    first
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut second = h.connect();
    skip_greeting(&mut second);
    loop {
        match read_frame(&mut first).unwrap() {
            None => break,
            Some(Frame::Output(_)) => {}
            Some(other) => panic!("unexpected frame on the old connection: {other:?}"),
        }
    }
}

#[test]
fn exit_with_no_daemon_writes_the_exit_file() {
    let h = Holder::start("sleep 0.5; exit 3");
    let start = Instant::now();
    while !h.exit_path().exists() {
        assert!(start.elapsed() < Duration::from_secs(3), "no exit file");
        std::thread::sleep(Duration::from_millis(20));
    }
    let exit: ExitFile = serde_json::from_slice(&std::fs::read(h.exit_path()).unwrap()).unwrap();
    assert_eq!(exit.code, Some(3));
    h.wait_for(
        || !h.socket().exists() && !h.pid_path().exists(),
        "socket and pid file removed",
    );
}

#[test]
fn exit_when_connected_is_sent() {
    let mut h = Holder::start("sleep 0.5; exit 0");
    let mut s = h.connect();
    skip_greeting(&mut s);
    assert_eq!(until_exit(&mut s), Some(0));
    write_frame(&mut s, &Frame::ExitSeen).unwrap();
    assert_eq!(h.wait_exit(Duration::from_secs(3)), Some(0));
    assert!(!h.exit_path().exists());
    assert!(!h.socket().exists());
    assert!(!h.pid_path().exists());
}

#[test]
fn a_lost_connection_while_exiting_writes_the_exit_file() {
    let mut h = Holder::start("sleep 0.5; exit 4");
    let mut s = h.connect();
    skip_greeting(&mut s);
    assert_eq!(until_exit(&mut s), Some(4));
    drop(s);
    assert_eq!(h.wait_exit(Duration::from_secs(3)), Some(0));
    let exit: ExitFile = serde_json::from_slice(&std::fs::read(h.exit_path()).unwrap()).unwrap();
    assert_eq!(exit.code, Some(4));
}

#[test]
fn kill_ends_the_child() {
    let h = Holder::start("sleep 30");
    let mut s = h.connect();
    skip_greeting(&mut s);
    write_frame(&mut s, &Frame::Kill).unwrap();
    assert_eq!(until_exit(&mut s), None);
}

#[test]
fn the_holder_is_its_own_session_leader() {
    let h = Holder::start("sleep 30");
    let pid = h.proc.id() as i32;
    assert_eq!(unsafe { libc::getsid(pid) }, pid);
}

#[test]
fn the_pid_file_names_holder_and_child() {
    let h = Holder::start("sleep 30");
    let p = h.pid_file();
    assert_eq!(p.holder, h.proc.id());
    assert_eq!(p.version, 1);
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &p.child.to_string()])
        .output()
        .unwrap();
    let ppid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(ppid, h.proc.id().to_string(), "the child's parent");
    let mut s = h.connect();
    match frame(&mut s) {
        Frame::Hello(hello) => assert_eq!(hello.child_pid, p.child),
        other => panic!("expected Hello, got {other:?}"),
    }
}
