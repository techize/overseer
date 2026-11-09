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
| T1 | Truecolor and 256-colour | In a shell slot run `printf '\e[48;2;255;100;0m truecolor \e[0m \e[48;5;33m 256 \e[0m\n'`, then look at Claude's diff and syntax colours | An orange block and a blue block; Claude colours match iTerm2 | Verified 2026-11-09 by screencapture on the installed build: a 24-bit gradient, 72 256-colour swatches, RGB and 256 foreground, bold and italic all drawn correctly (scripted slot, not Claude). Claude's own colours: awaiting Jonathan |
| T2 | Claude TUI and resize | Use Claude normally; resize the window while Claude is drawing and while idle | No garbage, input box and status line redraw at the new size | awaiting Jonathan |
| T3 | Shift+Enter | Type a line in Claude, Shift+Enter, type another, Enter | One message with two lines | Encoding unit-tested (`keys::tests::shift_enter_is_esc_cr`). Real Claude: awaiting Jonathan |
| T4 | Image paste | Cmd+Ctrl+Shift+4 a region to the clipboard, Cmd+V in Claude | Claude shows `[Image #1]`; confirms gpui `read_from_clipboard` sees the screenshot image | Image-only clipboard sending 0x16 was driven in task 9 with a zsh slot (`image_only_paste_sends_ctrl_v`). Real screenshot into Claude: awaiting Jonathan |
| T5 | Option as Meta | In Claude's input, Option+B and Option+F over a few words, Option+Backspace | Word jumps and word delete work | Encoding unit-tested (`keys::tests::option_b_is_meta`). Real Claude: awaiting Jonathan |
| T6 | Cmd+click links and paths | Cmd+hover and Cmd+click an http(s) URL in Claude output; Cmd+click `crates/overseer/src/main.rs:1` in a slot whose cwd is the repo | URL underlines and opens in the default browser; path opens in the editor (VS Code at line 1) | argv building unit-tested (`web_urls_open_with_open`, `path_with_line_uses_code_g_for_vscode`). Real clicks: awaiting Jonathan. Note: the app launched from Finder has launchd's short PATH, so check `code` is found |
| T7 | Mouse selection and copy | Drag, double-click and triple-click in Claude output, Cmd+C, paste elsewhere | Copied text matches the highlight | Driven with a zsh slot in task 9. Real Claude: awaiting Jonathan |
| T8 | Cmd+F search | After a long Claude answer, Cmd+F "error", Enter and Shift+Enter, Esc | Matches tinted, current one orange, view scrolls to it, i/n count right | Driven with a zsh slot in task 9. Real Claude: awaiting Jonathan |
| T9 | Font, size and theme from config | Put `[terminal]` with `font_family`, `font_size`, `background`, `foreground` in `~/.config/overseer/config.toml`, relaunch the app | Terminal uses them | Parsing unit-tested in `theme.rs`. On screen: awaiting Jonathan |
| T10 | IME | Option+E then E in Claude's input; switch to a Japanese or Chinese input source and type a word, pick a candidate | é appears; candidate window sits at the cursor and the committed text is inserted | Dead keys driven with `cat -v` in task 9 (é and ü). Claude and CJK: awaiting Jonathan |
| T11 | Bracketed paste | Copy 3 lines, Cmd+V into Claude's input | One paste with 3 lines, not three submissions | Encoding unit-tested (`paste::tests::paste_bracketed`); one bracketed paste seen in a zsh slot in task 9. Real Claude: awaiting Jonathan |

## Carried over from earlier tasks

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| C1 | `overseer attach` with real Claude in iTerm2 | Start S1 with Claude. In iTerm2 run `overseer attach 1`, type into Claude, press Ctrl-] | Claude is mirrored and usable; Ctrl-] returns to the shell with the prompt where it was, no cursor jump or blank screen (the restore writes `\x1b[?1049l` every time) | awaiting Jonathan |
| C2 | Image paste via gpui clipboard | As T4 | `[Image #1]` in Claude | awaiting Jonathan |
| C3 | IME | As T10, both dead keys and CJK | As T10 | awaiting Jonathan |
| C4 | Typing latency and CPU | Type quickly in Claude in Overseer and in iTerm2 side by side. Run `top -pid <gui pid> -pid <daemon pid>` with the board idle, with a session idle, and while Claude streams a long answer | Typing feels no slower than iTerm2; idle CPU near 0 | Partly verified 2026-11-09: release build, board open with no running session: GUI 0.8 to 0.9 % CPU, daemon 0.0 % (top, 1 s samples). Latency and CPU under output: awaiting Jonathan |
| C5 | Trackpad and alt-screen scrolling | Trackpad-scroll a long Claude answer; scroll in Claude's full-screen views and in `vim` with mouse off | Smooth scrollback in normal screen; alt screen gets arrows (1007) or mouse reports, per R9 | Wheel scrollback driven with a zsh slot in task 9. Trackpad and alt screen: awaiting Jonathan |
| C6 | Font rendering by eye | Compare Overseer and iTerm2 with the same font and size | Crisp, correctly spaced glyphs, box drawing joins up | awaiting Jonathan |
| C7 | Cmd+click URL and path | As T6 | As T6 | awaiting Jonathan |
| C8 | Shift-drag with mouse mode on | In Claude (or anything with mouse reporting on), Shift-drag across text, Cmd+C | Local selection, correct text copied, click without Shift goes to the app | Driven with a mouse-mode zsh slot in task 9. Real Claude: awaiting Jonathan |
| C9 | Folder picker in the start dialog | Board, `+ start` on S5, use the folder picker | Native picker opens, the chosen folder fills the cwd | awaiting Jonathan |
| C10 | Board flow with real Claude | Start dialog, session view, Stop, Restart, Esc and Cmd+Esc back to the board, Cmd+1..6 and Cmd+0 | Every step works; Esc reaches the board from the session chrome, Cmd+Esc from the terminal | Partly verified 2026-11-09: Cmd+3 and Cmd+5 opened the session view (real Claude in S3, scripted slot in S5) by osascript. The rest: awaiting Jonathan |
| C11 | Task 10 Step 1 | Start S1 and S3 with `ctl`, open the app, then `overseer ctl stop 3` | S1 and S3 Running with ticking elapsed time, others Empty; S3 flips to Exited within 1 s | Partly verified 2026-11-09: with S3 alone, the board showed S3 Running with an elapsed time (one screenshot, so ticking not observed) and the rest Empty. Two slots and the 1 s flip: awaiting Jonathan |
| C12 | Task 10 Step 2 | Click the S1 card; Cmd+3; Esc; type `1` in the terminal | Terminal with live Claude; Cmd+3 switches; Esc returns; `1` goes to Claude and does not navigate | awaiting Jonathan |
| C13 | Task 10 Step 3 | `+ start` on S5, choose `~/overseer` and the default command | S5 opens with Claude running in that cwd | awaiting Jonathan |
| C14 | Task 10 Step 4, Cmd+Q survival | With S1 and S3 running, Cmd+Q, reopen the app | Both still running; terminals show the replayed screen after the redraw | Partly verified 2026-11-09: Cmd+Q via osascript quit the GUI while S5 ran; `ctl status` still showed S5 running. Also S3 (real Claude) survived two window closes. Reopen with replay: awaiting Jonathan |
| C15 | Task 9 item 1 | Real Claude in mouse mode: click, Shift-drag, Cmd+C | Click goes to Claude, Shift-drag selects locally, Cmd+C copies the right text | awaiting Jonathan |
| C16 | Task 9 item 2 | Paste 3 lines into Claude's input | One paste, not three submissions | awaiting Jonathan |
| C17 | Task 9 item 3 | Cmd+Ctrl+Shift+4, then Cmd+V in Claude | `[Image #1]` | awaiting Jonathan |
| C18 | Task 9 item 4 | After a long answer: wheel scrollback, Cmd+F "error", Enter and Shift+Enter | Scrolls back; matches found, revealed and cycled | awaiting Jonathan |
| C19 | Task 9 item 5 | Cmd+click an http(s) URL in Claude output | Opens in the default browser | awaiting Jonathan |
| C20 | Task 9 item 6 | Cmd+click `crates/overseer/src/main.rs:1` in a slot whose cwd is the repo | VS Code opens at line 1 | awaiting Jonathan |
| C21 | Task 9 item 7 | Option+E E, Option+U U, Option+B in Claude's input | é, ü, and a word jump back | awaiting Jonathan |
| C22 | Task 9 item 8 | Trackpad scroll on the alt screen (Claude full-screen UI, or vim with mouse off) | Per R9: arrows with 1007, scrolls the app when mouse mode is on | awaiting Jonathan |
| C23 | Task 9 item 9 | Japanese or Chinese IME in Claude | Candidate window at the cursor; committing inserts the text | awaiting Jonathan |

## Sign-off

Phase 1 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by:
- Date:
