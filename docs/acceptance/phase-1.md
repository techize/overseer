# Phase 1 acceptance

Phase 1 is done only when every row below passes against a real Claude session
on the installed app (`make install`, then `~/Applications/Overseer.app`).

The Result column says what was actually observed and how. "awaiting Jonathan"
means nobody has checked it by hand yet. Unit tests named in a Result cover the
encoding or logic only; they do not replace the hand check against real Claude
unless the row says so.

Checks marked "verified 2026-11-09" were run on home.local with the release
build installed by `make install`, the daemon under launchd and Claude Code at
`/opt/homebrew/bin/claude`.

Checks marked "verified 2026-11-10" were driven by script on home.local:
System Events keystrokes, synthetic mouse events and screencapture, against the
installed build of main at ddd889d with Claude Code 2.1.259. The Claude CLI on
that Mac was logged out ("Login expired"), so those checks used Claude's input
box and its `!` bash mode rather than model answers.

## Setup for the hand checks

1. `make install` in `~/overseer`.
2. Open Overseer from `~/Applications` in Finder.
3. Start S1 to S4 from the board (`+ start`, cwd `~/overseer` or a real project,
   default command) and do about 30 minutes of normal Claude work across them
   before ticking the terminal items.

## Install and daemon

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| I1 | `make install` succeeds and is idempotent | Run `make install` twice in a row | Both exit 0 | Verified 2026-11-09: exit 0 on 4 runs (two in a row before and after an uninstall) |
| I2 | Missing cargo is explained | `env PATH=/usr/bin:/bin make install` | Prints the rustup command and stops before building | Verified 2026-11-09: printed the rustup line, make stopped |
| I3 | Agent running | `launchctl print gui/$UID/uk.co.techize.overseer` | `state = running`, program is `~/Applications/Overseer.app/Contents/MacOS/overseer`, logs under `~/Library/Logs/Overseer` | Verified 2026-11-09: state running, program and stdout/stderr paths as expected |
| I4 | Agent PATH finds claude | `launchctl print ...` shows an EnvironmentVariables PATH; `overseer ctl start 3 --cwd ~/overseer`, then `overseer ctl status` | PATH includes `/opt/homebrew/bin`; S3 is `running` with a `claude` child of the launchd daemon | Verified 2026-11-09: S3 running pid 43875, parent was the launchd daemon pid; screencapture showed Claude's MCP approval prompt in S3. Stopped afterwards |
| I5 | `ctl shutdown` keeps it down | `overseer ctl shutdown`, wait 12 s, `launchctl print ...` | `state = not running`, `last exit code = 0`, no respawn | Verified 2026-11-09: not running with last exit code 0 for 12 s (also `daemon_logs_and_exits_zero_on_shutdown`) |
| I6 | Next client starts it again | After I5, `overseer ctl status` | Prints 7 slots; agent is running again under launchd (parent pid 1) | Verified 2026-11-09: status printed, launchd `runs` went up, daemon parent pid 1 |
| I7 | Crash is respawned | `kill -9 <daemon pid>`, wait 12 s | Agent is running with a new pid | Verified 2026-11-09: new pid after about 10 s, `runs = 3` |
| I8 | Daemon logs | `tail ~/Library/Logs/Overseer/daemon.log` | Start, slot start/exit and stop lines | Verified 2026-11-09: starting, `S3 started ["claude"]`, `S3 exited`, `daemon stopped` lines present |
| I9 | Signature and Gatekeeper | `codesign -dv ~/Applications/Overseer.app`; `xattr -l ~/Applications/Overseer.app`; open it from Finder | Ad hoc signature; no `com.apple.quarantine`; opens without a Gatekeeper prompt | Partly verified 2026-11-09: `Signature=adhoc`, `codesign --verify --deep --strict` OK, only `com.apple.provenance` (no quarantine). `open` launched it with no prompt. `spctl --assess` says "rejected", as expected for an ad hoc app; this does not block a locally built, unquarantined app. Opening from Finder by hand: awaiting Jonathan |
| I10 | Board renders | `open ~/Applications/Overseer.app` | Board with S0 to S6 cards | Verified 2026-11-09 by screencapture: board drawn, S3 card Running with cwd and pid. macOS showed its "App Background Activity" notice for the agent, which is expected |
| I11 | `make uninstall` | Run `make uninstall` (then `make install` again) | Agent gone, plist, link and app removed, state and logs kept; second run also exits 0 | Verified 2026-11-09: all three removed, `launchctl print` could not find the service, state dir and logs kept, a second uninstall exited 0. Reinstalled afterwards |
| I12 | App exits 0 when its window closes (R3) | Run the bundled binary from a shell, close its window, `echo $?` | Exit status 0, sessions keep running | Verified 2026-11-09: exit=0 after clicking the close button via osascript; S3 (real Claude) still running |

## Terminal must-haves (design 7.9)

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| T1 | Truecolor and 256-colour | In a shell slot run `printf '\e[48;2;255;100;0m truecolor \e[0m \e[48;5;33m 256 \e[0m\n'`, then look at Claude's diff and syntax colours | An orange block and a blue block; Claude colours match iTerm2 | Verified 2026-11-09 by screencapture on the installed build: a 24-bit gradient, 72 256-colour swatches, RGB and 256 foreground, bold and italic all drawn correctly (scripted slot, not Claude). Claude's own colours: awaiting Jonathan. 2026-11-10: Claude's UI colours (logo, yellow and red notices, status line) matched the same session attached in iTerm2. Diff and syntax colours not seen because Claude was logged out |
| T2 | Claude TUI and resize | Use Claude normally; resize the window while Claude is drawing and while idle | No garbage, input box and status line redraw at the new size | Verified 2026-11-10: resized from 1200x833 to 900x600 while idle, then five resizes while 60 lines streamed from Claude's bash mode; the input box and status line redrew at each size with no garbage. Resize while the model streams an answer: awaiting Jonathan (Claude was logged out) |
| T3 | Shift+Enter | Type a line in Claude, Shift+Enter, type another, Enter | One message with two lines | Verified 2026-11-10 in real Claude: `line one`, Shift+Enter, `line two` gave one input with two lines |
| T4 | Image paste | Cmd+Ctrl+Shift+4 a region to the clipboard, Cmd+V in Claude | Claude shows `[Image #1]`; confirms gpui `read_from_clipboard` sees the screenshot image | Verified 2026-11-10: `screencapture -c` of a screen region (a PNG on the clipboard, as Cmd+Ctrl+Shift+4 leaves it), then Cmd+V in Claude showed `[Image #1]` |
| T5 | Option as Meta | In Claude's input, Option+B and Option+F over a few words, Option+Backspace | Word jumps and word delete work | Verified 2026-11-10 in real Claude: Option+B twice then X gave `alpha Xbeta gamma`; Option+F then Option+Backspace left `alpha gamma` |
| T6 | Cmd+click links and paths | Cmd+hover and Cmd+click an http(s) URL in Claude output; Cmd+click `crates/overseer/src/main.rs:1` in a slot whose cwd is the repo | URL underlines and opens in the default browser; path opens in the editor (VS Code at line 1) | Verified 2026-11-10 in real Claude output, app launched with `open`: Cmd+click on `https://example.com/overseer-test` brought the default browser forward; Cmd+click on `crates/overseer/src/main.rs:1` opened VS Code on main.rs. Underline on Cmd+hover was not seen with a synthetic Cmd key: awaiting Jonathan |
| T7 | Mouse selection and copy | Drag, double-click and triple-click in Claude output, Cmd+C, paste elsewhere | Copied text matches the highlight | Verified 2026-11-10 in real Claude. Claude turns mouse reporting on, so a plain drag uses Claude's own selection and Shift selects locally: Shift double-click copied `error`, Shift triple-click a whole line, Shift-drag rows 91 to 93, each matching the highlight |
| T8 | Cmd+F search | After a long Claude answer, Cmd+F "error", Enter and Shift+Enter, Esc | Matches tinted, current one orange, view scrolls to it, i/n count right | Verified 2026-11-10 in real Claude: Cmd+F `error` tinted both visible matches with the current one orange, count 2/2; Enter went to 1/2, Shift+Enter back to 2/2, Esc closed it. Scrolling to an off-screen match was not exercised. Note: Claude runs with `CLAUDE_CODE_NO_FLICKER=1` from the login shell and draws on the alternate screen, so search only sees what Claude has on screen (rows 30 and 60 had scrolled inside Claude and were not found). iTerm2 behaves the same |
| T9 | Font, size and theme from config | Put `[terminal]` with `font_family`, `font_size`, `background`, `foreground` in `~/.config/overseer/config.toml`, relaunch the app | Terminal uses them | Verified 2026-11-10: `[terminal]` with Courier New, 18, `#1a2b4c`, `#ffd700`; after a relaunch the terminal used all four. Config removed afterwards |
| T10 | IME | Option+E then E in Claude's input; switch to a Japanese or Chinese input source and type a word, pick a candidate | é appears; candidate window sits at the cursor and the committed text is inserted | Dead keys verified 2026-11-10 in real Claude: Option+E E gave é, Option+U U gave ü. CJK: awaiting Jonathan |
| T11 | Bracketed paste | Copy 3 lines, Cmd+V into Claude's input | One paste with 3 lines, not three submissions | Verified 2026-11-10 in real Claude: three copied lines pasted as one input with three lines, nothing submitted |

