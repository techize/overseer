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

## Slot environment and launch flags

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| E1 | I5b: parent host variables are scrubbed (from inside a Claude session) | In a Claude session, start `overseer daemon` under a temp `OVERSEER_HOME`. `overseer ctl start 1 --cwd <dir> -- sh -c 'env > <file>; sleep 5'`. Read `<file>` | `CLAUDE_CODE_ACCOUNT_UUID`, `ANTHROPIC_BASE_URL`, `CLAUDECODE`, `CLAUDE_CODE_ORGANIZATION_UUID`, `CLAUDE_CODE_HOST_SESSION_ID`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_EXECPATH`, `GIT_EDITOR` and `MCP_SERVER_CONNECTION_BATCH_SIZE` are absent; `OVERSEER_SLOT=S1` is present | Verified 2026-11-10: all nine absent, `OVERSEER_SLOT=S1` present. Two `MCP_` names remain, `MCP_GATEWAY_URL` and `MCP_CONNECTION_NONBLOCKING`: Jonathan's own `~/.zshrc` exports them (the first through `~/.k3s-config`), and a clean `zsh -l -i` shows both. The daemon keeps what the login shell sets on purpose, so this is not a leak (F1) |
| E2 | The same check through the installed agent | `launchctl kickstart -k gui/$UID/uk.co.techize.overseer` from a Claude session, start a slot whose command is `sh -c 'env > /tmp/e; sleep 5'`, read `/tmp/e` | Same absences as E1 | awaiting Jonathan |
| F1 | Claude slots get the Overseer flags | Stub script named `claude` that logs argv. `overseer ctl start 2 --cwd <dir> --label flags -- /path/to/claude --dangerously-skip-permissions` | argv has `--session-id <uuid>`, `--name "S2 flags"`, `--settings <state>/slots/S2/settings.json`, `--append-system-prompt-file <state>/slots/S2/role.md`, after the user's own arguments | Verified 2026-11-10: `[--dangerously-skip-permissions] [--session-id] [00085e4a-...] [--name] [S2 flags] [--settings] [.../slots/S2/settings.json] [--append-system-prompt-file] [.../slots/S2/role.md]`. `settings.json` lists the five hooks as `'<overseer binary>' hook <Event>` with timeout 2. Ledger row `2\|00085e4a-...\|` has the same id |
| F2 | The flags are on the real claude process | Start a slot with the default command, `ps -o args= -p <pid>` | The same flags appear on the real `claude` | awaiting Jonathan |
| F3 | `--prompt` that starts with a dash | `overseer ctl start 3 --cwd <dir> --label dash --prompt "--help me" -- /path/to/claude` | Accepted by clap; `--` comes right before the prompt, which is the last argv element and a single argument | Verified 2026-11-10: argv ended `[--] [--help me]`, one element. Unit tests `dash_prompt_follows_double_dash` and `prompt_may_start_with_a_dash` cover the rest |
| F4 | Non-claude slot is unaffected | `ctl start 1 -- sh -c '...'` (E1's slot). `ctl status`, then look in the state dir | No `--settings` or any other Overseer flag; no `slots/S1` dir; card stays `Running` with no activity line | Verified 2026-11-10 for the daemon side: command ran as given, `ctl status` showed `S1  running ... scrub`, then `exited code 0`, and `slots/` had only S2 and S3. The card colour is awaiting Jonathan (A1, row 8) |

## Hooks and latency

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| H1 | Hook budget with the daemon running | 12 runs of `OVERSEER_SLOT=S1 overseer hook Stop </dev/null`, drop the first two, timed from the parent by `time.perf_counter` | Under 50 ms of hook work | Verified 2026-11-10: median 7.2 ms, max 7.9 ms over three batches of 10 (this is whole process wall time: exec, socket, send, exit; hook work itself is less). `/usr/bin/true` on the same Mac: median 2.7 ms |
| H2 | Hook budget with no daemon | As H1 after `ctl shutdown` | Under 50 ms, exit 0, prints nothing, does not start a daemon | Verified 2026-11-10: median 7.0 ms, max 7.6 ms, and 6.9 and 7.2 ms in a second batch. Exit 0 each time, and no socket appeared in the state dir afterwards |
| H3 | Hook outside a slot | `overseer hook Stop </dev/null` with `OVERSEER_SLOT` unset | Exits 0 at once, no output | Covered by `hook.rs` (returns when the variable is missing); not run by script |

## Activity on the board (real Claude, awaiting Jonathan)

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| A1 | Working, needs-you and idle cards | Start a claude slot labelled `notify test` in S3. Send "run ls in this folder": watch the card. Then a command that needs permission. Then let the turn finish | Working: green `● Working <time>`, green edge, tool on the line under the cwd, session panel rows Activity and Doing. Needs you: orange `● Needs you <time>`, orange edge, reason in orange, header `1 running, 1 need you` in orange, panel rows Activity and Reason. Idle: grey `● Idle <time>`, no detail line, header `1 running` | awaiting Jonathan |
| A2 | Non-claude card unchanged | Start `htop` (or any non-claude command) in another slot | Green `● Running <time>`, no detail line, no notification | awaiting Jonathan |
| A3 | GUI restart keeps state, no repeat notification | Leave S3 on a permission prompt, Cmd+Q the GUI, reopen it | Card shows Needs you; no new notification (first-sync rule) | awaiting Jonathan |

## Notifications (awaiting Jonathan)

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| N1 | Authorization in the ad hoc signed bundle | First launch of `~/Applications/Overseer.app` from Finder after `make install`. Then System Settings, Notifications, Overseer | macOS asks to allow notifications; Alerts are allowed afterwards. Record whether the prompt appeared for an ad hoc signed bundle. If it does not, the board still works but there are no banners | awaiting Jonathan |
| N2 | A notification fires once on needs-you | As A1, make S3 ask for permission | Exactly one banner, title `S3 needs you`, body `notify test: <reason>`, with sound; none when the turn ends and none when only the reason changes | awaiting Jonathan |
| N3 | Click opens the session while the app is running | With another app frontmost and Overseer open on the board, cause a permission request and click the banner | Overseer comes forward on the S3 session view | awaiting Jonathan |
| N4 | Click from a cold launch (known gap) | Quit Overseer, click an old notification in Notification Center | The app launches. It may open on the board, not S3, because the delegate is installed inside the launch callback | awaiting Jonathan. Known limitation L2 |
| N5 | Denied notifications | Turn notifications off for Overseer, repeat N2 | No banner; card and header still show Needs you; the GUI log may show `notification for S3 not delivered` | awaiting Jonathan |

## Resume and session ledger

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| R1 | A daemon restart offers resume and resume continues the same id | Stub `claude` in S2 (and S3). `ctl shutdown`, start the daemon again, `ctl status`, then `ctl resume 2` | `status` shows `S2  resumable`; after resume the stub gets `--resume <same id>`, no `--session-id`, same cwd and name; the ledger row for S2 is unchanged and live (exited NULL) | Verified 2026-11-10: after the restart `S2  resumable` and `S3  resumable`. `ctl resume 2` started the stub with `[--resume] [00085e4a-...]`, the id the first run used, `--name` `S2 flags`; `sessions` table had S2 with the same id and an empty `exited` throughout |
| R2 | A resumed session gets the Overseer flags | Same run as R1, read the stub's argv | `--settings`, `--append-system-prompt-file` and `--name` are present next to `--resume` | Verified 2026-11-10: `[--dangerously-skip-permissions] [--resume] [<id>] [--name] [S2 flags] [--settings] [.../settings.json] [--append-system-prompt-file] [.../role.md]` |
| R3 | `ctl resume` on a slot that is not resumable | `ctl resume 5` | Clear error, nothing starts | Verified 2026-11-10: `overseer: S5 is not resumable` |
| R4 | Stop on a resumable slot drops the offer | `ctl stop 3` while S3 is resumable, then `ctl status` | S3 shows empty and a second restart does not offer it again | Partly verified 2026-11-10: `ctl status` showed `S3  empty` after the stop. The second restart was not run |
| R5 | Real resume with the installed agent | Real claude in S2, talk to it, `launchctl kickstart -k gui/$UID/uk.co.techize.overseer`, `overseer ctl status`, then Resume on the card (or `r`, or `ctl resume 2`) | Card is a grey `Resumable` with a Resume button; resuming shows the earlier conversation in S2 | awaiting Jonathan |
| R6 | `/clear` then restart resumes the new conversation | In a real claude slot run `/clear`, send a message, restart the daemon as R5, resume | The slot resumes the post-`/clear` conversation (the SessionStart hook updated the ledger id), not the first one | awaiting Jonathan |

## S0 coordinator

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| S1 | A fresh S0 gets the coordinator role and the kickoff | `ctl start 0 --cwd <dir> -- /path/to/claude` with a stub, then read `slots/S0/role.md` | argv ends `-- <kickoff text>`; `role.md` is the coordinator role (has `bd ready --json`, starts "You are S0, the coordinator"); other slots get the worker role | Verified 2026-11-10: argv ended `[--] [Run the start-of-day round: check beads and Jira, check the board, and propose who works on what.]`; S0 `role.md` begins `You are S0, the coordinator of an Overseer team` and has `bd ready --json`, S2's has none; `roles/` holds coordinator.md, kickoff.md and worker.md |
| S2 | A resumed S0 does not rerun the kickoff | Restart the daemon after S1, `ctl resume 0`, read the stub's argv | `--resume <same id>`, no positional prompt | Verified 2026-11-10: `[--resume] [1f89a5ed-...] [--name] [S0 s0] [--settings] ... [--append-system-prompt-file] ...`, nothing after the role path, and the id matched S1's `--session-id` |
| S3 | The round proposes a table and nothing starts until Jonathan says yes | Start S0 with the real claude command and no prompt. Watch the round (`bd ready --json` in `~/Repos/2ndBrain`, `command -v acli`, `overseer ctl status`). Do not answer yet, check `ctl status` | A table with columns slot, work item, folder, first prompt, then the items left for later. No other slot starts before Jonathan replies | awaiting Jonathan |
| S4 | An approved row starts its slot with its prompt | Approve one row | The slot starts in the stated folder with the stated label and shows the first prompt at launch. Also `overseer ctl send 0 "Run another round"` and `overseer ctl start 3 --cwd /tmp --label t --prompt "hi"` work | awaiting Jonathan |
| S5 | No `acli` installed | On a machine (or PATH) without `acli`, let S0 run the round | S0 says in the proposal that `acli` is missing and continues with beads only. Note: this Mac has `bd` at `/opt/homebrew/bin/bd` and no `acli`, so the plain run covers it | awaiting Jonathan |
| S6 | Resumed S0 in the real app | Restart the agent as R5 with S0 running, resume S0 | The conversation continues and no second kickoff round starts | awaiting Jonathan |

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
  installed on this Mac. The role text handles that (S5) but only a real session
  proves it.
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

## Sign-off

Phase 2 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by:
- Date:
