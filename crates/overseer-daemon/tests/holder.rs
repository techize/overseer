//! The holder process: one PTY served over a Unix socket.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use overseer_daemon::holder::{
    end_by_pid, exit_path, holder_alive, holder_gone, pid_path, read_frame, socket_path,
    write_frame, ExitFile, Frame, PidFile, HOLDER_PROTOCOL,
};
use overseer_daemon::pty::SpawnSpec;
use overseer_daemon::slot::{Attached, Slot};

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
        Holder::start_from(Path::new(HOLDER), script)
    }

    /// [`Holder::start`], running the holder from `program`.
    fn start_from(program: &Path, script: &str) -> Holder {
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
        let mut proc = Command::new(program)
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

/// A daemon that reads slowly is well behind when the child exits, with a
/// full queue of output still to go. It still gets the whole tail, and Exit
/// after it.
#[test]
fn exit_comes_after_the_whole_tail() {
    let mut h = Holder::start("seq 1 40000; exit 3");
    let mut s = h.connect();
    let mut seen = skip_greeting(&mut s).into_bytes();
    let code = loop {
        // About 3s for a full queue, well past the holder's 1s drain wait.
        std::thread::sleep(Duration::from_millis(3));
        match frame(&mut s) {
            Frame::Output(b) => seen.extend(b),
            Frame::Exit { code, .. } => break code,
            other => panic!("expected Output or Exit, got {other:?}"),
        }
    };
    assert_eq!(code, Some(3));
    write_frame(&mut s, &Frame::ExitSeen).unwrap();
    let text = String::from_utf8_lossy(&seen).replace('\r', "");
    let want: String = (1..=40000).map(|n| format!("{n}\n")).collect();
    assert!(
        text == want,
        "tail lost: got {} bytes of {}, ending {:?}",
        text.len(),
        want.len(),
        &text[text.len().saturating_sub(40)..]
    );
    // Nothing follows Exit.
    assert!(matches!(read_frame(&mut s), Ok(None) | Err(_)));
    h.wait_exit(WAIT);
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

/// A process this test started, killed and reaped when dropped.
struct Reap(Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `/bin/sleep 30`, leading its own process group as a holder's child does.
fn other_process() -> Reap {
    use std::os::unix::process::CommandExt;
    Reap(
        Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap(),
    )
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn holder_alive_knows_a_holder_from_another_process() {
    let h = Holder::start("sleep 30");
    assert!(holder_alive(&h.pid_file()));
    let other = other_process();
    let id = other.0.id();
    let named = PidFile {
        holder: id,
        child: id,
        version: HOLDER_PROTOCOL,
        holder_start: None,
        child_start: None,
    };
    assert!(!holder_alive(&named), "a sleep taken for a holder");
    let gone = PidFile {
        holder: u32::MAX,
        child: u32::MAX,
        version: HOLDER_PROTOCOL,
        holder_start: None,
        child_start: None,
    };
    assert!(!holder_alive(&gone));
}

#[test]
fn end_by_pid_ends_the_child_and_then_the_holder() {
    let mut h = Holder::start("sleep 30");
    let p = h.pid_file();
    end_by_pid(&p);
    h.wait_for(|| !pid_alive(p.child), "the child to end");
    h.wait_exit(WAIT);
}

#[test]
fn end_by_pid_never_signals_another_process() {
    let mut other = other_process();
    let id = other.0.id();
    end_by_pid(&PidFile {
        holder: id,
        child: id,
        version: HOLDER_PROTOCOL,
        holder_start: None,
        child_start: None,
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(other.0.try_wait().unwrap().is_none(), "signalled");
}

#[test]
fn a_hello_that_cannot_be_read_is_unsupported() {
    let state = tempfile::Builder::new()
        .prefix("ovh")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = state.path().join("S2.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let serve = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let payload = b"not a hello";
        let mut buf = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
        buf.push(1);
        buf.extend_from_slice(payload);
        conn.write_all(&buf).unwrap();
        // Held open until the daemon side is done with it.
        let mut rest = Vec::new();
        let _ = std::io::Read::read_to_end(&mut conn, &mut rest);
    });
    let (tx, _rx) = tokio::sync::broadcast::channel(16);
    let attached = Slot::attach_held(&sock, tx).unwrap();
    assert!(matches!(attached, Attached::Unreadable));
    drop(attached);
    serve.join().unwrap();
}

/// A copy of the test holder in a fresh folder, so a test can do to it what
/// an install does to the app's binary.
fn holder_copy() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("ovb")
        .tempdir_in("/tmp")
        .unwrap();
    let bin = dir.path().join("overseer-test-holder");
    std::fs::copy(HOLDER, &bin).unwrap();
    (dir, bin)
}

/// A holder started from a binary that is then removed, as `bundle.sh`'s
/// `rm -f`, and with `replace` another file copied to the same path, as
/// its `cp`, is still known for a holder and can still be ended.
fn holder_outlives_its_binary(replace: bool) {
    let (_dir, bin) = holder_copy();
    let mut h = Holder::start_from(&bin, "sleep 30");
    std::fs::remove_file(&bin).unwrap();
    if replace {
        std::fs::copy("/bin/echo", &bin).unwrap();
    }
    let p = h.pid_file();
    assert!(holder_alive(&p), "a live holder taken for something else");
    assert!(!holder_gone(&p), "a live holder taken for dead");
    end_by_pid(&p);
    h.wait_for(|| !pid_alive(p.child), "the child to end");
    h.wait_exit(WAIT);
}

#[test]
fn a_holder_whose_binary_was_replaced_is_still_a_holder() {
    holder_outlives_its_binary(true);
}

#[test]
fn a_holder_whose_binary_was_removed_is_still_a_holder() {
    holder_outlives_its_binary(false);
}

#[test]
fn another_program_named_by_a_pid_file_is_not_a_holder() {
    let other = other_process();
    let id = other.0.id();
    let p = PidFile {
        holder: id,
        child: id,
        version: HOLDER_PROTOCOL,
        holder_start: None,
        child_start: None,
    };
    assert!(holder_gone(&p), "sleep taken for a holder");
}

/// A live process with a holder's name that serves nothing, leading its
/// own process group: what a stale pid file's holder pid may have become.
fn idle_holder() -> Reap {
    use std::os::unix::process::CommandExt;
    Reap(
        Command::new(HOLDER)
            .arg("idle")
            .process_group(0)
            .spawn()
            .unwrap(),
    )
}

fn start_of(pid: u32) -> u64 {
    overseer_daemon::holder::process_start(pid).expect("start time readable")
}

#[test]
fn a_reused_holder_pid_is_not_a_holder() {
    let idle = idle_holder();
    let id = idle.0.id();
    let start = start_of(id);
    let named = |holder_start| PidFile {
        holder: id,
        child: id,
        version: HOLDER_PROTOCOL,
        holder_start,
        child_start: None,
    };
    assert!(
        holder_alive(&named(Some(start))),
        "its own holder not known"
    );
    // Another process that happens to run a holder's binary now has the pid.
    let reused = named(Some(start + 1));
    assert!(!holder_alive(&reused), "a reused pid taken for its holder");
    assert!(holder_gone(&reused), "a reused pid not taken for gone");
    // A pid file from before start times were recorded: the name decides.
    assert!(holder_alive(&named(None)));
}

#[test]
fn a_reused_child_pid_is_never_signalled() {
    let mut holder = idle_holder();
    let mut child = other_process();
    let p = PidFile {
        holder: holder.0.id(),
        child: child.0.id(),
        version: HOLDER_PROTOCOL,
        holder_start: Some(start_of(holder.0.id())),
        // The child the holder wrote down is gone; another process got its pid.
        child_start: Some(start_of(child.0.id()) + 1),
    };
    end_by_pid(&p);
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "a reused child pid was signalled"
    );
    // The holder itself is still shown to be one, so it is ended.
    let start = Instant::now();
    while holder.0.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < WAIT, "the holder was not ended");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_holder_records_its_own_and_its_childs_start() {
    let h = Holder::start("sleep 30");
    let p = h.pid_file();
    assert_eq!(p.holder_start, Some(start_of(p.holder)));
    assert_eq!(p.child_start, Some(start_of(p.child)));
}

/// A holder whose socket is bound but not listening for a moment, as
/// between a bind and its listen, still starts: the daemon retries the connect until the start deadline.
#[test]
fn a_socket_that_refuses_at_first_is_retried() {
    use std::os::unix::fs::PermissionsExt;
    let state = tempfile::Builder::new()
        .prefix("ovh")
        .tempdir_in("/tmp")
        .unwrap();
    let bin = tempfile::Builder::new()
        .prefix("ovb")
        .tempdir_in("/tmp")
        .unwrap();
    let late = bin.path().join("late-holder");
    let sock = socket_path(state.path(), SLOT);
    std::fs::write(
        &late,
        format!(
            "#!/bin/sh\nOVERSEER_TEST_HOLDER_LATE='{sock}' exec '{HOLDER}' \"$@\"\n",
            sock = sock.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&late, std::fs::Permissions::from_mode(0o755)).unwrap();
    let spec = SpawnSpec {
        slot: SLOT,
        label: "demo".into(),
        cwd: PathBuf::from("/tmp"),
        argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        cols: 80,
        rows: 24,
        run: RUN.into(),
    };
    let started = overseer_daemon::holder::client::start(&spec, &late, state.path());
    let pid = pid_path(state.path(), SLOT);
    let start = Instant::now();
    while !pid.exists() && start.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Ok(bytes) = std::fs::read(&pid) {
        if let Ok(p) = serde_json::from_slice::<PidFile>(&bytes) {
            end_by_pid(&p);
            let start = Instant::now();
            while pid_alive(p.holder) && start.elapsed() < WAIT {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    started.expect("a socket refusing at first was not retried");
}