## Carried over from earlier tasks

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| C1 | `overseer attach` with real Claude in iTerm2 | Start S1 with Claude. In iTerm2 run `overseer attach 1`, type into Claude, press Ctrl-] | Claude is mirrored and usable; Ctrl-] returns to the shell with the prompt where it was, no cursor jump or blank screen (the restore writes `\x1b[?1049l` every time) | Failed 2026-11-10 on the first run (F1). Verified 2026-11-10 after the fix: in a new iTerm2 window, `overseer attach 1` mirrored Claude and typed text reached its input; after Ctrl-] the screen from before the attach (a marker echoed just before it and the prompt) was back as it was, `Detached from S1. The session is still running.` sat on a clean line, the title was `-zsh` again and iTerm2 showed no clipboard warning |
| C2 | Image paste via gpui clipboard | As T4 | `[Image #1]` in Claude | Verified 2026-11-10, as T4 |
| C3 | IME | As T10, both dead keys and CJK | As T10 | Dead keys verified 2026-11-10, as T10. CJK: awaiting Jonathan |
| C4 | Typing latency and CPU | Type quickly in Claude in Overseer and in iTerm2 side by side. Run `top -pid <gui pid> -pid <daemon pid>` with the board idle, with a session idle, and while Claude streams a long answer | Typing feels no slower than iTerm2; idle CPU near 0 | Partly verified 2026-11-09: release build, board open with no running session: GUI 0.8 to 0.9 % CPU, daemon 0.0 % (top, 1 s samples). Latency and CPU under output: awaiting Jonathan. 2026-11-10: GUI 1.1 to 1.4 % with S1's session view open and idle, 1.7 to 2.4 % while 600 lines streamed through Claude's bash mode, daemon 0.0 % throughout. Claude collapses bash output, so a long model answer may cost more. Typing feel against iTerm2: awaiting Jonathan |
| C5 | Trackpad and alt-screen scrolling | Trackpad-scroll a long Claude answer; scroll in Claude's full-screen views and in `vim` with mouse off | Smooth scrollback in normal screen; alt screen gets arrows (1007) or mouse reports, per R9 | Verified 2026-11-10 with the mouse wheel: in Claude (alternate screen, mouse reporting on) the wheel went to Claude, which scrolled its own view and showed `Jump to bottom`; in `vim -u NONE` (mouse off) the wheel sent arrows; after `:set mouse=a` it sent mouse reports. Real trackpad: awaiting Jonathan |
| C6 | Font rendering by eye | Compare Overseer and iTerm2 with the same font and size | Crisp, correctly spaced glyphs, box drawing joins up | awaiting Jonathan (screenshots of Overseer and iTerm2 look alike, but this is a judgement by eye) |
| C7 | Cmd+click URL and path | As T6 | As T6 | As T6 |
| C8 | Shift-drag with mouse mode on | In Claude (or anything with mouse reporting on), Shift-drag across text, Cmd+C | Local selection, correct text copied, click without Shift goes to the app | Verified 2026-11-10 in real Claude: a plain click reached Claude (it jumped to the bottom), Shift-drag selected locally and Cmd+C copied the highlighted rows |
| C9 | Folder picker in the start dialog | Board, `+ start` on S5, use the folder picker | Native picker opens, the chosen folder fills the cwd | Verified 2026-11-10: Choose... opened the native folder picker; choosing `~/overseer` filled the folder field. Minor: the picker opened in Documents, not at the folder already in the field |
| C10 | Board flow with real Claude | Start dialog, session view, Stop, Restart, Esc and Cmd+Esc back to the board, Cmd+1..6 and Cmd+0 | Every step works; Esc reaches the board from the session chrome, Cmd+Esc from the terminal | Verified 2026-11-10: start dialog, session view, Restart (new pid) and Stop on S5 by the buttons, Cmd+Esc to the board from the terminal, Esc to the board after clicking the session panel, Cmd+0, Cmd+1, Cmd+3, Cmd+5 and Cmd+6 all switched |
| C11 | Task 10 Step 1 | Start S1 and S3 with `ctl`, open the app, then `overseer ctl stop 3` | S1 and S3 Running with ticking elapsed time, others Empty; S3 flips to Exited within 1 s | Verified 2026-11-10: S1 and S3 Running with the elapsed time going from 17 s to 19 s, the others Empty; a capture 0.9 s after `ctl stop 3` showed S3 Exited (code 129) |
| C12 | Task 10 Step 2 | Click the S1 card; Cmd+3; Esc; type `1` in the terminal | Terminal with live Claude; Cmd+3 switches; Esc returns; `1` goes to Claude and does not navigate | Verified 2026-11-10: clicking the S1 card opened live Claude; Cmd+3 switched; Cmd+Esc returned to the board; digits typed in the terminal went to Claude |
| C13 | Task 10 Step 3 | `+ start` on S5, choose `~/overseer` and the default command | S5 opens with Claude running in that cwd | Verified 2026-11-10: `+ start` on S5, `~/overseer` chosen, default command; Claude started with cwd `/Users/jonathan/overseer` (lsof) |
| C14 | Task 10 Step 4, Cmd+Q survival | With S1 and S3 running, Cmd+Q, reopen the app | Both still running; terminals show the replayed screen after the redraw | Verified 2026-11-10: with S1 and S5 running, Cmd+Q quit the GUI, both stayed running, and after reopening both terminals showed the replayed screen |
| C15 | Task 9 item 1 | Real Claude in mouse mode: click, Shift-drag, Cmd+C | Click goes to Claude, Shift-drag selects locally, Cmd+C copies the right text | As C8 |
| C16 | Task 9 item 2 | Paste 3 lines into Claude's input | One paste, not three submissions | As T11 |
| C17 | Task 9 item 3 | Cmd+Ctrl+Shift+4, then Cmd+V in Claude | `[Image #1]` | As T4 |
| C18 | Task 9 item 4 | After a long answer: wheel scrollback, Cmd+F "error", Enter and Shift+Enter | Scrolls back; matches found, revealed and cycled | Verified 2026-11-10, as T8 (search) and C5 (wheel) |
| C19 | Task 9 item 5 | Cmd+click an http(s) URL in Claude output | Opens in the default browser | As T6 (URL) |
| C20 | Task 9 item 6 | Cmd+click `crates/overseer/src/main.rs:1` in a slot whose cwd is the repo | VS Code opens at line 1 | As T6 (path) |
| C21 | Task 9 item 7 | Option+E E, Option+U U, Option+B in Claude's input | é, ü, and a word jump back | Verified 2026-11-10: é, ü, and the Option+B word jump (T5) |
| C22 | Task 9 item 8 | Trackpad scroll on the alt screen (Claude full-screen UI, or vim with mouse off) | Per R9: arrows with 1007, scrolls the app when mouse mode is on | Verified 2026-11-10 with the mouse wheel, as C5. Trackpad: awaiting Jonathan |
| C23 | Task 9 item 9 | Japanese or Chinese IME in Claude | Candidate window at the cursor; committing inserts the text | awaiting Jonathan |

## Findings from the 2026-11-10 run

- **F1 (failed C1, fixed): attach did not protect the outer screen.** `overseer attach`
  writes the replay straight onto the outer terminal's primary screen. The bytes
  Claude wrote before it entered its alternate screen overwrite the user's shell
  screen, so the `\x1b[?1049l` at detach has nothing to restore. Fixed: attach
  now enters the outer alternate screen before the replay and replaces the
  session's own 1049, 1047 and 47 switches with clears (`outer.rs`).
- **F2 (fixed): the attach replay re-sent clipboard writes.** Claude's OSC 52 from an
  earlier selection is in the ring, so attaching tried to set iTerm2's clipboard
  (iTerm2 blocked it). Fixed: the replay now drops every OSC 52 sequence.
- **F3 (minor, fixed): attach left the window title.** After detach iTerm2 still
  said "Claude Code". Attach now saves and restores it (`\x1b[22;0t`,
  `\x1b[23;0t`).
- **F4 (minor): the folder picker ignores the current field** and opens in the
  last folder macOS remembers.
- **Not Overseer:** `~/.claude/settings.json` points hooks at
  `/Users/jonathan.gill/...`, so every session on home.local shows SessionStart
  hook errors.

## Sign-off

Phase 1 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by:
- Date:
