# Phase 3 acceptance

Phase 3 is done only when every row below passes. Scriptable rows were run
against a daemon started from the freshly built release binary under a temp
`OVERSEER_HOME`. Hand rows need the GUI, a real Claude session or real Jira on
the installed app (`make install`, then `~/Applications/Overseer.app`).

The Result column says what was actually observed and how. "awaiting Jonathan"
means nobody has checked it by hand yet. Unit tests cover logic only and do not
replace a hand check where a row asks for one.

Checks marked "verified 2026-11-11" were run on home.local by script: a daemon
started with `overseer daemon` from `target/release/overseer` at d30f02c, with
`OVERSEER_HOME` set to a temp dir (`/tmp/ov3acc.*`, short because a unix socket
path is limited to 104 characters) and `OVERSEER_CONFIG` pointing at a temp
`config.toml`. The installed app, its LaunchAgent (`uk.co.techize.overseer`),
its socket, `~/Library/Application Support/overseer` and
`~/.claude/settings.json` were not touched. Every row that writes used stub
scripts: a stub `bd` that keeps labels in a file and logs its argv, a stub Jira
command that can print good output, junk, a warning line before the JSON, or
exit non-zero, and a stub `claude` that logs argv and sleeps. The daemon finds
its sources through the login shell, so the runs used a temp `HOME` with a
`.profile` that puts the stub folder on `PATH`, and `SHELL=/bin/sh`. The real
`bd update` was never run. The one row that reads the real beads
(`bd list --json` through the daemon, nothing else) records counts only. The
temp daemons were stopped with `ctl shutdown` and the temp dirs removed
afterwards. The examples in this file (`DEMO-12`, `ov-3`) are invented.

## Setup for the hand checks

1. `make install` in `~/overseer`.
2. Write `~/.config/overseer/config.toml` only if the defaults do not suit
   (beads and brain folders default to `~/Repos/2ndBrain`). Then run
   `launchctl kickstart -k gui/$(id -u)/uk.co.techize.overseer` so the daemon
   rereads it.
3. Have a real claude login, and for the Jira rows `acli` installed and logged
   in. Until then the Jira source says `acli not found`, which is a normal
   state (J1).
4. Pick a throwaway bead in a test beads folder for the label rows, so the
   real beads database is not written while testing.

## Config and sources

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| C1 | `config.toml` is read, and a bad one falls back | Temp `OVERSEER_CONFIG` with `beads_dir`, `brain_dir`, `jira_command` and `ticket_pattern` set, start the daemon, `ctl items`. Then replace the file with broken TOML and restart | The values in the file are used (the stub sources were found). Broken TOML starts the daemon on the defaults and logs one warning with the parse error; a missing file starts on the defaults with no warning | Verified 2026-11-11 at d30f02c: the stub beads and Jira loaded from the configured paths with `ticket_pattern = "DEMO-[0-9]+"`. Broken TOML gave `config not loaded, using defaults: <path>: TOML parse error at line 1, column 13` in `daemon.log` and `ctl status` still answered. A missing file added no warning |
| J1 | Jira missing on this Mac | Daemon with the default `jira_command` (`acli ...`) on a `PATH` without `acli`, `ctl refresh`, `ctl items` | The Jira source line says `acli not found`; beads still load; the daemon does not error or exit | Verified 2026-11-11: `jira  acli not found` in `ctl items` and `ok: false, count 0, message "acli not found"` in `--json`; the beads source was `ok` in the same refresh |
| J2 | Jira output that is junk, has a warning line first, or fails (Review Focus 1) | Stub Jira command in each mode, `ctl refresh`, `ctl items` | Junk: the source says why (no JSON found). A warning line before the JSON: tickets still load. Non-zero exit: the source shows the exit code and stderr text. Beads load in all cases, nothing panics | Verified 2026-11-11: junk gave `jira  jira output: no JSON found`; the warning line before the JSON gave `jira  ok  2 items`; exit 3 gave `jira  <stub> exited with 3: auth expired`. Beads were `ok  5 items` each time and the daemon kept answering |
| B1 | Beads load from `~/Repos/2ndBrain` | Temp daemon with `beads_dir` set to `~/Repos/2ndBrain`, no assignments, `ctl refresh`, `ctl items --json`; record counts only | The beads source is `ok` with a count; beads that name a ticket are grouped under it | Verified 2026-11-11 (read-only `bd list --json` through the daemon, no write command run): beads source `ok`, 50 beads; 13 ticket nodes (Jira was missing, so they are made from the beads); 15 beads grouped under a ticket. The 50 was `bd list`'s default limit, not the real count (133); fixed after the run: the daemon now runs `bd list --json --limit 0`. Recount awaiting Jonathan |
| B2 | A slow `bd` does not stall the daemon (Review Focus 2) | Stub `bd list` that sleeps 100 s. `ctl refresh` in the background, and meanwhile `ctl status`, `ctl start`, `ctl items` and an `overseer hook Stop` | Each of those answers at once (milliseconds); the refresh ends at about 30 s with `beads  timed out after 30 s`, the hung `bd` is killed, and a later refresh works once the stub is fast again | Verified 2026-11-11: `ctl status`, `ctl start 1 -- sleep 600`, `ctl items` and the hook (5.5 ms) all answered in under 20 ms while the refresh waited. `ctl refresh` returned after 30 s; no `sleep 100` process was left; after removing the delay the next refresh read 5 items. One earlier refresh queued behind another in flight took 38 s (observation, see F2) |

## Grouping

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| G1 | Beads group under their ticket | Stub beads `DEMO-12 wire the widget` and `DEMO-12 test the widget`, stub Jira ticket `DEMO-12`, `ctl items` | Tickets are listed first, with their beads indented under them; a bead with no key sits at the top level | Verified 2026-11-11: `DEMO-12  In Progress  -  Widget wiring` followed by `  ov-3` and `  ov-4`; `ov-5  open  -  Standalone chore` at the top level |
| G2 | Ticket keys inside other text (Review Focus 5) | Beads titled `See also DEMO-7 and DEMO-12`, `Mentions DEMO-123 only`, with Jira tickets `DEMO-12` and `DEMO-123` | The first key in the text wins (`DEMO-7`); `DEMO-123` is never read as `DEMO-12`; a key with no Jira ticket still gets a node titled with the key alone | Verified 2026-11-11: the first bead went under `DEMO-7  unknown  -  DEMO-7`, the second under `DEMO-123`, neither under `DEMO-12`. The `jira-DEMO-77` external_ref form is covered by unit tests only |

## Ownership and labels

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| O1 | Assign writes the bead label, checked with `bd show` | Stub `bd`. `ctl assign 1 ov-5`, then `bd show ov-5` (the stub's, labels kept in a file) | `bd update ov-5 --add-label overseer:S1` was run, `bd show` lists `overseer:S1`; the assignee field is not touched | Verified 2026-11-11: stub log `update ov-5 --add-label overseer:S1`, `bd show` said `labels: overseer:S1`; no `--assignee` argument was ever passed |
| O2 | Refuse | `ctl assign 2 ov-5` while S1 owns it | Refused, exit 1, names the owner, nothing written | Verified 2026-11-11: `overseer: ov-5 is owned by S1`, exit 1, no new `bd update` call |
| O3 | Move | `ctl assign 2 ov-5 --move` | S2 owns it; the label moves (`--add-label overseer:S2 --remove-label overseer:S1` in one call); S1 gets a notice | Verified 2026-11-11: `bd show` listed only `overseer:S2`; `ctl items` showed `notice  S1  ov-5 was moved to S2` |
| O4 | Warn, then confirm | Assign a ticket to S3. `ctl assign 4 ov-3` (a bead under it), then again with `--yes`. Also assign a ticket whose bead another slot owns | Without `--yes` the question is printed and the exit status is 2 with nothing written; with `--yes` it is assigned, labelled, and the other owner gets a notice. Ticket assignments are not labelled, only beads | Verified 2026-11-11: `This bead is under DEMO-12, owned by S3. Assign to S4 anyway?`, exit 2; with `--yes` exit 0, `bd show ov-3` listed `overseer:S4`, S3 got `S4 took ov-3, which overlaps your work`. The ticket-with-owned-bead case asked `DEMO-12 has beads owned by other slots: ov-3 (S4). Assign to S5 anyway?`, exit 2, then S5 owned it and S4 got a notice. No label was ever set on `DEMO-12` |
| O5 | Release removes the label | `ctl release ov-5`, `bd show ov-5`; release it again | Label gone; releasing an unowned key is an error | Verified 2026-11-11: `bd show` listed no label after the release; the second release printed `overseer: ov-5 is not assigned`, exit 1 |
| O6 | Two clients assign at once (Review Focus 3) | Five `ctl assign <n> ov-5 --yes` started together for five slots | Exactly one succeeds, the other four are refused, one label is written | Verified 2026-11-11: S2 exited 0, S1, S3, S4 and S6 got `ov-5 is owned by S2`; `bd show` listed `overseer:S2` only. The GUI against `ctl` at the same time is awaiting Jonathan |
| O7 | Restart keeps ownership, before the first refresh (Review Focus 4) | With owners in place, `ctl shutdown`, start the daemon again, `ctl items` straight away, once with a hung `bd` and once after a refresh; `ctl status` | The same owners and notices show before any source has answered, and again after the refresh; slots that were running come back `resumable`; labels in beads are unchanged | Verified 2026-11-11: with `bd` hung (`beads  not read yet`) `ctl items` listed `DEMO-12  ...  S5` and `ov-3  unknown  S4  ov-3  (left the list)` plus the three notices; after a refresh `ov-3` was back under its ticket, still `S4`. S0, S2 and S5 showed `resumable`. `ov-3` still carried `overseer:S4` |
| O8 | Notices | After O3 and O4, `ctl items` | The notices appear under the items with their slot, and survive a restart | Verified 2026-11-11: three notices listed, the same three after the restart in O7. Showing them in the slot panel is awaiting Jonathan (U2) |

## Kickoff, role files and S0

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| K1 | `ctl kickoff` builds the text from the slot's items | `ctl kickoff 5` (owns a ticket whose folder exists under `<brain>/Sprints/*/`), `ctl kickoff 6` (owns nothing) | The text names the slot, each item with its status and title, and the ticket folder; a slot with nothing is an error | Verified 2026-11-11: text began `You are S5. Overseer has assigned you this work:`, listed `- DEMO-12 (In Progress): Widget wiring` and the `Sprints/sprint-99/DEMO-12-widget` folder; `ctl kickoff 6` printed `overseer: S6 owns no work items`, exit 1 |
| K2 | The kickoff becomes the first prompt of a started slot (the `ctl` form of Assign and start) | `ctl start 5 --cwd <folder> --prompt "$(ctl kickoff 5)" -- /path/to/claude` with a stub | Stub argv ends `-- <kickoff text>` after the Overseer flags | Verified 2026-11-11: argv ended `[--]` then one element beginning `You are S5. Overseer has assigned you this work:` |
| K3 | An edited `assignment.md` is used | Edit `<state>/roles/assignment.md`, `ctl kickoff 5` | The edited text comes out | Not run by script: awaiting Jonathan. A parked unit-test gap |
| K4 | An existing install upgrades its role files | Put the previous shipped `coordinator.md` (from git history) and an edited `worker.md` in `<state>/roles/`, start S0 and S2 | The unedited old coordinator is replaced by the new one (it gains the `ctl assign` text); the edited `worker.md` is kept, the shipped default is written beside it as `worker.md.new`, and S2's `role.md` still has the edit | Verified 2026-11-11: `coordinator.md` went from no `ctl assign` line to one; `worker.md` kept its extra line, `worker.md.new` was identical to the shipped worker role, and the slot's `role.md` contained the edit. Run on a fresh temp state dir, not Jonathan's installed one: awaiting Jonathan for his real `~/Library/Application Support/overseer/roles/` after `make install` |
| K5 | S0 can assign through `ctl` | Start S0 with a stub claude, read `slots/S0/role.md` and the argv | `role.md` tells S0 to run `ctl items` first and `ctl assign <n> <key>` before `ctl start`, handling the question and exit 2; the kickoff is the last argv element | Verified 2026-11-11 for the role text and argv: lines for `ctl items` and `ctl assign <n> <key> ... before ctl start` are in S0's `role.md`, and the argv ended with the round kickoff. A real S0 round that assigns is awaiting Jonathan |
| K6 | Kickoff for a slot that is Exited or Resumable | In the GUI, assign an item to a slot whose card is Exited, open the panel | Send kickoff stays disabled and Assign and start is what the picker offers; nothing is typed into a dead terminal | awaiting Jonathan. Not checked by the reviewers |
| K7 | Kickoff reaches a real Claude session intact | Real claude in S2 sitting Idle, assign an item, press Send kickoff in the Owns panel | Claude shows the whole kickoff as one pasted message and answers it once; it is not submitted line by line. Send kickoff is greyed while the slot is Working or Needs you, and a double click sends once | awaiting Jonathan. The paste uses bracketed paste and then a Return after 100 ms; a terminal without bracketed paste mode would submit the lines one by one, as Cmd+V does. Claude Code turns the mode on |
| K8 | Role file upgrade on the installed app | After `make install`, start a slot and look in `~/Library/Application Support/overseer/roles/`. If a `.new` file sits beside an edited role, merge it by hand | The coordinator role mentions `ctl assign`; edited files are untouched | awaiting Jonathan |

## Cards, panel and picker (GUI, awaiting Jonathan)

| # | Item | Steps | Passes when | Result |
|---|------|-------|-------------|--------|
| U1 | Pills on cards | Assign an item to a slot (picker or `ctl`) | The slot's card shows the item key as a pill, a ticket and a bead are told apart, and the pill appears within a second of the `ctl assign` without reopening the window | awaiting Jonathan |
| U2 | Owns panel | Click the card, open the session panel | An Owns section lists the slot's items with status; notices for the slot show there; releasing an item removes it from both the panel and the card | awaiting Jonathan |
| U3 | Picker lists the work | Open the picker from the board | Tickets with their beads indented, owners shown, source health (for example `acli not found`) at the top, arrow keys and Return work, Esc closes. Read the layout: nothing clipped at the window sizes Jonathan uses | awaiting Jonathan. The layout has never been seen: the picker was reviewed from code and a unit-tested model only |
| U4 | Picker refuse, warn and move | Pick an item another slot owns; pick a bead under an owned ticket; answer the question both ways | Refused items say who owns them and nothing changes; a warning asks first; Move takes the item; the question stays until it is answered and a click on the question row does not answer it | awaiting Jonathan |
| U5 | Assign and start | Pick an item for an empty slot and choose Assign and start | The slot starts in the item's folder with the kickoff as its first prompt (label is the item key), the pill appears, and the bead has `overseer:S<n>` in a test beads folder | awaiting Jonathan |
| U6 | Send kickoff only when idle | Slot Working, then Idle, owning an item | Disabled while Working, enabled when Idle; see K6 and K7 | awaiting Jonathan |
| U7 | Pills after a daemon restart | `launchctl kickstart -k gui/$(id -u)/uk.co.techize.overseer` from Terminal, reopen the window (F9 of phase 2) | The same pills show at once, before Jira answers | awaiting Jonathan. The daemon side is O7 |
| U8 | Real Jira | With `acli` installed and logged in, open the picker and run `ctl refresh` | Tickets from the configured JQL load; nothing in Jira changes; a logged-out `acli` shows its failure on the Jira line while beads still load | awaiting Jonathan. `acli` is not installed on this Mac, and J1 to J2 covered the failure paths with stubs |

## Findings from the 2026-11-11 run

- **F1 (fixed): a bead's kickoff named the bead's own folder only.** `ctl
  kickoff 4` for S4, which owns `ov-3` under `DEMO-12` (whose folder exists),
  printed `Ticket folders in 2ndBrain: none found`. Fixed after the run: the
  kickoff now also lists the folder of an owned bead's ticket, once, whoever
  owns the ticket (unit test `bead_kickoff_names_its_tickets_folder` and the
  server test of the same name).
- **F2 (note): `ctl refresh` can take longer than 30 s.** With `bd` hung, one
  refresh returned after 38 s because it queued behind a run already in flight;
  a refresh on its own returned after exactly 30 s and killed the child.
- **F3 (fixed): before the first refresh an owned item showed its key as its
  title and `(left the list)`.** After a restart with `bd` hung, `ov-3`
  appeared as `ov-3  unknown  S4  ov-3  (left the list)` although it was only
  not read yet. Fixed after the run: while the source that lists the item
  (beads for a bead, beads or Jira for a ticket) has not been read since the
  start, `ctl items` says `(not read yet)` and the Owns panel says `not read
  yet`. The title is still the key until the source answers. See L2.
- **Environment note:** the temp state dirs were under `/tmp` and not the
  session scratch directory, because the unix socket path would have been
  longer than the 104 character limit.

No scriptable check failed.

## Known limitations from the reviews

- **L1: label writes still queued 3 s after shutdown are dropped.** Labels are
  written to beads by a queue after the assignment is stored in the ledger.
  Shutdown now waits up to 3 s for the queue to drain (server test
  `shutdown_waits_for_queued_labels`), but a `bd` slower than that is killed,
  nothing retries it, and the bead and the ledger can disagree until the item
  is assigned again.
- **L2: an owned item that left the list shows its key as its title.** Closed
  items leave the beads list. An owned item keeps its owner and shows as "not
  in the current list" with the key as its title until it is released.
- **L3: no promotion detection across a daemon restart.** If a bead gains a
  ticket key while the daemon is down, nothing notices the change in who owns
  what.
- **L4: notices are shown, not delivered.** A moved or overlapped session is
  told only through the panel and `ctl items`. Typing into a session safely
  needs the delivery rules of phase 4.
- **L5: Send kickoff does not check the slot owns something first.** With
  nothing owned it is enabled when the slot is Idle and the daemon's
  `owns no work items` error shows in the status line.
- **L6: without bracketed paste mode the kickoff would submit line by line.**
  Claude Code enables the mode. A slot running another program might not.
- **L7: unknown keys in `config.toml` are ignored,** and a bad `ticket_pattern`
  or an empty `jira_command` is reported where it is used (on the source line),
  not when the file loads.
- **L8: a bead with no `id` makes `bd list --json` fail as a whole,** and
  the source commands run with a cleared environment, so they see only the
  login shell's `PATH` and `HOME`.
- **L9: a SQLite `user_version` above 2 is accepted silently,** so an older
  Overseer would open a newer ledger without complaint.
- **L10: the worktree is not part of the kickoff.** It names the ticket folder;
  worktrees and targets are phase 4.
- **L11 (fixed): `ctl refresh` printed nothing.** It now prints the source
  lines in the `ctl items` format, so a source failure shows straight away.

## Sign-off

Phase 3 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by:
- Date:
- Open follow-ups:
