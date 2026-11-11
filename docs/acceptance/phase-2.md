# Phase 2 acceptance

Phase 2 is done only when every row below passes. Scriptable rows were run
against a daemon started from the freshly built release binary under a temp
`OVERSEER_HOME`. Hand rows need a real Claude session on the installed app
(`make install`, then `~/Applications/Overseer.app`).

The Result column says what was actually observed and how. "awaiting Jonathan"
means nobody has checked it by hand yet. Unit tests cover logic only and do not
replace a hand check where a row asks for one.

Checks marked "verified 2026-11-10" were run on home.local by script: a daemon
started with `overseer daemon` from `target/release/overseer` at 9565dcf, with
`OVERSEER_HOME` set to a temp dir (`/tmp/ovacc.*`, short because a unix socket
path is limited to 104 characters), run from inside a Claude Code session so
`CLAUDECODE` and the host variables were in the environment. The installed
app, its LaunchAgent (`uk.co.techize.overseer`) and its socket were not
touched. Where a row needs `claude`, a stub script named `claude` logged its
argv and slept. The temp daemon was stopped with `ctl shutdown` and the temp
dir removed afterwards.

## Setup for the hand checks

1. `make install` in `~/overseer`.
2. Open Overseer from `~/Applications` in Finder.
3. Have a claude session you can drive for a few minutes (a real login).

The rows marked "daemon side, real claude" were run on 2026-11-10 against a
temp daemon from `target/release/overseer` at 2a5a83f under a temp
`OVERSEER_HOME`, with Jonathan's real `claude` login, so the installed app
and its running S1 were not touched. Slot state was read over the socket with
a small client (`ListSlots` and `SlotChanged` events) and the screen through
`overseer attach`. The real sessions it created (in `~/overseer` and
`~/Repos/2ndBrain`) remain in Claude's own history.

## Slot environment and launch flags

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| E1 | I5b: parent host variables are scrubbed (from inside a Claude session) | In a Claude session, start `overseer daemon` under a temp `OVERSEER_HOME`. `overseer ctl start 1 --cwd <dir> -- sh -c 'env > <file>; sleep 5'`. Read `<file>` | `CLAUDE_CODE_ACCOUNT_UUID`, `ANTHROPIC_BASE_URL`, `CLAUDECODE`, `CLAUDE_CODE_ORGANIZATION_UUID`, `CLAUDE_CODE_HOST_SESSION_ID`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_EXECPATH`, `GIT_EDITOR` and `MCP_SERVER_CONNECTION_BATCH_SIZE` are absent; `OVERSEER_SLOT=S1` is present | Verified 2026-11-10: all nine absent, `OVERSEER_SLOT=S1` present. Two `MCP_` names remain, `MCP_GATEWAY_URL` and `MCP_CONNECTION_NONBLOCKING`: Jonathan's own `~/.zshrc` exports them (the first through `~/.k3s-config`), and a clean `zsh -l -i` shows both. The daemon keeps what the login shell sets on purpose, so this is not a leak (F1) |
| E2 | The same check through the installed agent | in Terminal (not as a slot command, where `$(...)` is not expanded) `launchctl kickstart -k gui/$(id -u)/uk.co.techize.overseer` from a Claude session, start a slot whose command is `sh -c 'env > /tmp/e; sleep 5'`, read `/tmp/e` | Same absences as E1 | Passed 2026-11-11 (Jonathan) |
| F1 | Claude slots get the Overseer flags | Stub script named `claude` that logs argv. `overseer ctl start 2 --cwd <dir> --label flags -- /path/to/claude --dangerously-skip-permissions`, then `cat <state>/slots/S2/settings.json` | argv has `--session-id <uuid>`, `--name "S2 flags"`, `--settings <state>/slots/S2/settings.json`, `--append-system-prompt-file <state>/slots/S2/role.md`, after the user's own arguments; settings.json has the five hooks | Verified 2026-11-10: `[--dangerously-skip-permissions] [--session-id] [00085e4a-...] [--name] [S2 flags] [--settings] [.../slots/S2/settings.json] [--append-system-prompt-file] [.../slots/S2/role.md]`. Ledger row `2\|00085e4a-...\|` has the same id. `settings.json` (cat, rerun on a second run) has `hooks` keys SessionStart, UserPromptSubmit, Notification, Stop and PostToolUse (the last with `"matcher": "*"`), each `{"type": "command", "command": "'<overseer binary>' hook <Event>", "timeout": 2}`; the binary path is quoted. The slot dir holds `role.md` and `settings.json`; `<state>/roles/` holds coordinator.md, kickoff.md and worker.md |
| F2 | The flags are on the real claude process | Start a slot with the default command, `ps -o args= -p <pid>` | The same flags appear on the real `claude` | Verified 2026-11-10 (daemon side, real claude, temp daemon from `target/release` at 2a5a83f, see the note below the setup): `ps -o args=` on the real claude in S2 showed `claude --session-id 421277d6-... --name S2 real --settings <state>/slots/S2/settings.json --append-system-prompt-file <state>/slots/S2/role.md -- <prompt>`, each flag once; the ledger command was `["claude"]` |
| F3 | `--prompt` that starts with a dash | `overseer ctl start 3 --cwd <dir> --label dash --prompt "--help me" -- /path/to/claude` | Accepted by clap; `--` comes right before the prompt, which is the last argv element and a single argument | Verified 2026-11-10: argv ended `[--] [--help me]`, one element. Unit tests `dash_prompt_follows_double_dash` and `prompt_may_start_with_a_dash` cover the rest |
| F4 | Non-claude slot is unaffected | `ctl start 1 -- sh -c '...'` (E1's slot). `ctl status`, then look in the state dir | No `--settings` or any other Overseer flag; no `slots/S1` dir; card stays `Running` with no activity line | Verified 2026-11-10 for the daemon side: command ran as given, `ctl status` showed `S1  running ... scrub`, then `exited code 0`, and `slots/` had only S2 and S3. The card colour is awaiting Jonathan (A2) |
| F5 | `--settings` merges with Jonathan's own settings and hooks | In a real claude slot started from the installed app, run `/hooks`. Then send a short message and let the turn finish. Then check `/model` and `/permissions` (or try a command Jonathan's own settings pre-approve) | `/hooks` lists Jonathan's global hooks (from `~/.claude/settings.json`) and his plugins' hooks. It does not list hooks passed with `--settings`, so Overseer's five are not shown there; that they run is shown by the card changing state and by the turn's "Ran N stop hooks" count being one more than his own Stop hooks. At the end of the turn one of Jonathan's own hooks fired (its usual output or side effect) and the card went to Idle. Jonathan's model and permission rules still apply. Note: a managed (enterprise) setting `allowManagedHooksOnly` would turn off every non-managed hook, Overseer's included, and the cards would never leave Running | Verified 2026-11-10 (daemon side, real claude, temp daemon from `target/release` at 2a5a83f, see the note below the setup): in a real slot in `~/overseer` the turn ended with "Ran 4 stop hooks" (Jonathan's three Stop hooks plus Overseer's one), Jonathan's `link-current-transcript.sh` hook repointed `~/.claude/CURRENT_TRANSCRIPT.jsonl` at the slot's transcript, the daemon saw Working then Idle, and the slot ran Sonnet 5 in auto mode with Jonathan's status line. `/hooks` itself was not opened: awaiting Jonathan for that listing |

## Hooks and latency

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| H1 | Hook budget with the daemon running | A Python script runs `OVERSEER_SLOT=S1 overseer hook Stop </dev/null` as a child process 12 times in a row, takes the wall time of each run from the parent with `time.perf_counter` (exec to exit), drops the first two runs as warm-up and reports median and max of the other 10 | Under 50 ms of hook work | Verified 2026-11-10: median 7.4, 7.3 and 7.2 ms, max 7.6, 7.9 and 7.8 ms in three batches of 10 (process wall time: exec, socket, send, exit; the hook work itself is less). `/usr/bin/true` timed the same way: median 2.7 ms, max 3.0 ms |
| H2 | Hook budget with no daemon | As H1 after `ctl shutdown`. Then one run with stdout and stderr captured, ten more checking the exit status, and `ls <state>` | Under 50 ms, exit 0, prints nothing, does not start a daemon | Verified 2026-11-10: median 7.0 ms and 6.9 ms, max 7.6 ms and 7.2 ms in two batches of 10 (timed as H1). Rerun 2026-11-10: exit=0, 0 bytes on stdout and stderr, then ten runs with no non-zero exit; `ls <state>` before and after listed logs, overseer.db, roles and slots, with no `overseer.sock` |
| H3 | Hook outside a slot | `overseer hook Stop </dev/null` with `OVERSEER_SLOT` unset | Exits 0 at once, no output | Covered by `hook.rs` (returns when the variable is missing); not run by script |

## Activity on the board (real Claude, awaiting Jonathan)

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| A1 | Working, needs-you and idle cards | Start a claude slot labelled `notify test` in S3. Send "run ls in this folder": watch the card. Then press Shift+Tab until the mode line no longer says auto mode or accept edits (auto mode approves commands itself, so nothing asks), and ask for a Bash command that is not pre-approved, for example "run `touch /tmp/overseer-perm-test` with Bash". Then let the turn finish | Working: green `● Working <time>`, green edge, tool on the line under the cwd, session panel rows Activity and Doing. Needs you: orange `● Needs you <time>`, orange edge, reason in orange, header `1 running, 1 need you` in orange, panel rows Activity and Reason. Idle: grey `● Idle <time>`, no detail line, header `1 running`. Check Idle within a minute of the turn ending (L6), and end the turn normally, not with Esc (L5) | Verified 2026-11-10 (daemon side, real claude, temp daemon from `target/release` at 2a5a83f, see the note below the setup): over the socket S2 went `Working` with `now_doing: null`, then `Working` with `now_doing: "Bash ls"`, then `Idle` 4 s later; about 60 s after a finished turn it went `NeedsYou` with reason `Claude is waiting for your input` (L6). The permission case and the card colours are awaiting Jonathan |
| A2 | Non-claude card unchanged | Start `htop` (or any non-claude command) in another slot | Green `● Running <time>`, the command (`htop`) on the line under the cwd, no notification for this slot | 2026-11-11, Jonathan: green Running, but no command shown (added since, recheck) and a banner arrived around the same time; it was most likely for S1, a new claude slot going idle, not for htop, since a slot without activity cannot notify. Recheck after the fix Passed 2026-11-11 on the 11:35 build: green, `htop` shown under the folder, no banner for S2. |
| A3 | GUI restart keeps state, no repeat notification | Leave S3 on a permission prompt, Cmd+Q the GUI, reopen it | Card shows Needs you; no new notification (first-sync rule) | Passed 2026-11-11 (Jonathan) |

## Notifications (awaiting Jonathan)

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| N1 | Authorization in the ad hoc signed bundle | First launch of `~/Applications/Overseer.app` from Finder after `make install`. Then System Settings, Notifications, Overseer | macOS asks to allow notifications; Alerts are allowed afterwards. Record whether the prompt appeared for an ad hoc signed bundle. If it does not, the board still works but there are no banners | Passed 2026-11-11 after a fix (F8): the first launch was refused. With the stale Trash copy unregistered and the app opened through Finder, the ad hoc signed bundle can post banners |
| N2 | A notification fires once on needs-you | As A1, make S3 ask for permission | Exactly one banner, title `S3 needs you`, body `notify test: <reason>`, with sound; none when the turn ends and none when only the reason changes. The idle Notification about 60 s after a finished turn is a separate needs-you with its own banner (L6), so judge "none when the turn ends" within that minute | Failed 2026-11-11, then fixed: only the first banner per slot ever showed. Each post reused the identifier `overseer-slot-<n>`, and macOS updates a delivered notification with the same identifier in place without a banner. Each post now gets a new identifier and the slot's previous one is removed first. Recheck after `make install` Passed 2026-11-11 on the 11:35 build: one banner for the permission prompt, not repeated. |
| N3 | Click opens the session while the app is running | With another app frontmost and Overseer open on the board, cause a permission request and click the banner | Overseer comes forward on the S3 session view | Not testable 2026-11-11 (no banner, N2). Recheck after the N2 fix Passed 2026-11-11: clicking the banner opened S3. |
| N4 | Click from a cold launch (known gap) | Quit Overseer, click an old notification in Notification Center | The app launches. It may open on the board, not S3, because the delegate is installed inside the launch callback | awaiting Jonathan. Known limitation L2 Passed 2026-11-11: with Overseer quit, clicking an old notification launched it on the board. Jonathan is happy with the board here. |
| N5 | Denied notifications | Turn notifications off for Overseer, repeat N2 | No banner; card and header still show Needs you; the GUI log may show `notification for S3 not delivered` | 2026-11-11: no banner with notifications off; whether the card still showed Needs you was not recorded. Recheck after the N2 fix, since no banner had shown with them on either Passed 2026-11-11 on the 11:35 build: no banner with notifications off, and the card still said Needs you. |

## Resume and session ledger

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| R1 | A daemon restart offers resume and resume continues the same id | Stub `claude` in S2 (and S3). `ctl shutdown`, start the daemon again, `ctl status`, then `ctl resume 2` | `status` shows `S2  resumable`; after resume the stub gets `--resume <same id>`, no `--session-id`, same cwd and name; the ledger row for S2 is unchanged and live (exited NULL) | Verified 2026-11-10: after the restart `S2  resumable` and `S3  resumable`. `ctl resume 2` started the stub with `[--resume] [00085e4a-...]`, the id the first run used, `--name` `S2 flags`; `ctl status` showed the same cwd for S2 before and after; `sessions` table had S2 with the same id and an empty `exited` throughout |
| R2 | A resumed session gets the Overseer flags | Same run as R1, read the stub's argv | `--settings`, `--append-system-prompt-file` and `--name` are present next to `--resume` | Verified 2026-11-10: `[--dangerously-skip-permissions] [--resume] [<id>] [--name] [S2 flags] [--settings] [.../settings.json] [--append-system-prompt-file] [.../role.md]` |
| R3 | `ctl resume` on a slot that is not resumable | `ctl resume 5` | Clear error, nothing starts | Verified 2026-11-10: `overseer: S5 is not resumable` |
| R4 | Stop on a resumable slot drops the offer | Stub `claude` in S3, `ctl stop 3`, `ctl status`, then `ctl shutdown` and start the daemon again, twice, `ctl status` after each | S3 is empty after each restart and never `resumable` | Verified 2026-11-10: `ctl status` right after the stop showed `S3  exited` (the stop was of a running slot), the ledger row `3\|9002d721-...\|1794315758` had a non-empty exited time, and after restart 1 and restart 2 `S3  empty`. A slot stopped while it was already resumable was only checked by `ctl status` showing `S3  empty` on the first run, and by the server test `start_and_stop_on_a_resumable_slot` |
| R5 | Real resume with the installed agent | Real claude in S2, talk to it, in Terminal (not as a slot command, where `$(...)` is not expanded) `launchctl kickstart -k gui/$(id -u)/uk.co.techize.overseer`, `overseer ctl status`, quit and reopen Overseer (the window does not reconnect, F9), then Resume on the card (or `r`, or `ctl resume 2`) | Card is a grey `Resumable` with a Resume button; Resume on the card and `r` both open S2's session screen, and resuming shows the earlier conversation in S2 with its terminal focused | 2026-11-11, partial. The first try was entered as a slot command, where `$UID` is not expanded, and launchctl rejected the target; the step now says Terminal and `$(id -u)`. Run again from Terminal: the daemon restarted cleanly three times, S1 and S3 (claude) came back `Resumable` with their session ids, and htop in S2 came back `Exited`. Resume needed the window reopened first (F9). S3 then resumed its earlier conversation. S1 exited at once with code 1, because it was started and never used, so claude had saved nothing and `--resume` said "No conversation found" (F10, fixed). `r` on S1 was not seen to work; Jonathan used the button. Retest S1 and `r` after `make install` Passed 2026-11-11 on the 11:35 build: after a kickstart and reopening the window, S3 resumed its earlier conversation, and an unused S1 resumed with `r` on the board and stayed running (F10 fix). |
| R6 | `/clear` then restart resumes the new conversation | In a real claude slot run `/clear`, send a message, restart the daemon as R5, resume | The slot resumes the post-`/clear` conversation (the SessionStart hook updated the ledger id), not the first one | Verified 2026-11-10 (daemon side, real claude, temp daemon from `target/release` at 2a5a83f, see the note below the setup): after `/clear` the ledger id for S2 changed from `bf41724c-...` to `04d9e500-...`; after a restart `ctl resume 2` used `--resume 04d9e500-...` and the session recalled a codeword given only after the `/clear` |

## S0 coordinator

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| K1 | A fresh S0 gets the coordinator role and the kickoff | `ctl start 0 --cwd <dir> -- /path/to/claude` with a stub, then read `slots/S0/role.md` | argv ends `-- <kickoff text>`; `role.md` is the coordinator role (has `bd ready --json`, starts "You are S0, the coordinator"); other slots get the worker role | Verified 2026-11-10: argv ended `[--] [Run the start-of-day round: check beads and Jira, check the board, and propose who works on what.]`; S0 `role.md` begins `You are S0, the coordinator of an Overseer team` and has `bd ready --json`, S2's has none; `roles/` holds coordinator.md, kickoff.md and worker.md |
| K2 | A resumed S0 does not rerun the kickoff | Restart the daemon after K1, `ctl resume 0`, read the stub's argv | `--resume <same id>`, no positional prompt | Verified 2026-11-10: `[--resume] [1f89a5ed-...] [--name] [S0 s0] [--settings] ... [--append-system-prompt-file] ...`, nothing after the role path, and the id matched K1's `--session-id` |
| K3 | The round proposes a table and nothing starts until Jonathan says yes | On the installed app. Open Overseer from `~/Applications` (the binary is `~/Applications/Overseer.app/Contents/MacOS/overseer`, linked as `~/.local/bin/overseer`; there is no copy in `/Applications`). Board, `+ start` on S0, folder `~/Repos/2ndBrain`, label `coordinator`, command left as the default (`claude`), prompt empty. The same from a shell: `overseer ctl start 0 --cwd ~/Repos/2ndBrain --label coordinator`. Watch S0 (click its card) and run `overseer ctl status` while it works and again after it stops | S0 runs `bd ready --json` in `~/Repos/2ndBrain`, checks `command -v acli`, runs `overseer ctl status`, then prints a markdown table with columns slot, work item, folder, first prompt (one row per idle slot) and a list of items left for later with reasons. After the table it waits: `ctl status` shows S1 to S6 unchanged (no new running slot) until Jonathan types yes | Verified 2026-11-10 (daemon side, real claude, temp daemon from `target/release` at 2a5a83f, see the note below the setup): `ctl start 0 --cwd ~/Repos/2ndBrain --label coordinator` launched with the kickoff after `--`. S0 ran `bd ready --json`, `command -v acli` and `"<overseer>" ctl status`, then printed a table with columns slot, work item, folder, first prompt (one row per idle slot) and a "Left for later, and why" list, and ended "Waiting for your go-ahead before starting anything." `ctl status` afterwards showed every other slot unchanged. The GUI start route is awaiting Jonathan The installed app passed 2026-11-11: S0 proposed a table and started nothing. Jonathan asked for the label to default to `coordinator` on S0, which the start dialog now does. |
| K4 | An approved row starts its slot with its prompt | Approve one row | The slot starts in the stated folder with the stated label and shows the first prompt at launch. Also `overseer ctl send 0 "Run another round"` and `overseer ctl start 3 --cwd /tmp --label t --prompt "hi"` work | Passed 2026-11-11 on the installed app. Jonathan told S0 to start one row; S0 ran `ctl start 5` with the folder, label and full first prompt from its table, and S5 started there with that label. S5 then exited after 34 s with code 1 and saved no conversation, because the folder had not been trusted in Claude: the slot opened on the folder-trust prompt (F6), and leaving it exits claude. S0 also resumed S3 on request with `ctl resume 3` |
| K5 | No `acli` installed | On a machine (or PATH) without `acli`, let S0 run the round | S0 says in the proposal that `acli` is missing and continues with beads only. Note: this Mac has `bd` at `/opt/homebrew/bin/bd` and no `acli`, so the plain run covers it | Verified 2026-11-10 (daemon side, real claude, temp daemon from `target/release` at 2a5a83f, see the note below the setup): S0 said "acli isn't installed, so this round is beads-only" and went on with beads |
| K6 | Resumed S0 in the real app | Restart the agent as R5 with S0 running, resume S0 | The conversation continues and no second kickoff round starts | Verified 2026-11-10 (daemon side, real claude, temp daemon from `target/release` at 2a5a83f, see the note below the setup): after a restart `ctl resume 0` relaunched `--resume b0e3f491-...` with no positional prompt; the transcript held one kickoff prompt before and after, and S0 went Idle. The GUI side is awaiting Jonathan The GUI side passed 2026-11-11 with R5. |

## Findings from the 2026-11-10 run

- **F1 (not a failure, wording of the ruling): `MCP_*` is not fully absent from a
  slot.** `MCP_GATEWAY_URL` and `MCP_CONNECTION_NONBLOCKING` are in the slot
  environment, but they come from Jonathan's `~/.zshrc` (`~/.k3s-config` and an
  `export` on line 41), not from the parent session; the parent's
  `MCP_SERVER_CONNECTION_BATCH_SIZE` was removed. This is the designed rule
  ("values the user's rc files set survive" in `env.rs`). If the check should
  mean "no `MCP_` variable at all", say so and the rule changes; as written the
  scrub works.
- **F2 (note): hook latency is process wall time.** 7 ms median, 7.9 ms max with
  the daemon up and 7.6 ms max with none, so the budget of 50 ms has plenty of
  room. A debug build was not measured.
- **F3 (note): the S0 round needs `acli` or `bd` at run time.** `acli` is not
  installed on this Mac. The role text handles that (K5) but only a real session
  proves it.
- **F5 (finding): the dead `gsd-*` hooks fail in every slot.** Each slot shows
  SessionStart and Stop hook errors from `/Users/jonathan.gill/.claude/hooks/gsd-*`,
  which do not exist on this Mac. They come from Jonathan's own settings, not
  from Overseer, and go away once those entries are removed.
- **F6 (finding): a slot in a folder Claude has not trusted waits on the trust
  prompt.** Started in a new folder, claude sat on "Is this a project you
  created or one you trust?" and no hook fired, so the card stays plain
  Running. Answer it in the session; Overseer cannot see it.
- **F7 (finding): `/hooks` does not show Overseer's hooks.** Checked
  2026-11-10 in a real slot: `/hooks` said 27 hooks configured, all marked
  `[User]` or from plugins, and PostToolUse listed only Jonathan's three. In
  the same slot a turn still took the card through Working, `Bash pwd` and
  Idle, so the `--settings` hooks run; Claude Code just leaves them out of
  that menu. F5 now checks the card instead.
- **N1 (observed): notifications were not allowed on the first launch.** The
  GUI log on 2026-11-10 at 16:43 says `notifications not allowed:
  Notifications are not allowed for this application`. Whether macOS showed
  a prompt is for Jonathan to record.
- **F8 (finding): notifications were refused while an old copy sat in the
  Trash.** On 2026-11-10 macOS refused Overseer's notification request and
  listed no entry for it. LaunchServices knew two bundles with the id
  `uk.co.techize.overseer`, the installed one and an old one in `~/.Trash`.
  After the Trash copy was deleted, its registration removed with
  `lsregister -u`, and the window reopened through Finder (a Cmd+Q had left
  the old window process running), banners appeared. If notifications fail
  after a reinstall, check `lsregister -dump | grep Overseer.app` for
  duplicates.
- **F9 (finding): the window does not reconnect after a daemon restart.**
  After the R5 kickstart the board kept the old slot states and showed
  "Lost the connection to the daemon. Quit and reopen Overseer to
  reconnect." Resume did nothing until the window was reopened. Reconnecting
  on its own is a follow-up. A `Resumable` card's "Started" line also shows
  the first start (18h earlier in this run), not the last time it ran.
- **F10 (fixed): resuming a session that was never used failed.** Claude
  saves a conversation only once it has a message, so `claude --resume <id>`
  on an unused session exits with code 1 and "No conversation found". The
  daemon now looks for `<id>.jsonl` under Claude's projects folder
  (`$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`) and, when there
  is none, starts the slot fresh with `--session-id <id>` instead. Covered by
  `resume_without_a_saved_conversation_starts_fresh_under_the_same_id`.
- **Environment note:** the temp state dir had to be under `/tmp` and not the
  session scratch directory, because the unix socket path would have been 112
  characters (the limit is 104).

## Known limitations from the reviews

- **L1: a stale hook event from a killed run can land on the next run of the
  same slot.** Hooks are matched by slot, so a late `Stop` or `Notification`
  from a process that was just killed can set the new session's activity for a
  moment until its own events arrive.
- **L2: a cold-launch notification click opens the board.** If Overseer is not
  running and Jonathan clicks an old notification, the click can arrive before
  the delegate is installed, so the app opens on the board and not on the
  session. Clicks while the app is running open the session.
- **L3: banners show even when Overseer is frontmost,** including for the
  session being looked at (`willPresentNotification` returns banner, list and
  sound). A later change could hide the banner for the visible session.
- **L4: the GUI start dialog has no prompt field,** so a GUI start or restart of
  S0 always gets the kickoff; `ctl start --prompt` is the only way to give a
  first prompt.
- **L5: an interrupted turn stays Working for about a minute.** Claude Code
  does not fire `Stop` when Jonathan interrupts a turn with Esc, so the card
  stays `Working` until Claude's idle `Notification` arrives about 60 s later.
  That turns it `Needs you` and posts a banner, although nothing is waiting on
  a permission.
- **L6: every finished turn becomes Needs you after about a minute.** Claude
  Code sends its idle `Notification` ("waiting for your input") about 60 s
  after any turn ends, so an `Idle` card turns `Needs you` and posts a banner
  if nobody has typed in the meantime.

## Sign-off

Phase 2 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by: Jonathan Gill
- Date: Wednesday 11 November 2026
- Open follow-ups: F9 (the window does not reconnect after a daemon restart), L1 to L6 above, and the folder-trust prompt blocking a new slot (F6)
