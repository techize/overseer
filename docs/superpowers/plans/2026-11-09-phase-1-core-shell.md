# Overseer Phase 1: Core Shell Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a single `overseer` Rust binary. Its daemon holds up to 7 Claude Code PTY sessions (S0 to S6). Its native GPUI window shows a tile board and a full-fidelity embedded terminal per session, and it offers `overseer attach` as a fallback. The result is usable on its own as "Warp replacement with a board".

**Architecture:**
- **Cargo workspace:**
  - `overseer-proto`: wire types, frame codec and the async client.
  - `overseer-daemon`: PTY slot manager and Unix-socket server.
  - `overseer-term`: an `alacritty_terminal` bridge plus key, mouse and paste encoding. It has no GUI deps, so it's unit-testable.
  - `overseer-gui`: GPUI views.
  - `overseer`: the binary, one executable with subcommands.
- **Daemon and clients.** The daemon is the only owner of PTYs. GUI, `attach` and `ctl` are clients over a Unix socket.
- **Terminal state mirroring.** This uses **byte replay**. The daemon keeps a per-slot ring buffer of raw output. A client that attaches gets the buffer, then the live stream, and feeds it into its own `alacritty_terminal::Term`. On attach the client sends its size, and the PTY resize makes Claude Code redraw.

**Tech Stack:** Rust 1.97 (stable), `gpui` 0.2.2 (`default-features = false`, features `font-kit` + `runtime_shaders`), `alacritty_terminal` 0.26, `portable-pty` 0.9, `tokio` 1.54, `serde`/`serde_json`, `clap` 4, `crossterm` (attach mode), `anyhow`/`thiserror`, `tracing`.

**Spec:** `docs/superpowers/specs/2026-11-09-overseer-design.md` (sections 7.1, 7.2, 7.9, 7.11 and 9 item 1)

**Later phases:** each gets its own plan, written when the previous phase lands: 2 hooks and state, 3 ledger and work items, 4 locks and relays, 5 2ndBrain writer, 6 coordinator, 7 polish.

## Global Constraints

- **Platform.** macOS only. Build must not need full Xcode or a Metal compiler: use the `gpui` feature `runtime_shaders`.
- **No admin rights at any point.** Install targets are `~/Applications/Overseer.app`, `~/.local/bin/overseer` and `~/Library/LaunchAgents/uk.co.techize.overseer.plist`.
- **State dir.** `~/Library/Application Support/Overseer/` (socket `overseer.sock`, later `overseer.db`). The env var `OVERSEER_HOME` overrides the whole state dir, and every test uses a temp `OVERSEER_HOME`.
- **Logs.** `~/Library/Logs/Overseer/`.
- **Slots.** Exactly 7: `S0` (coordinator) and `S1` to `S6`. A slot is addressed by its index `0..=6` on the wire and displayed as `S<n>`.
- **Session spawn command.** Default `claude`, overridable per slot (tests use `/bin/sh`). Spawned env includes `OVERSEER_SLOT=S<n>`.
- **Session lifetime.** Quitting or crashing the GUI never stops a session. Only an explicit stop, or the daemon exiting, ends PTYs.
- **Terminal input bytes.** Shift+Enter sends `ESC CR` (`\x1b\r`). Option acts as Meta by sending an `ESC` prefix. Bracketed paste wraps text in `\x1b[200~ … \x1b[201~` when the app has enabled mode 2004.
- **Text.** No em dashes (U+2014) in any user-visible string, log line or commit message.
- **Commits.** Commit messages are human-style, with no AI language and no emojis. Each ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Several clients attached to one slot at once** (GUI plus `overseer attach`). Both see live output, input from either reaches the PTY, and the last resize wins without crashing either client.
2. **A slot whose process exits** (Claude quits or `/exit`). The card shows Exited with the exit code and the slot can be restarted. The daemon must not spin or leak the PTY.
3. **A huge burst of output** (Claude printing a long diff). The ring buffer caps at 4 MiB and drops oldest bytes, the GUI stays responsive, and no client blocks the daemon. A slow client gets disconnected, not waited on.
4. **The daemon not running when the GUI or ctl starts.** The client auto-starts it, waits for the socket for up to 3 s, then shows a clear error.
5. **A stale socket file left after a daemon crash.** The daemon detects that nothing is listening, removes the file and binds.

Each of these is pinned by a test in the owning task: 1 to 3 and 5 in Task 4, and 4 in Task 5.

---

## File Structure

```
Cargo.toml                       workspace, shared deps, profile.release (lto, strip)
Makefile                         build, test, install, bundle
.github/workflows/ci.yml         macos runner: fmt, clippy, test
crates/overseer-proto/src/
  lib.rs                         re-exports
  paths.rs                       state dir, socket path, log dir (OVERSEER_HOME aware)
  msg.rs                         Request, Response, Event, SlotInfo, SlotState
  frame.rs                       Frame enum + async encode/decode
  client.rs                      Client: connect (+autostart hook), request, subscribe
crates/overseer-daemon/src/
  lib.rs                         run(DaemonConfig)
  ring.rs                        RingBuffer (cap 4 MiB)
  slot.rs                        Slot: PTY, reader task, writer, resize, exit watch
  manager.rs                     SlotManager: 7 slots, start/stop/input/resize/attach
  server.rs                      Unix socket accept loop, per-connection task
crates/overseer-term/src/
  lib.rs
  bridge.rs                      TermBridge: feed bytes, snapshot, resize, scroll, search
  keys.rs                        encode_key(KeyInput, TermModes) -> Vec<u8>
  mouse.rs                       encode_mouse(MouseInput, TermModes) -> Option<Vec<u8>>
  paste.rs                       encode_paste(PasteContent, TermModes) -> Vec<u8>
  links.rs                       find_link_at(snapshot, point) -> Option<Link>
crates/overseer-gui/src/
  lib.rs                         run_gui(client)
  app.rs                         root view, navigation (board | session N), key bindings
  board.rs                       BoardView + SlotCard
  session.rs                     SessionView: TerminalView + ContextPanel
  terminal_view.rs               TerminalView element (render grid, input, mouse, IME)
  start_dialog.rs                start-slot form (label, cwd, command)
  theme.rs                       colours, font config
crates/overseer/src/
  main.rs                        clap: (none)=gui, daemon, attach, ctl, install-agent
  attach.rs                      raw-mode passthrough, detach key Ctrl-]
  ctl.rs                         status/start/stop/send
  agent.rs                       LaunchAgent plist write/load
packaging/Info.plist             bundle metadata, id uk.co.techize.overseer
tests/fake-claude.sh             test session: echoes input, prints colours, exits on "quit"
```

---

### Task 1: Workspace scaffold and GPUI build spike

This task proves the risky part first: that GPUI builds and opens a window on this Mac with only Command Line Tools installed.

**Files:**
- Create: `Cargo.toml`, `crates/*/Cargo.toml`, `crates/*/src/lib.rs` (empty modules), `crates/overseer/src/main.rs`, `Makefile`, `.github/workflows/ci.yml`, `README.md`

**Interfaces:**
- Produces: workspace members `overseer-proto`, `overseer-daemon`, `overseer-term`, `overseer-gui`, `overseer`, plus the `make build|test|lint` targets.

- [ ] **Step 1: Write the workspace `Cargo.toml`.** Include `[workspace.dependencies]` pinning the versions in Tech Stack. `gpui = { version = "0.2.2", default-features = false, features = ["font-kit", "runtime_shaders"] }`. Set `[profile.release] lto = "thin", strip = true, codegen-units = 1`.
- [ ] **Step 2: Make `overseer-gui::run_gui_spike()`.** It opens one 900×600 window titled `Overseer` that renders the text "Overseer spike". `main.rs` calls it when run with no args.
- [ ] **Step 3: Build.** Run `cargo build -p overseer`. Expected: it compiles with no Metal-compiler error. If `runtime_shaders` fails to build, stop and report. The whole UI approach depends on it.
- [ ] **Step 4: Run.** Run `cargo run -p overseer`. Expected: a window titled "Overseer" appears showing the text. Close it, and the process exits 0.
- [ ] **Step 5: Add CI.** `ci.yml` runs on `macos-14` with `cargo fmt --check`, `cargo clippy --workspace -- -D warnings` and `cargo test --workspace`. The `Makefile` gets targets `build`, `test`, `lint`.
- [ ] **Step 6: Commit.** Message: "Scaffold workspace and prove GPUI window builds without Xcode".

---

### Task 2: Paths and wire protocol

**Files:**
- Create: `crates/overseer-proto/src/{paths,msg,frame}.rs`
- Test: `crates/overseer-proto/tests/frame_roundtrip.rs`, plus unit tests in `paths.rs`

**Interfaces:**
- Produces:
  - `paths::state_dir() -> PathBuf`, `paths::socket_path() -> PathBuf`, `paths::log_dir() -> PathBuf`. All honour `OVERSEER_HOME`: when it's set, `socket_path` is `$OVERSEER_HOME/overseer.sock` and `log_dir` is `$OVERSEER_HOME/logs`.
  - `type SlotId = u8` (`0..=6`). `fn slot_name(SlotId) -> String` returns `"S3"`.
  - `enum SlotState { Empty, Running { pid: u32, since: i64 }, Exited { code: Option<i32>, at: i64 } }`. Timestamps are unix seconds.
  - `struct SlotInfo { id: SlotId, label: String, cwd: Option<PathBuf>, command: Option<Vec<String>>, state: SlotState, cols: u16, rows: u16 }`
  - `enum Request`:
    - `Hello { client: String }`
    - `ListSlots`
    - `StartSlot { id, label, cwd: PathBuf, command: Option<Vec<String>> }`
    - `StopSlot { id }`
    - `Attach { id }`
    - `Detach { id }`
    - `Resize { id, cols: u16, rows: u16 }`
    - `Shutdown`
  - `enum Response { Ok, Slots(Vec<SlotInfo>), Error { message: String } }`
  - `enum Event { SlotChanged(SlotInfo) }`
  - `enum Frame`:
    - `Request { seq: u32, req: Request }`
    - `Response { seq: u32, resp: Response }`
    - `Event(Event)`
    - `Output { id: SlotId, data: Bytes }`
    - `Input { id: SlotId, data: Bytes }`
  - `async fn write_frame<W: AsyncWrite + Unpin>(w, &Frame) -> io::Result<()>` and `async fn read_frame<R: AsyncRead + Unpin>(r) -> io::Result<Option<Frame>>`. `Ok(None)` means a clean EOF.
- **Encoding.** Each frame is a `u32` big-endian length (of everything after it), then a `u8` kind, then the payload.
  - Kinds `0` (Request), `1` (Response) and `2` (Event) carry the serde_json payload of the inner value plus `seq` where it applies.
  - Kinds `3` (Output) and `4` (Input) carry a payload of a `u8` slot id followed by the raw bytes.
  - The maximum frame is 8 MiB, and `read_frame` returns `InvalidData` above it.

- [ ] **Step 1: Write the failing tests:**
  - `roundtrip_every_frame_kind`: writes one of each `Frame` variant into a `Vec<u8>`, reads them back and asserts they're equal. The Output data includes the bytes `0x00`, `0x1b` and `0xff`.
  - `eof_returns_none`: an empty reader gives `Ok(None)`.
  - `oversize_frame_rejected`: a header announcing 9 MiB gives an `InvalidData` error.
  - `overseer_home_override`: with `OVERSEER_HOME=/tmp/x`, `socket_path()` equals `/tmp/x/overseer.sock`.
- [ ] **Step 2: Run them to see them fail.** Run `cargo test -p overseer-proto`. Expected: compile errors or FAIL.
- [ ] **Step 3: Implement** `paths.rs`, `msg.rs` (serde derive, `#[serde(tag = "type")]`) and `frame.rs`.
- [ ] **Step 4: Run them to see them pass.** Run `cargo test -p overseer-proto`. Expected: 4 passed.
- [ ] **Step 5: Commit.** Message: "Add wire protocol and state paths".

---

### Task 3: Ring buffer and PTY slot

**Files:**
- Create: `crates/overseer-daemon/src/{ring,slot}.rs`, `tests/fake-claude.sh`
- Test: unit tests in `ring.rs`, `crates/overseer-daemon/tests/slot.rs`

**Interfaces:**
- Consumes: `SlotId`, `SlotState`, `slot_name` (Task 2).
- Produces:
  - `RingBuffer::new(cap: usize)`, `push(&mut self, &[u8])` and `contents(&self) -> Vec<u8>`. When full, it drops the oldest bytes. The daemon uses `cap = 4 * 1024 * 1024`.
  - `Slot::spawn(id: SlotId, label: String, cwd: &Path, command: &[String], cols: u16, rows: u16, out: broadcast::Sender<Bytes>) -> anyhow::Result<Slot>`
  - `Slot::write(&self, &[u8]) -> io::Result<()>`
  - `Slot::resize(&self, cols, rows) -> anyhow::Result<()>`
  - `Slot::kill(&self)`
  - `Slot::snapshot(&self) -> Vec<u8>`: the ring contents.
  - `Slot::info(&self) -> SlotInfo`
  - `Slot::wait_exit(&self) -> impl Future<Output = Option<i32>>`
- **Implementation notes.** The PTY comes from `portable_pty::native_pty_system()`. The env is the parent env plus `OVERSEER_SLOT`, with `TERM=xterm-256color` and `COLORTERM=truecolor`. A blocking reader thread pushes into the ring (behind a `Mutex`) and onto the broadcast channel, which has capacity 1024.
- **`fake-claude.sh`** prints `READY`, then a line with 256-colour and truecolor escapes, then echoes each input line as `ECHO:<line>`. On `quit` it exits 3. On `flood` it prints 6 MiB of text.

- [ ] **Step 1: Write the failing tests:**
  - `ring_keeps_newest_when_full`: cap 8, push `b"abcdef"` then `b"ghij"`, and contents equal `b"cdefghij"`.
  - `slot_echoes_input`: spawn `fake-claude.sh` and wait for `READY` on the broadcast. `write(b"hi\r")` should produce `ECHO:hi` within 2 s.
  - `slot_reports_exit_code`: write `quit\r`, and `wait_exit` gives `Some(3)` within 2 s.
  - `slot_sets_env`: spawn `/bin/sh -c 'echo $OVERSEER_SLOT'` with id 4, and the output contains `S4`.
  - `ring_caps_flood`: write `flood\r` and wait for exit. `snapshot().len()` is at most 4 MiB, and the tail of the snapshot equals the last bytes the script printed.
- [ ] **Step 2: Run them to see them fail.** Run `cargo test -p overseer-daemon`. Expected: FAIL.
- [ ] **Step 3: Implement** `ring.rs` and `slot.rs` as specified above.
- [ ] **Step 4: Run them to see them pass.** Run `cargo test -p overseer-daemon`. Expected: 5 passed.
- [ ] **Step 5: Commit.** Message: "Add PTY slots with output ring buffer".

---

### Task 4: Slot manager and socket server

**Files:**
- Create: `crates/overseer-daemon/src/{manager,server,lib}.rs`
- Test: `crates/overseer-daemon/tests/server.rs`

**Interfaces:**
- Consumes: `Slot` (Task 3), `Frame`/`Request`/`Response`/`Event` and `read_frame`/`write_frame` (Task 2).
- Produces:
  - `pub struct DaemonConfig { pub socket: PathBuf, pub default_command: Vec<String> }`. The default command is `["claude"]`.
  - `pub async fn run(cfg: DaemonConfig) -> anyhow::Result<()>`
  - `SlotManager` holds 7 `Option<Slot>`s plus labels, and a `broadcast::Sender<Event>` for `SlotChanged`.
- **Request handling:**
  - `StartSlot` on a running slot gives `Error { "S3 is already running" }`.
  - `Attach` replies `Ok`, then sends one `Output` frame with the snapshot, then streams live `Output`.
  - `Input` frames write to the PTY.
  - `Resize` resizes the PTY. The last resize wins.
  - `Detach` stops that slot's stream for this connection.
  - `StopSlot` kills the process. Its state becomes `Exited` once it has exited.
  - `Shutdown` kills all slots, removes the socket and returns from `run`.
- **Slow clients.** Each connection has a bounded outbound queue (`mpsc` of 256 frames). If the queue is full, the connection is dropped and logged as `client too slow, disconnected`. The daemon never awaits a slow client.
- **Stale socket.** At startup, if the socket file exists and `UnixStream::connect` fails, delete the file and bind. If the connect succeeds, return the error "daemon already running".

- [ ] **Step 1: Write the failing tests.** Each test starts `run` on a temp socket with `default_command = [fake-claude.sh]`.
  - `start_list_stop`: StartSlot 1 gives Ok. ListSlots shows S1 Running. StopSlot 1 is followed by `SlotChanged` with state Exited.
  - `double_start_refused`: a second StartSlot 1 gives an Error message containing `S1 is already running`.
  - `attach_replays_then_streams`: start, wait 300 ms, then Attach. The first Output contains `READY`. Send Input `hi\r`, and an Output containing `ECHO:hi` arrives.
  - `two_clients_same_slot`: two connections attach to S2. Input from client A produces `ECHO` seen by both. Resize from B, then from A, is reflected in ListSlots `cols`/`rows` as A's values.
  - `slow_client_dropped`: a client attaches, sends `flood\r` and never reads. A second client's ListSlots still answers within 1 s, and the first connection is closed.
  - `stale_socket_removed`: create a regular file at the socket path, and `run` still binds and answers ListSlots.
  - `exit_then_restart`: send `quit\r` and get Exited with `code: Some(3)`. StartSlot on the same id gives Ok, then Running.
- [ ] **Step 2: Run them to see them fail.** Run `cargo test -p overseer-daemon --test server`. Expected: FAIL.
- [ ] **Step 3: Implement** `manager.rs`, `server.rs` and `run`.
- [ ] **Step 4: Run them to see them pass.** Run `cargo test -p overseer-daemon`. Expected: all pass.
- [ ] **Step 5: Commit.** Message: "Add daemon socket server and slot manager".

---

### Task 5: Client library, CLI, daemon autostart, LaunchAgent

**Files:**
- Create: `crates/overseer-proto/src/client.rs`, `crates/overseer/src/{main,ctl,agent}.rs`
- Test: `crates/overseer/tests/cli.rs` (uses `assert_cmd`)

**Interfaces:**
- Consumes: protocol (Task 2), `overseer_daemon::run` (Task 4).
- Produces:
  - `Client::connect(path) -> io::Result<Client>`
  - `Client::connect_or_start(path, start: impl FnOnce() -> io::Result<()>) -> anyhow::Result<Client>`. It retries the connect every 100 ms for up to 3 s, then fails with `could not reach the Overseer daemon at <path>`.
  - `Client::request(&self, Request) -> anyhow::Result<Response>`
  - `Client::subscribe(&self) -> mpsc::Receiver<Frame>`: Events and Output.
  - `Client::send_input(&self, SlotId, &[u8])`
- **Clap commands:**
  - `overseer` (GUI)
  - `overseer daemon [--foreground]`
  - `overseer ctl status`: prints one line per slot, like `S1  running  pid 123  ~/Repos/eks  karpenter`.
  - `overseer ctl start <n> --cwd <dir> [--label <l>] [-- <cmd>...]`
  - `overseer ctl stop <n>`
  - `overseer ctl send <n> <text>`: sends the text plus `\r`.
  - `overseer ctl shutdown`
  - `overseer install-agent`
  - `overseer uninstall-agent`
  - `overseer attach <n>` (Task 6)
- **Autostart.** It spawns `current_exe() daemon` detached (`setsid`), with stdout and stderr going to `log_dir()/daemon.log`.
- **`install-agent`.** It writes `~/Library/LaunchAgents/uk.co.techize.overseer.plist` with `ProgramArguments [<exe>, daemon, --foreground]`, `RunAtLoad true`, `KeepAlive true` and log paths in `log_dir()`, then runs `launchctl bootstrap gui/$UID <plist>`. `uninstall-agent` runs `bootout` and removes the plist.

- [ ] **Step 1: Write the failing tests** (temp `OVERSEER_HOME`, and `OVERSEER_DEFAULT_COMMAND` pointing to `fake-claude.sh`):
  - `ctl_status_autostarts_daemon`: with no daemon running, `overseer ctl status` exits 0 and prints 7 lines, S0 to S6, all `empty`.
  - `ctl_start_send_status`: `ctl start 2 --cwd /tmp --label demo`, then `ctl send 2 hello`, then `ctl status`, which shows `S2  running`.
  - `ctl_unreachable_message`: with `OVERSEER_HOME` pointing at a read-only dir, so the daemon can't bind, the command exits non-zero within 4 s and stderr contains `could not reach the Overseer daemon`.
  - `agent_plist_contents`: `agent::plist_xml(exe, log_dir)` contains `<string>daemon</string>`, `<string>--foreground</string>` and `<key>KeepAlive</key><true/>`. This is a unit test that doesn't call launchctl.
- [ ] **Step 2: Run them to see them fail.** Run `cargo test -p overseer --test cli`. Expected: FAIL.
- [ ] **Step 3: Implement** the client, `main.rs`, `ctl.rs` and `agent.rs`. The daemon reads `OVERSEER_DEFAULT_COMMAND` (space-split) to override `["claude"]`.
- [ ] **Step 4: Run them to see them pass.** Run `cargo test -p overseer`. Expected: all pass.
- [ ] **Step 5: Commit.** Message: "Add client, ctl commands, daemon autostart and LaunchAgent".

---

### Task 6: `overseer attach` fallback

**Files:**
- Create: `crates/overseer/src/attach.rs`
- Test: unit tests in `attach.rs`

**Interfaces:**
- Consumes: `Client` (Task 5).
- Produces:
  - `attach::run(slot: SlotId) -> anyhow::Result<()>`
  - `fn split_detach(input: &[u8]) -> (Vec<u8>, bool)`: returns the bytes before Ctrl-] (`0x1d`) and whether the detach key was seen.
- **Behaviour.**
  - Uses crossterm raw mode and the alternate screen off.
  - Sends `Resize` with the terminal size on start and on every resize event.
  - Writes Output bytes to stdout and forwards stdin bytes as Input.
  - Ctrl-] detaches, restores the terminal and prints `Detached from S3. The session is still running.`
  - Attaching to an empty slot prints `S3 is empty. Start it with: overseer ctl start 3 --cwd <dir>` and exits 1.

- [ ] **Step 1: Write the failing tests:**
  - `split_detach_none`: `b"abc"` gives `(b"abc", false)`.
  - `split_detach_mid`: `b"ab\x1dcd"` gives `(b"ab", true)`.
- [ ] **Step 2: Run them to see them fail.** Run `cargo test -p overseer attach`. Expected: FAIL.
- [ ] **Step 3: Implement** `split_detach` and `attach::run`.
- [ ] **Step 4: Run them to see them pass.** Run `cargo test -p overseer attach`. Expected: PASS.
- [ ] **Step 5: Manual check in iTerm2.** Run `overseer ctl start 1 --cwd ~/overseer -- claude`, then `overseer attach 1`. Expected: Claude Code's TUI renders, typing works, Ctrl-] detaches with the message, and `overseer ctl status` still shows S1 running.
- [ ] **Step 6: Commit.** Message: "Add attach fallback with Ctrl-] detach".

---

### Task 7: Terminal bridge and input encoding (`overseer-term`)

**Files:**
- Create: `crates/overseer-term/src/{bridge,keys,mouse,paste,links}.rs`
- Test: unit tests in each file

**Interfaces:**
- Produces:
  - `TermBridge::new(cols, rows, scrollback_lines: usize)`: the default is 10_000 scrollback lines.
  - `feed(&mut self, &[u8])`
  - `resize(&mut self, cols, rows)`
  - `snapshot(&self) -> Snapshot`
  - `modes(&self) -> TermModes`
  - `scroll(&mut self, delta_lines: i32)`
  - `search(&self, query: &str) -> Vec<Match>`
  - `selection_text(&self, sel: Selection) -> String`
  - `take_pty_writes(&mut self) -> Vec<u8>`: replies the terminal must send back, such as device-attribute answers.
  - `Snapshot { cols, rows, cells: Vec<Cell>, cursor: Cursor, display_offset: usize }`
  - `Cell { ch: char, zerowidth: Option<Vec<char>>, fg: Rgb, bg: Rgb, bold, italic, underline, inverse, wide: bool }`. Colours are resolved through a `Palette` with xterm defaults that can be overridden.
  - `TermModes { app_cursor: bool, bracketed_paste: bool, mouse: MouseMode, sgr_mouse: bool, alt_screen: bool }`
  - `encode_key(&KeyInput, &TermModes) -> Vec<u8>`, where `KeyInput { key: Key, shift, ctrl, alt, cmd }`.
  - `encode_mouse(&MouseInput, &TermModes) -> Option<Vec<u8>>`. It returns `None` when mouse mode is off, or when `shift` is held (Shift overrides into local selection).
  - `encode_paste(&str, &TermModes) -> Vec<u8>`. It strips any embedded `\x1b[201~` before wrapping.
  - `find_link_at(&Snapshot, row, col) -> Option<Link>`, where `Link::Url(String)` covers `https?://…` and `Link::Path(PathBuf, Option<u32>)` covers absolute, `~/` and `./` paths, with an optional `:line`.
- **Implementation notes.** `alacritty_terminal::Term` is driven through `vte::ansi::Processor`. The `EventListener` collects `PtyWrite` events into the buffer that `take_pty_writes` returns.

- [ ] **Step 1: Write the failing tests:**
  - **Bridge:**
    - `feed_plain_text`: feeding `"hello"` puts `h` at cell (0,0) and `o` at (0,4).
    - `truecolor_fg`: feeding `\x1b[38;2;10;20;30mX` makes cell (0,0) fg `Rgb(10,20,30)`.
    - `alt_screen_mode`: feeding `\x1b[?1049h` makes `modes().alt_screen` true.
    - `bracketed_paste_mode`: `\x1b[?2004h` makes `modes().bracketed_paste` true.
    - `device_attributes_reply`: feeding `\x1b[c` makes `take_pty_writes()` non-empty.
    - `search_finds_in_scrollback`: feed 200 numbered lines into a 10-row terminal, and `search("line 5 ")` returns 1 match.
  - **Keys:**
    - `shift_enter_is_esc_cr`: Enter with shift gives `b"\x1b\r"`.
    - `enter_is_cr`: Enter gives `b"\r"`.
    - `option_b_is_meta`: char `b` with alt gives `b"\x1bb"`.
    - `ctrl_c`: gives `b"\x03"`.
    - `arrow_normal_vs_app_cursor`: Up gives `b"\x1b[A"`, or `b"\x1bOA"` when `app_cursor`.
    - `cmd_keys_not_encoded`: any key with cmd gives an empty vec, because the GUI owns Cmd shortcuts.
  - **Mouse:**
    - `mouse_off_returns_none`
    - `shift_overrides_mouse_mode`
    - `sgr_left_press_at_3_5`: gives `b"\x1b[<0;6;4M"` (1-based col and row).
  - **Paste:**
    - `paste_plain_when_mode_off`: `"a\nb"` gives `b"a\rb"`.
    - `paste_bracketed`: gives `\x1b[200~a\rb\x1b[201~`.
    - `paste_strips_end_marker`
  - **Links:**
    - `finds_https_url`
    - `finds_path_with_line`: `~/Repos/eks/main.tf:42` gives `Path("~/Repos/eks/main.tf", Some(42))`.
    - `no_link_on_plain_word`
- [ ] **Step 2: Run them to see them fail.** Run `cargo test -p overseer-term`. Expected: FAIL.
- [ ] **Step 3: Implement** each module as specified.
- [ ] **Step 4: Run them to see them pass.** Run `cargo test -p overseer-term`. Expected: all pass.
- [ ] **Step 5: Commit.** Message: "Add terminal bridge and key, mouse, paste and link encoding".

---

### Task 8: TerminalView element (render, input, resize)

**Files:**
- Create: `crates/overseer-gui/src/{terminal_view,theme}.rs`
- Modify: `crates/overseer-gui/src/lib.rs`

**Interfaces:**
- Consumes: `TermBridge`, `encode_key` (Task 7), `Client` (Task 5).
- Produces:
  - `TerminalView::new(slot: SlotId, client: Arc<Client>, theme: Theme, cx) -> Entity<TerminalView>`. It attaches on creation and detaches on drop.
  - `TerminalView::focus(&self, window)`
- **Theme.** `Theme { font_family: String, font_size: f32, palette: Palette, background: Rgb }`, read from `~/.config/overseer/config.toml` `[terminal]` with defaults `"Menlo"`, `13.0` and the xterm palette.
- **Implementation notes:**
  - **Rendering.** It renders the snapshot as rows of shaped text runs, batching cells with the same style. Backgrounds are painted as quads and the cursor is a block or beam per the terminal's cursor shape. Font metrics come from GPUI's text system: the cell width is the advance of `M`.
  - **Output.** It pumps Output frames on a GPUI background task into the bridge, then calls `cx.notify()`. Notifications are coalesced to at most one per frame.
  - **Resize.** On layout size change it computes cols and rows, calls `bridge.resize`, and sends `Request::Resize`.
  - **Keys.** Key down goes to `encode_key` and then `send_input`. `bridge.take_pty_writes()` is flushed after every feed.

- [ ] **Step 1: Temporary wiring.** Temporarily wire `run_gui` to show one full-window `TerminalView` for slot 1, after `ctl start 1`.
- [ ] **Step 2: Manual check against `fake-claude.sh`.** Expected: `READY` shows, the colour line renders in 256-colour and truecolor, and typing `hi` then Enter shows `ECHO:hi`.
- [ ] **Step 3: Manual check against real `claude`** in `~/overseer`. Expected: the welcome screen renders with box drawing aligned, and typing and Enter submit a prompt. Shift+Enter inserts a newline in the input box, Option+Left jumps a word, and Ctrl+C interrupts. Resizing the window reflows Claude's UI with no garbage left behind.
- [ ] **Step 4: Typing-latency check.** Hold a key for 3 s in Claude's input box. Expected: no visible lag compared with iTerm2 side by side, and the CPU of `overseer` stays under 30% while idle output is zero.
- [ ] **Step 5: Run `cargo clippy --workspace -- -D warnings`.** Expected: clean.
- [ ] **Step 6: Commit.** Message: "Render embedded terminal with keyboard input and resize".

---

### Task 9: Terminal fidelity: mouse, selection, clipboard, scrollback, search, links, IME, image paste

**Files:**
- Modify: `crates/overseer-gui/src/terminal_view.rs`

**Interfaces:**
- Consumes: `encode_mouse`, `encode_paste`, `TermBridge::{scroll, search, selection_text}` and `find_link_at` (Task 7).
- **Behaviour:**
  - **Mouse.** Mouse events go to `encode_mouse` when the app has enabled mouse mode. Otherwise, and always with Shift held, they drive a local selection (drag, double-click word, triple-click line).
  - **Copy.** Cmd+C copies the selection when there is one.
  - **Paste.** Cmd+V reads the clipboard. If it holds text, the text goes through `encode_paste`. If it holds **only an image**, the terminal sends `\x16` (Ctrl+V), so that Claude Code reads the image from the clipboard itself, the same as its own image-paste path.
  - **Scrolling.** The wheel scrolls the scrollback when the alt screen is off or mouse mode is off. Otherwise it is encoded to the app.
  - **Search.** Cmd+F opens a find bar over the terminal: Enter or Shift+Enter cycles matches, they're highlighted, and Esc closes the bar.
  - **Links.** Cmd+hover underlines a link. Cmd+click opens URLs with `open <url>` and paths with `open -a <editor> <path>`, where `editor` comes from config with default `Visual Studio Code`. Line numbers are passed as `code -g path:line` when the editor is VS Code.
  - **IME.** Uses GPUI's `InputHandler` for marked text and composition, so dead keys and accents work. Composed text goes to the PTY on commit.

- [ ] **Step 1: Implement the mouse and selection behaviour, then check it manually.** Expected:
  - In plain shell output in `fake-claude.sh`, drag selection works.
  - In Claude Code (mouse mode on), Shift-drag still selects and Cmd+C copies the selected text.
- [ ] **Step 2: Implement paste, then check it manually.** Expected:
  - Pasting 3 lines of text into Claude's input box arrives as one bracketed paste, not 3 submits.
  - Take a screenshot to the clipboard (Cmd+Ctrl+Shift+4) and press Cmd+V in Claude. Claude shows `[Image #1]`.
- [ ] **Step 3: Implement scrollback and Cmd+F, then check them manually.** Expected: after a long Claude answer, the wheel scrolls back and Cmd+F `error` highlights matches and cycles through them.
- [ ] **Step 4: Implement Cmd+hover and Cmd+click links, then check them manually.** Expected: Cmd+click on a URL opens the browser, and on `crates/overseer/src/main.rs:1` it opens the editor at that line.
- [ ] **Step 5: Implement IME composition, then check it manually.** Expected: with the US keyboard, Option+E then E types `é` into Claude's box.
- [ ] **Step 6: Commit.** Message: "Add mouse, selection, clipboard, image paste, search, links and IME".

---

### Task 10: Board, session view, start dialog and navigation

**Files:**
- Create: `crates/overseer-gui/src/{app,board,session,start_dialog}.rs`
- Modify: `crates/overseer-gui/src/lib.rs` (`run_gui(client: Arc<Client>)`), `crates/overseer/src/main.rs` (no args: connect_or_start, then `run_gui`)

**Interfaces:**
- Consumes: `Client`, `SlotInfo` and `Event::SlotChanged` (Tasks 2 and 5), `TerminalView` (Tasks 8 and 9).
- Produces:
  - `enum Screen { Board, Session(SlotId) }`
  - Key bindings in the `Overseer` context:
    - `cmd-0` … `cmd-6` go to `Session(n)`
    - `escape` goes to `Board`, but only when the terminal's find bar isn't open
    - `cmd-n` opens the start dialog for the focused card
- **Board** (layout A from the design):
  - **Header:** `Overseer`, plus the count of running slots.
  - **Coordinator panel:** a placeholder ("Coordinator summary appears here in phase 6"), plus an S0 card.
  - **A 3×2 grid of S1 to S6 cards.** Each shows the slot name and label, a state badge (Running with elapsed time in green, Exited with code in grey, Empty with "+ start"), cwd and pid.
- **Session view:** a tab strip (`Esc board`, `0` to `6`, with the active one highlighted), the `TerminalView` on the left at about 62%, and a `ContextPanel` on the right. In phase 1 the panel shows label, cwd, pid, state, started time and buttons `Stop` and `Restart`. "Owns", "Holds" and "Progress" sections are placeholders, filled in phases 3 to 4.
- **Start dialog:** label, cwd (a text field with a folder picker through `cx.prompt_for_paths`, default `~/Repos`) and command (default `claude`). Submit sends `StartSlot` and opens that session.
- **Live updates.** Cards update live from `Event::SlotChanged`.
- **Terminal views are kept alive while switching tabs**, one entity per running slot, so switching is instant and keeps scroll position.

- [ ] **Step 1: Implement the board and cards. Manual check:** start S1 and S3 with `ctl`, then open the GUI. Expected: S1 and S3 are Running with ticking elapsed time and the others are Empty. `ctl stop 3` flips S3 to Exited within 1 s.
- [ ] **Step 2: Implement the session view, tabs and key bindings. Manual check:** Expected:
  - Clicking the S1 card opens its terminal with Claude live.
  - Cmd+3 switches to S3, Esc returns to the board.
  - Typing `1` in the terminal sends `1` to Claude, and doesn't navigate.
- [ ] **Step 3: Implement the start dialog. Manual check:** Expected: clicking `+ start` on S5 and choosing `~/overseer` and the default command opens S5 with Claude running in that cwd.
- [ ] **Step 4: Survival check.** Quit the GUI (Cmd+Q) with S1 and S3 running, then reopen it. Expected: both are still running, and their terminals show the replayed screen after the redraw.
- [ ] **Step 5: Commit.** Message: "Add board, session view, start dialog and navigation".

---

### Task 11: App bundle, install and phase 1 acceptance

**Files:**
- Create: `packaging/Info.plist`, `scripts/bundle.sh`
- Modify: `Makefile` (`install`, `uninstall`), `README.md`

**Interfaces:**
- Consumes: the binary from all earlier tasks.
- **`make install`:**
  1. If `cargo` is missing, prints the user-local rustup command and stops.
  2. Runs `cargo build --release -p overseer`.
  3. Runs `scripts/bundle.sh`, which assembles `~/Applications/Overseer.app/Contents/{MacOS/overseer,Info.plist}` and runs `codesign --force --sign - ~/Applications/Overseer.app`.
  4. Symlinks `~/.local/bin/overseer` to the bundled binary.
  5. Runs `overseer install-agent`.
  6. Prints next steps.
- **`make uninstall`** reverses each step and leaves the state dir alone.
- **README.** It covers install on a non-admin Mac, `overseer attach` as an emergency fallback, and upgrade with `git pull && make install`, with a warning that this restarts the daemon and running sessions end. Resume arrives in phase 2.

- [ ] **Step 1: Write `bundle.sh` and the Makefile targets.**
- [ ] **Step 2: Install.** Run `make install`. Expected: exit 0, `~/Applications/Overseer.app` opens from Finder without a Gatekeeper prompt, and `launchctl print gui/$UID/uk.co.techize.overseer` shows it running.
- [ ] **Step 3: Phase 1 acceptance against real Claude.** Run 4 Claude sessions (S1 to S4) for 30 minutes of normal work. Then tick each design must-have:
  - truecolor and 256-colour
  - Claude TUI and resize
  - Shift+Enter
  - image paste
  - Option as Meta
  - Cmd+click links and paths
  - mouse selection and copy
  - Cmd+F search
  - font, size and theme from config
  - IME
  - bracketed paste

  Record the results in `docs/acceptance/phase-1.md`. Every item must pass before phase 1 is called done.
- [ ] **Step 4: Commit.** Message: "Add app bundle and install, record phase 1 acceptance".
