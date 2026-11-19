# Sessions that survive a daemon restart

Issue: #62

## Why

Every Claude session in a slot is a child of the daemon, on a terminal the
daemon opened (`Slot::spawn_in` in `crates/overseer-daemon/src/slot.rs`).
Restarting the daemon, which `make install` does, kills them all. The ledger
brings each back as `Resumable`, but a running turn is lost and every card
needs r.

The user builds from source on the work laptop and wants
`git pull && make install` to put new daemon code into service without
disturbing the sessions that are running, mid-turn work included.

## Goals

- A daemon restart (install, `ctl restart`, launchd restarting a crashed
  daemon) leaves every running claude process running, on the same terminal,
  with no lost output.
- After the restart, each slot shows the same live screen as before,
  accepts input, resizes, and reports its exit as it does today.
- Upgrades are `git pull && make install`. No extra step, no prompt about
  ending sessions.
- Hook events sent while the daemon is down are not lost.

## Non-goals

- Upgrading the holder itself in place. A running holder stays on the build
  that started it until its session ends.
- Surviving a reboot or logout. Those still end sessions, and the ledger
  resume path still covers them.
- Changing the GUI. It already reconnects after a daemon restart
  (`overseer-gui/src/reconnect.rs`).
- macOS privacy-protected folders. The team's sessions never run in
  ~/Documents, ~/Desktop, ~/Downloads or iCloud Drive, so a change in which
  process macOS holds responsible for claude's file access does not matter.

## Anti-goals

- A holder that grows features. It must stay small enough that old holders
  and new daemons can talk for months.
- Any restart path that leaves a claude process running but unreachable,
  with no card for it.

## Approaches

1. **Drain, restart, resume automatically.** Wait for every slot to be
   idle, restart, run `claude --resume` on each. Small, but every session
   reloads and long turns block the upgrade.
2. **Re-exec with handed-over file descriptors.** The daemon execs the new
   binary keeping the terminal fds and sockets open. True zero disruption,
   but fragile across tokio and portable-pty, and it needs approach 1 as a
   fallback anyway.
3. **A holder process per slot (chosen).** A small process owns the
   terminal and the claude child; the daemon talks to it over a socket and
   reattaches after any restart. Roughly four to five times the work of
   approach 1, but contained mostly inside `Slot`, and it also makes
   sessions survive a daemon crash. The user picked this one.

## Design

### The holder

A new subcommand, `overseer hold`, one process per running slot. It:

- detaches into its own process session (`setsid`) before anything else.
  The LaunchAgent plist does not set `AbandonProcessGroup`, so launchd kills
  the daemon's whole process group when the job stops; the holder must not
  be in it.
- opens the terminal and spawns the command exactly as `Slot::spawn_in`
  does today (same environment, `OVERSEER_SLOT`, `OVERSEER_RUN`, `TERM`,
  `COLORTERM`). The daemon passes the spawn spec to it; the holder makes up
  nothing.
- reads the terminal all the time, whether or not a daemon is connected, so
  claude never blocks on a full terminal while the daemon is away. Output
  goes into a ring buffer the size of today's `RING_CAP`, and to the
  connected daemon if there is one.
- listens on `<state>/holders/<slot>.sock` (directory mode 0700), and
  serves one daemon connection at a time. A new connection replaces the old
  one.
- waits on the child. When it exits, the holder sends the exit to the
  connected daemon. With no daemon connected, it writes
  `<state>/holders/<slot>.exit` (exit code, time, and the ring contents) and
  exits. The daemon picks that file up on start.
- does not run the screen emulator. Only the daemon keeps a screen.

### The binary the holder runs from

Holders run from the daemon's own executable (`std::env::current_exe()`),
the app bundle's `Contents/MacOS/overseer`. `scripts/bundle.sh` already
unlinks that file before copying the new one in, so a running holder keeps
its old file and an install never rewrites it underneath it. No copy of the
binary is needed. A test pins this: the install path must never write to
the existing file.

### Protocol

Length-prefixed frames on a Unix stream socket. The first frame each way is
a hello carrying a protocol version number.

- Holder to daemon: `Hello { version, slot, pid, run, label, cwd, command,
  cols, rows, started }`, then `Replay(bytes)` (the ring), then `Output(bytes)`
  as it arrives, then `Exit(code)`.
- Daemon to holder: `Hello { version }`, `Input(bytes)`, `Resize { cols, rows }`,
  `Kill`, `ExitSeen` (the holder can exit once it gets this).

The protocol is versioned from the first release. A daemon supports the
current version and the one before. The holder never sees a newer daemon as
an error; unknown frames are ignored.

Each holder also writes `<state>/holders/<slot>.pid` (holder pid, child pid,
protocol version) when it starts, so a daemon can always end a session
without speaking its protocol. A holder whose version the daemon does not
support is treated as orphaned for good:

- its slot shows as running on an older build, with Stop as the only action;
- Stop signals the holder by pid (SIGTERM to the child's process group,
  then the holder), and the slot becomes `Resumable` from the ledger;
- starting a new session in that slot is refused until it is stopped, so two
  claudes never share a slot, a socket or `OVERSEER_SLOT`.

The `.pid` file is checked against the live process (pid exists, and the
process is `overseer hold`) before any signal, so a reused pid is never
killed.

### `Slot` over the holder

`Slot` keeps its public API (`spawn_in`, `write`, `resize`, `kill`,
`screen`, `snapshot`, `attach`, `attach_at`, `catch_up`, `info`,
`wait_exit`, `run`), so the manager, courier and server change as little as
possible.

- `spawn_in` starts a holder, waits for its socket, connects, and builds the
  slot from its hello.
- A new `Slot::attach_holder(socket)` builds a slot from a holder that is
  already running.
- The reader thread reads frames from the socket instead of the terminal.
  It feeds the ring, the broadcast channel and the screen exactly as today.
- `write`, `resize` and `kill` send frames. `wait_exit` resolves on the
  `Exit` frame. A dropped socket with no `Exit` frame means the holder died;
  the slot then shows as exited with no code, and the ledger offers resume.

### Reattach on start

`Manager::restore()` first lists `<state>/holders/*.sock`:

- Live holder: attach it. The slot is `Running` with the holder's original
  `run` token, so hook events from that session still match.
- `.exit` file: record the exit in the ledger, show the slot as `Exited`
  with that code, delete the file.
- Socket with no listener: delete it.

Then it falls back to today's ledger path for any slot still live in the
ledger with no holder: those are `Resumable`, as now.

**Activity.** Activity lives only in daemon memory and starts as `None`,
and the courier will not deliver to a slot with no activity. So on
shutdown the daemon writes each slot's activity and when it was set to
`<state>/holders/<slot>.activity`, and a reattached slot starts with it.
Spooled hook events (below) are applied on top, so anything that happened
while the daemon was down wins. A slot saved as Working keeps the existing
quiet-screen rule in the courier (`QUIET_FOR`), so a relay is never typed
into a turn that is still running.

**Screen rebuild.** The daemon's screen copy does not survive a restart, and
a replay of the ring alone can miss rows drawn before its oldest byte (the
comment on `Slot::screen` says why). After feeding the replay into a fresh
screen, the daemon resizes the terminal one column narrower and back, which
makes Claude Code redraw everything. Until that redraw has arrived (output
after the resize, then the screen quiet for a moment), the courier treats
the slot as not ready, so it never reads a half-rebuilt screen.

The first task of the build checks the redraw against the installed Claude
Code. If it does not redraw fully, the fallback is that the holder keeps a
screen too, reusing the existing emulator, and sends it on connect as
plain terminal bytes that repaint it (not a new snapshot type), so the
protocol does not grow. That is decided at that checkpoint, not later.

### Ending sessions

- On SIGTERM (launchd stopping the job, install, `ctl restart`),
  `shutdown_all` closes its holder connections and leaves the holders
  running. It no longer kills children.
- Stopping a slot from the GUI or `ctl` sends `Kill`, as today.
- `scripts/uninstall.sh` ends every held session. A new `ctl stop-all`
  does it through the daemon's shutdown path, so the ledger keeps them live
  and a reinstall offers them as `Resumable`, matching uninstall's promise
  to leave state in place. Locks and relays are kept, as on any shutdown.
  With no daemon running, uninstall signals each holder from its `.pid`
  file. Then it removes `<state>/holders`.

### Install and restart prompts

`restart_choice` in `crates/overseer/src/agent.rs` warns that a restart ends
sessions. Once every running session is held, a restart ends none, so it
restarts without asking. The daemon reports, per session, whether it is
held, so the prompt still appears for sessions started before this feature.
The first install of this feature ends sessions one last time; the install
output says so.

### Hook events while the daemon is down

`overseer hook` delivers within 50 ms or gives up (`DELIVER_WITHIN` in
`crates/overseer/src/hook.rs`). When it cannot connect at all, it appends
the event as one JSON line to `<state>/hook-spool.jsonl` (opened with
`O_APPEND`, one `write` per line). On start, after reattaching holders, the
daemon replays the spool in order for slots whose `run` token matches a
reattached session, drops the rest, and empties the file. The `PreToolUse`
lock check keeps failing open when the daemon is down, as it does now.

## Testing

Integration tests in `crates/overseer-daemon/tests/server.rs` with the
existing `tests/fake-claude.sh`:

- restart the daemon: the session keeps its pid, the screen shows the same
  content, input typed after the restart reaches it;
- output written while the daemon is down appears after reattach;
- the child exits while the daemon is down: the next daemon shows the exit
  code;
- the holder is killed while the daemon is down: the slot comes back
  `Resumable`;
- a hook event sent while the daemon is down is applied after the restart;
- a holder speaking an unsupported version is left running, shown as an
  older-build session, refuses a new start in its slot, and Stop ends it by
  pid;
- a reattached idle slot gets a waiting relay (activity restored);
- a reattached slot gets no relay before its redraw has arrived.

Unit tests for the frame encoding, the spool reader, and the version check.

One manual check on macOS, since tests cannot run launchd: with a session
running, `launchctl kickstart -k gui/$UID/uk.co.techize.overseer`, and the
claude process must still be there afterwards.

## Left to the builder

- How the daemon crate's tests start a holder. Tests cannot run the
  installed binary, so the holder logic should live in `overseer-daemon` as a
  library function and the tests need an executable that calls it (a test
  binary target, or whatever fits the workspace best).
- Frame encoding details (header size, maximum frame size).
- Exact wording of the "older build" note and the install message.
