# Session Holder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Running claude sessions survive a daemon restart, so `git pull && make install` upgrades Overseer without ending them.

**Architecture:** The terminal code in `Slot` moves into a `LocalPty` type. A new `overseer hold` process per slot owns a `LocalPty` and serves it over a Unix socket. `Slot` gains a second backend that talks to a holder. On start, the daemon reattaches to every holder it finds, restores saved activity, replays spooled hook events, and gates relays until the screen has redrawn.

**Tech Stack:** Rust workspace (overseer-proto, overseer-daemon, overseer CLI), portable-pty, tokio, serde_json, libc.

**Spec:** `docs/superpowers/specs/2026-11-19-session-holder-design.md` (issue #62)

## Global Constraints

- Public repo: invented names only in tests, docs and fixtures (DEMO-n, demo.atlassian.net, demo@example.test, "Demo User").
- No em dashes anywhere, in code comments, docs, commit messages or output. No emojis in commits.
- Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Run `git log -5` before committing. Messages say "Part of #62".
- Gates before each commit: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`.
- Never touch `~/Library/Application Support/Overseer`, the installed app, or its LaunchAgent. Tests use temp state dirs only.
- The holder must stay small: no screen emulator, no SQLite, no network, no config reading. It gets everything from the daemon.
- `SlotManager`, courier and server callers keep using `Slot`'s existing public methods unchanged: `spawn_in`, `write`, `resize`, `kill`, `screen`, `snapshot`, `attach`, `attach_at`, `catch_up`, `info`, `wait_exit`, `run`.
- New fields on proto types are `#[serde(default)]`, so an old client and a new daemon still understand each other.
- macOS Unix socket paths are limited to 104 bytes. Holder sockets live at `<state>/holders/S<n>.sock`. Tests put state in a short temp dir (the existing `temp_socket()` pattern).

## Decisions this plan makes (spec left them open)

- **Frames:** 4-byte big-endian length (covering type plus payload), 1-byte type, payload. `MAX_FRAME = 1 << 20`. JSON payloads for `Hello`, `Exit` and `Resize`; raw bytes for `Output`, `Replay` and `Input`. `HOLDER_PROTOCOL: u32 = 1`. The daemon supports `SUPPORTED: &[u32] = &[1]`.
- **The spawn spec goes to the holder on stdin** as one JSON object, so the session environment is never written to disk.
- **The exit file holds only the code and the time**, not the ring. A reattached exited slot shows `Exited` with that code and no output. (This is a small cut from the spec, which said the ring as well.)
- **An older-build holder** shows as `Running` with ` (older build)` appended to its label, which needs no GUI change. Attaching to it is refused with "this session runs on an older Overseer build; stop it to use the slot". `Stop` ends it by pid.
- **Redraw gate:** after a reattach, `Slot::redraw_pending()` is true until output has arrived after the resize nudge and then `REDRAW_QUIET = 1s` has passed with no output, or `REDRAW_GIVE_UP = 10s` after the nudge, whichever comes first.
- **Holder start:** the daemon waits up to `HOLDER_START = 5s` for the holder's socket to accept.
- **Tests run a real holder.** `overseer-daemon` gets a bin target `overseer-test-holder` (`src/bin/test_holder.rs`, calls `overseer_daemon::holder::main()`). Integration tests pass `env!("CARGO_BIN_EXE_overseer-test-holder")` as `DaemonConfig::holder_program`. With `holder_program: None` the daemon uses `LocalPty` in-process, as today, which keeps the unit tests in `slot.rs` and `manager.rs` working unchanged.

## Review Focus

1. **A holder whose claude exits between the daemon's shutdown and its restart.** The next daemon shows `Exited` with the right code, and the slot can be started again. (Test in Task 6.)
2. **Two daemons racing at start**, for example launchd's restart overlapping a manual one. A holder serves one connection, and a new one replaces the old. The losing daemon's slot must fail cleanly, not leave two writers. (Test in Task 4: a second connect makes the first see EOF.)
3. **Stale socket or pid files after a crash or a reboot**, where the pid may now belong to an unrelated process. Never signal a pid whose process is not `overseer hold` or `overseer-test-holder`. (Test in Task 8.)
4. **Output burst while the daemon is down**, larger than the ring. The reattached screen still converges after the redraw, and no relay is typed before that. (Test in Task 6.)
5. **The hook spool growing without bound** when the daemon stays down for hours. Replay caps at `SPOOL_MAX_LINES = 10_000` (the newest win), and the file is emptied after replay. (Test in Task 7.)

---

### Task 1: Redraw checkpoint

The spec makes this the first task: does a resize one column narrower and back make the installed Claude Code redraw its whole screen? The answer decides whether Task 6 uses the nudge or a holder-side screen.

**Files:**
- Create: `crates/overseer-daemon/examples/redraw_check.rs`

- [ ] **Step 1: Write the example.** It:
  - starts `claude` with `Slot::spawn` in a folder given as the first argument (one already trusted), at 120x40;
  - waits until `relay::screen::input_box` finds the prompt box (timeout 30s);
  - records the screen text (`slot.screen().0`, rows as strings);
  - builds a fresh `vt::TermBridge` at the same size and feeds it only the last 2 KiB of `slot.snapshot()`, which simulates a ring that lost the start;
  - resizes the slot to 119x40 and then back to 120x40 through `Slot::resize`, feeding everything that arrives afterwards into the fresh bridge for 3s;
  - prints `PASS` if every row of the fresh bridge's screen equals the recorded one, otherwise `FAIL` and both screens;
  - kills the slot.

  Expose whatever it needs from `vt` and `relay::screen` as `pub`. Nothing else changes.

- [ ] **Step 2: Run it**

Run: `cargo run -p overseer-daemon --example redraw_check -- /Users/jonathan/overseer/.claude/worktrees/share-ready`
Expected: `PASS`. Record the result and the Claude Code version (`claude --version`) in the commit message. On `FAIL`, stop and report: Task 6 then needs the fallback (the holder keeps a screen and sends a repaint on connect), and the plan has to be amended before going on.

- [ ] **Step 3: Commit**

```bash
git add crates/overseer-daemon/examples/redraw_check.rs crates/overseer-daemon/src
git commit -m "Check that a resize makes Claude Code redraw the whole screen"
```

### Task 2: Move the terminal into `LocalPty`

A pure refactor with no behaviour change.

**Files:**
- Create: `crates/overseer-daemon/src/pty.rs`
- Modify: `crates/overseer-daemon/src/slot.rs`, `crates/overseer-daemon/src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct SpawnSpec { pub slot: SlotId, pub label: String, pub cwd: PathBuf,
      pub argv: Vec<String>, pub env: Vec<(String, String)>, pub cols: u16,
      pub rows: u16, pub run: String }          // Serialize + Deserialize
  pub struct LocalPty { .. }
  impl LocalPty {
      pub fn spawn(spec: &SpawnSpec, on_output: Box<dyn FnMut(&[u8]) + Send>) -> anyhow::Result<LocalPty>;
      pub fn pid(&self) -> u32;
      pub fn write(&self, data: &[u8]) -> io::Result<()>;
      pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()>;
      pub fn kill(&self);
      pub fn exit(&self) -> watch::Receiver<Option<Option<i32>>>;  // same ExitValue as today
  }
  pub fn session_env(env: &[(OsString, OsString)], slot: SlotId, run: &str) -> Vec<(String, String)>;
  ```
  `session_env` is today's environment logic from `spawn_in`: `env::for_session`, then `OVERSEER_SLOT`, `OVERSEER_RUN`, `TERM=xterm-256color` and `COLORTERM=truecolor`. `next_run()` stays in `slot.rs`.
- `on_output` runs on the reader thread for every chunk. It returns once the stream ends, and the reader thread drops it then. `Slot` uses that drop to close its broadcast sender, as today.

- [ ] **Step 1: Move the open, spawn, reader-thread and wait-thread code** from `Slot::spawn_in` into `LocalPty::spawn`. `Slot` keeps the ring, screen, broadcast and lock ordering, and builds its `on_output` closure from them. `Slot` holds a `LocalPty` where it held `master`, `writer` and `killer`.
- [ ] **Step 2: Run the gates.** All existing tests pass unchanged, notably `crates/overseer-daemon/tests/slot.rs` and the manager unit tests.
- [ ] **Step 3: Commit** with the message "Move the session terminal into LocalPty".

### Task 3: Holder frames

**Files:**
- Create: `crates/overseer-daemon/src/holder/mod.rs`, `crates/overseer-daemon/src/holder/frame.rs`

**Interfaces:**
- Produces:
  ```rust
  pub const HOLDER_PROTOCOL: u32 = 1;
  pub const SUPPORTED: &[u32] = &[1];
  pub const MAX_FRAME: usize = 1 << 20;
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct HolderHello { pub version: u32, pub slot: SlotId, pub pid: u32, pub child_pid: u32,
      pub run: String, pub label: String, pub cwd: PathBuf, pub argv: Vec<String>,
      pub cols: u16, pub rows: u16, pub since: i64 }
  #[derive(Debug, PartialEq)]
  pub enum Frame {
      Hello(HolderHello),           // 1, holder to daemon
      DaemonHello { version: u32 }, // 2
      Replay(Vec<u8>),              // 3
      Output(Vec<u8>),              // 4
      Input(Vec<u8>),               // 5
      Resize { cols: u16, rows: u16 }, // 6
      Kill,                         // 7
      Exit { code: Option<i32>, at: i64 }, // 8
      ExitSeen,                     // 9
      Unknown(u8),                  // any other type; payload skipped
  }
  pub fn write_frame(w: &mut impl Write, f: &Frame) -> io::Result<()>;
  pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>>; // Ok(None) on clean EOF
  ```
  Blocking std I/O. Both sides use dedicated threads, as the reader thread does today.

- [ ] **Step 1: Write the failing tests** in `frame.rs`:
  - `every_frame_round_trips`: each variant except `Unknown` is written, read back and compared with `assert_eq!`;
  - `an_unknown_type_is_skipped`: write a raw frame of type 200 with 3 payload bytes, then an `Output(b"x")`; reading gives `Unknown(200)` and then `Output(b"x")`;
  - `an_oversized_frame_is_an_error`: a header claiming `MAX_FRAME + 1` gives `Err` with kind `InvalidData`;
  - `eof_between_frames_is_none`: an empty reader gives `Ok(None)`, and a truncated header gives `Err(UnexpectedEof)`.
- [ ] **Step 2: Run them.** Run: `cargo test -p overseer-daemon --lib holder::frame`. Expected: they fail to compile, because the types are missing.
- [ ] **Step 3: Implement** `write_frame` and `read_frame`.
- [ ] **Step 4: Run them.** Same command; expected PASS.
- [ ] **Step 5: Commit** with the message "Add the holder frame format".

### Task 4: The holder process

**Files:**
- Create: `crates/overseer-daemon/src/holder/serve.rs`, `crates/overseer-daemon/src/bin/test_holder.rs`, `crates/overseer-daemon/tests/holder.rs`
- Modify: `crates/overseer-daemon/Cargo.toml` (bin target `overseer-test-holder`), `crates/overseer/src/main.rs` (hidden `Hold` subcommand)

**Interfaces:**
- Consumes: `LocalPty`, `SpawnSpec` (Task 2); frames (Task 3).
- Produces:
  ```rust
  pub fn holders_dir(state_dir: &Path) -> PathBuf;          // <state>/holders
  pub fn socket_path(state_dir: &Path, slot: SlotId) -> PathBuf;   // holders/S<n>.sock
  pub fn pid_path(state_dir: &Path, slot: SlotId) -> PathBuf;      // holders/S<n>.pid
  pub fn exit_path(state_dir: &Path, slot: SlotId) -> PathBuf;     // holders/S<n>.exit
  pub fn activity_path(state_dir: &Path, slot: SlotId) -> PathBuf; // holders/S<n>.activity
  #[derive(Serialize, Deserialize)] pub struct PidFile { pub holder: u32, pub child: u32, pub version: u32 }
  #[derive(Serialize, Deserialize)] pub struct ExitFile { pub code: Option<i32>, pub at: i64 }
  /// Entry point for `overseer hold <state_dir>` and the test binary.
  /// Reads one SpawnSpec as JSON from stdin, then serves until the child has exited
  /// and its exit has been seen (ExitSeen) or written to the exit file.
  pub fn main() -> !;
  ```
  The `overseer` CLI's hidden `hold` subcommand takes the state dir as its one argument and calls `overseer_daemon::holder::main()`. So does the test binary.

Behaviour, in order: `setsid()`; create `holders/` with mode 0700; read the spec from stdin; remove any stale socket; bind the socket; spawn `LocalPty`; write the pid file; accept connections in a loop.
- **On connect:** send `Hello`, then `Replay(ring contents)`, then stream `Output`. Ring and broadcast are taken under one lock, as `Slot::attach_at` does, so nothing is lost or duplicated.
- **A new connection replaces the current one.** The old connection is shut down.
- **Daemon to holder:** `Input` goes to `write`, `Resize` to `resize`, `Kill` to `kill`.
- **On child exit:** if connected, send `Exit` and wait up to 5s for `ExitSeen`. Without `ExitSeen`, or with no connection, write the exit file. Either way, remove the socket and pid file, then exit 0.
- The ring has capacity `RING_CAP`, the same constant `Slot` uses. Reuse `ring::RingBuffer`.

- [ ] **Step 1: Write the failing integration tests** in `tests/holder.rs`. They start `env!("CARGO_BIN_EXE_overseer-test-holder")` with a temp state dir and a spec running `/bin/sh -c` scripts:
  - `hello_then_replay_then_output`: the spec runs `printf before; sleep 1; printf after; sleep 30`. Connect after 0.5s. The first frame is `Hello` with `version == 1` and the right `slot` and `run`. The next frame is `Replay` containing `before`. A later `Output` contains `after`.
  - `input_reaches_the_child`: the spec runs `read l; echo got:$l; sleep 30`. Send `Input(b"hi\n")`, and some `Output` contains `got:hi`.
  - `output_while_disconnected_is_replayed`: connect, disconnect, the child prints `later`, reconnect. The `Replay` contains `later`.
  - `a_second_connection_replaces_the_first`: after a second connect, the first stream reads `Ok(None)` within 1s.
  - `exit_with_no_daemon_writes_the_exit_file`: the spec runs `exit 3` after 0.5s with nothing connected. Within 3s the exit file has `code == Some(3)`, and the socket and pid file are gone.
  - `exit_when_connected_is_sent`: connected, the child exits 0. `Exit { code: Some(0), .. }` arrives. After sending `ExitSeen`, the holder process exits and no exit file exists.
  - `kill_ends_the_child`: send `Kill`, and `Exit` arrives with `code == None`.
  - `the_holder_is_its_own_session_leader`: `getsid(holder_pid) == holder_pid`.
  - `the_pid_file_names_holder_and_child`: `PidFile.holder` is the holder's pid, `child` the shell's pid, and `version == 1`.
- [ ] **Step 2: Run them.** Run: `cargo test -p overseer-daemon --test holder`. Expected: FAIL (the binary does not exist yet).
- [ ] **Step 3: Implement** `holder::serve` and the two entry points.
- [ ] **Step 4: Run them.** Same command; expected PASS.
- [ ] **Step 5: Commit** with the message "Add the holder process that keeps a session's terminal".

### Task 5: `Slot` over a holder

**Files:**
- Create: `crates/overseer-daemon/src/holder/client.rs`
- Modify: `crates/overseer-daemon/src/slot.rs`, `crates/overseer-daemon/src/manager.rs`, `crates/overseer-daemon/src/server.rs` (`DaemonConfig`), `crates/overseer/src/main.rs`, `crates/overseer-proto/src/msg.rs`, `crates/overseer-daemon/tests/server.rs` (harness)

**Interfaces:**
- Consumes: Tasks 2 to 4.
- Produces:
  - `DaemonConfig.holder_program: Option<PathBuf>`. `main.rs` sets `Some(std::env::current_exe()?)`. Both binaries are invoked the same way, `<program> hold <state_dir>`, and the test binary skips a leading `hold` argument.
  - `SlotInfo.held: bool` (`#[serde(default)]`), true when a holder backs the slot.
  - `Slot::spawn_held(spec: SpawnSpec, program: &Path, state_dir: &Path, out: broadcast::Sender<Bytes>) -> anyhow::Result<Slot>`: starts the holder (stdin piped, the spec written, stdin closed, the child reaped on a background thread), waits up to `HOLDER_START` for the socket, connects, and calls `attach_held`.
  - `Slot::attach_held(socket: &Path, out: broadcast::Sender<Bytes>) -> anyhow::Result<Attached>` where `enum Attached { Slot(Slot), Unsupported(HolderHello) }`: connects, sends `DaemonHello { version: HOLDER_PROTOCOL }`, reads `Hello`, feeds `Replay` and then `Output` into the ring, screen and broadcast exactly as the local reader does. `Exit` sets the exit value and is answered with `ExitSeen`. EOF without `Exit` sets the exit value to `Some(None)`.
  - `Slot::is_held(&self) -> bool`.
  - Inside `Slot`, the `LocalPty` field becomes `enum Backend { Local(LocalPty), Held(HeldPty) }`, and `write`, `resize` and `kill` go through it. `HeldPty` is in `holder/client.rs`.
  - `SlotManager::with_holder(program: PathBuf) -> Self`. When set, `start` uses `spawn_held` (needs `launch` for the state dir). Without it, `start` uses `spawn_in` as today.

- [ ] **Step 1: Point the integration harness at the holder.** `start_daemon_full` in `tests/server.rs` sets `holder_program: Some(env!("CARGO_BIN_EXE_overseer-test-holder").into())`, so the whole existing suite runs over holders. Add `held_slot_reports_held`: start a slot, and `list()` shows `held == true` and `state == Running`.
- [ ] **Step 2: Run it.** Run: `cargo test -p overseer-daemon --test server held_slot_reports_held`. Expected: FAIL to compile (no `held` field).
- [ ] **Step 3: Implement** the interfaces above.
- [ ] **Step 4: Run the gates.** The whole existing `tests/server.rs` suite passes over holders, along with the new test. A test that fails only because of timing over the extra hop gets a longer wait, never a skip, and the commit message says which.
- [ ] **Step 5: Commit** with the message "Run sessions through a holder process".

### Task 6: Survive a restart

**Files:**
- Modify: `crates/overseer-daemon/src/manager.rs` (`restore`, `shutdown_all`, `start`, `stop`, `attach`), `crates/overseer-daemon/src/server.rs` (startup order), `crates/overseer-daemon/src/slot.rs` (redraw gate), `crates/overseer-daemon/src/relay/courier.rs`
- Test: `crates/overseer-daemon/tests/server.rs`

**Interfaces:**
- Consumes: Task 5.
- Produces:
  - `SlotManager::restore()` first scans `holders/`:
    - live socket: `attach_held`, then `Entry` with `session_id`, `command` and `session_note` from the ledger row for that slot (from the hello if there is none), and `activity` from the activity file;
    - exit file: `ledger.record_exit(slot, code, at)`, then `Restored` as `Exited`, then delete the file;
    - dead socket: delete it.

    After that comes today's ledger path, for slots still unfilled.
  - Each reattached slot gets a `monitor_exit` and, if claude, a `watch_trust`, as `start` does.
  - `shutdown_all()`: held slots get their activity written to `activity_path` (JSON `Activity`), and their connection closed. No `Kill`. Local slots are killed as today. Ledger rows of held slots stay live.
  - `Slot::nudge_redraw(&self)`: resize to `cols - 1` and back, and start the gate. `Slot::redraw_pending(&self) -> bool` follows the rule in "Decisions". `restore` calls `nudge_redraw` on every reattached slot.
  - `courier::ready` takes the slot's `redraw_pending` and returns `Err("screen is being redrawn")` while it is true. Add it as a named constant beside `NO_BOX` and `WORKING`.
  - `server::run`: `restore()` (now reattaching) runs before `locks.reconcile_exits`, as it does today, so reattached sessions keep their waits and relays.

- [ ] **Step 1: Write the failing tests** in `tests/server.rs`. Each one starts a daemon on a state dir, shuts it down with the existing shutdown request, and starts a second daemon on the same state dir and socket:
  - `a_held_session_survives_a_restart`: `fake-claude.sh` in S1. After the restart S1 is `Running` with the same pid, attaching replays the old output, and input typed after the restart is echoed back.
  - `output_while_down_is_shown_after_restart`: the fake prints a marker 1s after the first daemon stops. The second daemon's `attach` snapshot contains it.
  - `exit_while_down_shows_the_code`: the fake exits 7 while no daemon runs. The second daemon lists S1 as `Exited { code: Some(7), .. }`, and a new start in S1 works.
  - `killed_holder_falls_back_to_resumable`: kill the holder pid from its pid file while down. S1 comes back `Resumable`.
  - `activity_survives_a_restart`: send an Idle hook to S1, then restart. S1's `activity.kind == Idle`, and a queued relay to S1 is typed once the redraw gate opens.
  - `no_relay_before_the_redraw`: after a restart, a relay queued for S1 is not typed during the first 500 ms, even though the activity is Idle. (The fake keeps printing every 100 ms for 1s after any resize, so the gate stays shut.)
  - `a_burst_bigger_than_the_ring_still_converges`: while the daemon is down, the fake writes `RING_CAP * 2` bytes and then redraws on SIGWINCH. After the restart and the gate, `screen()` matches what the fake last drew.
- [ ] **Step 2: Run them.** Run: `cargo test -p overseer-daemon --test server restart`. Expected: FAIL.
- [ ] **Step 3: Extend `tests/fake-claude.sh` only as far as these tests need.** Add a SIGWINCH handler that redraws a known screen, and a mode that prints a marker after a delay. Keep its existing behaviour.
- [ ] **Step 4: Implement** the interfaces above.
- [ ] **Step 5: Run the gates.** Expected: all pass.
- [ ] **Step 6: Commit** with the message "Reattach held sessions when the daemon starts".

### Task 7: Spool hook events while the daemon is down

**Files:**
- Modify: `crates/overseer/src/hook.rs`, `crates/overseer-daemon/src/server.rs`
- Create: `crates/overseer-proto/src/spool.rs` (shared by the hook and the daemon)

**Interfaces:**
- Produces:
  ```rust
  pub const SPOOL_FILE: &str = "hook-spool.jsonl";      // in the state dir
  pub const SPOOL_MAX_LINES: usize = 10_000;
  pub fn append(state_dir: &Path, ev: &HookEvent) -> io::Result<()>;   // O_APPEND, one write per line
  pub fn take(state_dir: &Path) -> io::Result<Vec<HookEvent>>; // renames the file aside, reads it, deletes it; keeps the newest SPOOL_MAX_LINES; skips bad lines
  ```
  - `overseer hook` calls `append` only when connecting to the socket fails. A timeout after connecting does not spool. `PreToolUse` behaviour is unchanged: it fails open.
  - `server::run`, right after `restore()`, calls `take` and passes each event through the same function the `Request::Hook` arm uses (`server.rs` near line 1339: `manager.hook` plus `locks.on_hook`). Factor that arm into one function if it isn't one already. `manager.hook` already drops events whose `run` doesn't match, which covers "drop the rest".

- [ ] **Step 1: Write the failing tests:**
  - unit tests in `spool.rs`: `append_then_take_returns_events_in_order`, `take_empties_the_spool`, `take_keeps_the_newest_when_over_the_cap` (write `SPOOL_MAX_LINES + 5` events and get back the last `SPOOL_MAX_LINES`), `a_corrupt_line_is_skipped`;
  - integration test `a_hook_sent_while_down_is_applied_after_restart` in `tests/server.rs`: with the daemon down, append a Working event for S1's run, restart, and S1's `activity.kind == Working`;
  - hook test: with no daemon listening, running the hook writes one spool line, and the process still exits 0.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement** the interfaces above.
- [ ] **Step 4: Run the gates.** Expected: PASS.
- [ ] **Step 5: Commit** with the message "Spool hook events while the daemon is down".

### Task 8: Older-build holders and stale files

**Files:**
- Modify: `crates/overseer-daemon/src/manager.rs`, `crates/overseer-daemon/src/holder/client.rs`, `crates/overseer-daemon/src/holder/mod.rs`
- Test: `crates/overseer-daemon/tests/server.rs`, `crates/overseer-daemon/tests/holder.rs`

**Interfaces:**
- Produces:
  - `pub fn holder_alive(pid_file: &PidFile) -> bool`: true only if `holder` is alive and its executable name (`proc_pidpath` on macOS, via libc) ends in `overseer` or `overseer-test-holder`.
  - `pub fn end_by_pid(pid_file: &PidFile)`: if `holder_alive`, SIGTERM the child's process group (`-child`), then SIGTERM the holder after 2s if it is still alive.
  - The test holder accepts `OVERSEER_TEST_HOLDER_VERSION` in its environment to announce a different protocol version. Test-only; the real `overseer hold` ignores it.
  - In `restore`, `Attached::Unsupported(hello)` becomes an `Entry` whose slot is a stub: `Running` with `pid = hello.child_pid`, label `"<label> (older build)"`, `held == true`. `write` and `resize` on it are no-ops, `attach` is refused with the message from "Decisions", and `stop` calls `end_by_pid` and then makes the slot `Resumable` from the ledger.
  - A `Hello` that cannot be parsed at all is treated the same way, using the pid file.

- [ ] **Step 1: Write the failing tests:**
  - `an_older_build_session_is_kept_and_can_be_stopped`: start S2 with `OVERSEER_TEST_HOLDER_VERSION=99`, then restart. S2 is `Running` with a label ending ` (older build)`. `StartSlot` on S2 is refused. `attach` is refused with the exact message. `StopSlot` ends the child (its pid is gone within 3s) and S2 becomes `Resumable`.
  - `a_stale_pid_file_never_signals_another_process`: write a pid file naming a running `/bin/sleep 30` and a dead socket, then restart. The sleep is still alive afterwards, and the files are removed.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement** the interfaces above.
- [ ] **Step 4: Run the gates.** Expected: PASS.
- [ ] **Step 5: Commit** with the message "Keep and end sessions held by an older build".

### Task 9: Install, restart and uninstall

**Files:**
- Modify: `crates/overseer/src/agent.rs`, `crates/overseer/src/ctl.rs`, `crates/overseer-proto/src/msg.rs`, `crates/overseer-daemon/src/server.rs`, `crates/overseer-daemon/src/manager.rs`, `scripts/uninstall.sh`, `scripts/install.sh` (message only), `README.md` (upgrade section)

**Interfaces:**
- Produces:
  - The spec's pin on `bundle.sh`: a unit test in `crates/overseer/src/agent.rs` (or wherever the install tests live) reads `scripts/bundle.sh` with `include_str!` and asserts that the `rm -f "$app/Contents/MacOS/overseer"` line comes before the `cp "$bin"` line, so an install never rewrites a running holder's binary.
  - `RunningDaemon.running: Vec<(SlotId, String, bool)>`, where the bool is `held`. `may_end_sessions()` is `!listed || running.iter().any(|(_, _, held)| !held)`. A daemon that does not report `held` (older build) reads as `false`, so the first upgrade still asks.
  - When it restarts a daemon with running sessions, the install output says one of two things. If all of them are held: "Running sessions are kept: they reattach to the new daemon." Otherwise: "Sessions started before this version end this one last time; each comes back as resumable."
  - `Request::StopAll` answered with `Response::Ok`, and `ctl stop-all`. `SlotManager::end_all()` sets `shutting_down`, sends `Kill` to every held slot and kills the local ones, so the ledger keeps them live (resumable). Locks and relays are kept.
  - `scripts/uninstall.sh`: before unloading the agent, `overseer ctl stop-all` if a daemon answers. Otherwise it reads every `holders/*.pid` and calls `overseer hold --end <pid-file>`, a hidden flag that runs `end_by_pid`. Then it removes `holders/`.
  - README: an "Upgrading" section saying `git pull && make install` keeps running sessions, with the one-time caveat.

- [ ] **Step 1: Write the failing tests:**
  - in `agent.rs`, `restart_choice` tests: all held means a restart with no prompt; one unheld asks as today; unlisted asks as today;
  - in `tests/server.rs`, `stop_all_ends_sessions_but_keeps_them_resumable`: two slots. After `StopAll` both child pids are gone, and after a restart both are `Resumable`.
- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement** the interfaces above.
- [ ] **Step 4: Run the gates.** Expected: PASS.
- [ ] **Step 5: Commit** with the message "Restart without ending held sessions, and end them on uninstall".

### Task 10: The launchd check (manual, the user runs it)

Tests cannot run launchd, so this is a checklist for the user after `YES=1 sh scripts/install.sh`. Write it into the PR body, not into code.

1. Start a session in a slot and give it a long task.
2. Run `launchctl kickstart -k gui/$(id -u)/uk.co.techize.overseer`.
3. The claude process is still there (`pgrep -fl claude`), and its card shows the same screen and keeps working.
4. `git pull && make install YES=1` with a session mid-turn: same result, with no prompt about ending sessions.
