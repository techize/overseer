# Phase 6 acceptance

Phase 6 (the coordinator) is done only when every row below passes. Every row
runs on the home Mac in an isolated rig: a state folder at `/tmp/ov-hand6`, a
throwaway git repo `demo-api` with its worktrees under the rig, a small test
vault, stub `bd`, Jira and `claude`, a temp socket and a temp copy of the app
bundle. Never run any of it against the real vault (`~/Repos/2ndBrain`), the
real `~/.claude/CURRENT_SESSION.md`, `~/Repos/.worktrees`, the installed app,
its LaunchAgent `uk.co.techize.overseer`, its socket or
`~/Library/Application Support/Overseer`. Row Z1 checks exactly that.

Unit and integration tests cover the logic (`cargo test --workspace`). They do
not replace a hand check where a row asks for one: rows marked "real session"
need a real Claude Code session, rows marked "GUI" need the app on screen,
and the rest are scripted from a terminal. The Result column reads `pending`
until Jonathan (or a controller) has run the row; then write what was
observed, and when. The examples in this file (`DEMO-12`, `ov-3`, `demo-api`,
`uat/root`, the `demo lane` label) are invented.

The checks quote the texts the code produces. If a text differs on screen,
that is a finding: record it in the Result column.

## Setup for the checks

Use one Terminal tab for every step so it keeps the variables.

**Risk, read first.** The real-session rows (P1, P2, P11, P12, P18) start a
real `claude`. With your normal Claude settings that session would also run
your global hooks and skills, which can commit `~/Repos/2ndBrain` or write the
real `~/.claude/CURRENT_SESSION.md`. So give the real sessions the isolation
in step 5 (a throwaway `CLAUDE_CONFIG_DIR` and a temp `HOME`, so none of your
global settings, hooks or `~/.claude.json` are read or written). Every other
row uses the stub `claude` from step 2 and never starts a real one. Step 0
records the real state so Z1 can prove nothing leaked.

### Step 0: record the real state

```
git -C ~/Repos/2ndBrain status --short > /tmp/ov-hand6-real-status.before
git -C ~/Repos/2ndBrain log -3 --format='%h %s' > /tmp/ov-hand6-real-log.before
cp -L ~/.claude/CURRENT_SESSION.md /tmp/ov-hand6-real-cs.before
ls -laR ~/Repos/.worktrees > /tmp/ov-hand6-real-wt.before 2>&1
launchctl list 2>/dev/null | grep uk.co.techize.overseer > /tmp/ov-hand6-real-agent.before
touch /tmp/ov-hand6-real-marker
find ~/Library/Application\ Support/Overseer -type f 2>/dev/null | sort > /tmp/ov-hand6-real-state.before
```

(`/tmp/ov-hand6-*` is outside `/tmp/ov-hand6/`, so `rm -rf /tmp/ov-hand6` at
the end does not remove them before Z1.)

### Step 1: state folder, binary, wrapper

Build from this branch's worktree before it is merged, or from the merged
`main` checkout after (`cd` to whichever you are testing):

```
export OVERSEER_HOME=/tmp/ov-hand6
export OVERSEER_CONFIG=$OVERSEER_HOME/config.toml
export OVERSEER_TARGETS=$OVERSEER_HOME/targets.toml
export OV=$OVERSEER_HOME/Overseer.app/Contents/MacOS/overseer
export V=$OVERSEER_HOME/vault      # the test vault
export R=$OVERSEER_HOME/demo-api   # the throwaway repo
export WT=$OVERSEER_HOME/worktrees # worktree_root
export W=$OVERSEER_HOME/work
mkdir -p $OVERSEER_HOME/bin $OVERSEER_HOME/stubs $OVERSEER_HOME/plans $OVERSEER_HOME/home $OVERSEER_HOME/claude-config $WT $W
command -v claude > $OVERSEER_HOME/real-claude   # before the stubs go on PATH
cd ~/overseer/.claude/worktrees/phase-5-plan   # or the main checkout once merged
cargo build --release -p overseer
scripts/bundle.sh target/release/overseer $OVERSEER_HOME/Overseer.app
printf '#!/bin/sh\nexec %s "$@"\n' "$OV" > $OVERSEER_HOME/bin/overseer
chmod +x $OVERSEER_HOME/bin/overseer
printf 'export PATH=/tmp/ov-hand6/stubs:/tmp/ov-hand6/bin:$PATH\n' > $OVERSEER_HOME/home/.profile
export PATH=$OVERSEER_HOME/stubs:$OVERSEER_HOME/bin:$PATH
```

The daemon reads the work list and starts sessions with the login shell's
environment, so the temp `HOME`'s `.profile` puts the stubs first (as phase 4
and 5 did) and the app is started with `SHELL=/bin/sh`.

### Step 2: stubs, repo and vault

The stub `bd` serves three beads and logs its calls; the stub `acli` (Jira)
serves three tickets; the stub `claude` logs its arguments (the kickoff is the
last one, after `--`) and sleeps, unless `$OVERSEER_HOME/use-real` exists, in
which case it runs the real `claude` found in step 1:

```
cat > $OVERSEER_HOME/stubs/bd <<'STUB'
#!/bin/sh
{ echo "argv: $*"; if [ "$1" != list ] && [ "$1" != sync ]; then echo "stdin: $(cat 2>/dev/null)"; fi; } >> /tmp/ov-hand6/bd.log
if [ "$1" = list ]; then
  echo '[{"id":"ov-3","title":"DEMO-12 wire the widget","status":"open","priority":1},{"id":"ov-4","title":"Standalone chore","status":"open","priority":2},{"id":"ov-5","title":"DEMO-13 tidy the docs","status":"open","priority":2}]'
else echo "bd stub ok: $*"; fi
STUB
cat > $OVERSEER_HOME/stubs/acli <<'STUB'
#!/bin/sh
cat /tmp/ov-hand6/jira.json
STUB
cat > $OVERSEER_HOME/jira.json <<'EOF2'
{"issues":[
  {"key":"DEMO-12","fields":{"summary":"Widget wiring","status":{"name":"In Progress"}}},
  {"key":"DEMO-13","fields":{"summary":"Docs tidy","status":{"name":"To Do"}}},
  {"key":"DEMO-14","fields":{"summary":"Release notes","status":{"name":"Done"}}}
]}
EOF2
cat > $OVERSEER_HOME/stubs/claude <<'STUB'
#!/bin/sh
if [ -e /tmp/ov-hand6/use-real ]; then exec "$(cat /tmp/ov-hand6/real-claude)" "$@"; fi
{ echo "--- OVERSEER_SLOT=$OVERSEER_SLOT cwd=$(pwd)"; for a in "$@"; do echo "$a"; done; } >> /tmp/ov-hand6/claude-stub.log
exec sleep 3600
STUB
chmod +x $OVERSEER_HOME/stubs/bd $OVERSEER_HOME/stubs/acli $OVERSEER_HOME/stubs/claude
: > $OVERSEER_HOME/bd.log; : > $OVERSEER_HOME/claude-stub.log
```

The throwaway repo and the test vault (a journal for S0 to read in P2, the
folder `Sprints/sprint-26-01/DEMO-12-demo-lane`, no git remote; the writer is
off, so nothing here commits):

```
git init -q -b main $R && git -C $R config user.name demo && git -C $R config user.email demo@example.test
printf '# demo-api\n' > $R/README.md && git -C $R add -A && git -C $R commit -q -m seed
D=$(date +%Y-%m-%d); J=$V/06-Daily/$(date +%Y)/$(LC_ALL=C date +%m-%B)/$D-journal.md
mkdir -p $V/Sprints/sprint-26-01/DEMO-12-demo-lane $(dirname $J)
printf '# demo sprint\n' > $V/Sprints/sprint-26-01/SPRINT-DASHBOARD.md
printf '# Journal\n\n## Work Log\n\n- DEMO-14 release notes: parked until Friday.\n' > $J
printf 'old history\n' > $OVERSEER_HOME/CURRENT_SESSION.md
```

### Step 3: config

```
cat > $OVERSEER_HOME/config.toml <<EOF2
brain_dir = "$V"
beads_dir = "$V"
brain_writer = false
current_session = "$OVERSEER_HOME/CURRENT_SESSION.md"
bd_command = ["$OVERSEER_HOME/stubs/bd"]
jira_command = ["$OVERSEER_HOME/stubs/acli", "{jql}"]
ticket_pattern = 'DEMO-\d+'
beads_poll_secs = 5
jira_poll_secs = 5
worktree_root = "$WT"
EOF2
```

`targets.toml` does not exist, so the compiled-in patterns are used.

### Step 4: helpers the rows use

```
# Writes a plan file.
plan_a() { cat > $OVERSEER_HOME/plans/a.json <<EOF2
{"summary":"Two lanes: widget and docs","slots":[
 {"slot":"S1","label":"docs","items":["DEMO-13","ov-5"],"worktree":{"repo":"$R","ticket":"DEMO-13"},"kickoff":"Start with the README."},
 {"slot":"S3","label":"demo lane","items":["DEMO-12","ov-3"],"targets":[{"target":"repo:demo-api:main","reason":"merge"}],"worktree":{"repo":"$R","ticket":"DEMO-12"},"kickoff":"Wire the widget, then ask before merging.","rules":["Do not deploy to UAT before S2 merges"]},
 {"slot":"S4","label":"chore","items":["ov-4"],"targets":[{"target":"argo:uat/root","reason":"sync"}],"cwd":"$W"}
]}
EOF2
}
# Puts the rig back to "S1 running, S0 running, S2 to S6 empty, nothing owned or held".
reset() {
  for n in 1 2 3 4 5 6; do $OV ctl stop $n 2>/dev/null; done
  for k in DEMO-12 DEMO-13 ov-3 ov-4 ov-5; do $OV ctl release $k 2>/dev/null; done
  $OV ctl release S1 argo:uat/root 2>/dev/null; $OV ctl release S3 repo:demo-api:main 2>/dev/null
  for p in $(git -C $R worktree list --porcelain | sed -n 's/^worktree //p' | grep -v "^$R$"); do git -C $R worktree remove --force $p; done
  git -C $R branch --list 'DEMO-*' | xargs -r git -C $R branch -D >/dev/null
  : > $OVERSEER_HOME/claude-stub.log
  : > "$OVERSEER_HOME/bd.log"
}
# Sends one hook event the way Claude does: hk <slot> <event> '<json>'.
hk() { printf '%s' "$3" | OVERSEER_SLOT=$1 $OV hook $2; }
```

`plan_a` is the plan most rows use. It has a running slot (S1, which also
gets a worktree, so the plan warns), an empty slot with a worktree, items,
a target and a rule (S3), and an empty slot with a plain folder that queues
for a target held by S1 (S4). Before each row that needs it:
`reset; $OV ctl start 1 --cwd $W --label old -- sleep 3600; $OV ctl claim 1 argo:uat/root` (S1 holds `argo:uat/root`) and
`$OV ctl start 0 --cwd $V` (S0 is the stub `claude`, so S0 counts as live and
its relays queue in the ledger; `$OV ctl locks` lists them).

### Step 5: start the app, isolated

```
HOME=$OVERSEER_HOME/home SHELL=/bin/sh CLAUDE_CONFIG_DIR=$OVERSEER_HOME/claude-config $OV &
```

The app starts the temp daemon itself with that environment. Scripted rows can
instead start the daemon alone with the same prefix and `$OV daemon &`.

Pre-flight: `$OV ctl items` shows `beads  ok  3 items` and the Jira source ok
with 3 items; the stub's items appear (`ov-3  open  -  DEMO-12 wire the
widget`, `ov-4  open  -  Standalone chore`, ...) and no real beads list. If it
shows anything else the daemon is not using the stub `PATH`: stop and fix.

A real session in this setup has no login of its own: in the first real
session's terminal view, run `/login` and sign in yourself (Overseer never
types credentials); that writes only under `$OVERSEER_HOME/claude-config`.
Answer Claude's folder trust prompt there too. For the real-session rows:
`touch $OVERSEER_HOME/use-real` before starting the session, `rm` it after.
Before `/login` or any real work, in the first real session's terminal view
run `env | grep -E '^(HOME|CLAUDE_CONFIG_DIR|OVERSEER_SLOT|OVERSEER_HOME)='`:
`HOME` must be `/tmp/ov-hand6/home`, `CLAUDE_CONFIG_DIR` must be
`/tmp/ov-hand6/claude-config`, and `OVERSEER_SLOT` and `OVERSEER_HOME` must be
there. If `HOME` or `CLAUDE_CONFIG_DIR` is missing or anywhere else, quit the session and
the app and start again from the line above; do not log in.

### Step 6: finish

`$OV ctl shutdown`, quit the app, run row Z1, then
`rm -rf /tmp/ov-hand6 /tmp/ov-hand6-real-*`.

## The coordinator's tools and a real round

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| P1 | S0's tools are listed and pre-approved (real session) | `touch $OVERSEER_HOME/use-real`; `$OV ctl start 0 --cwd $V --label coordinator`; in S0 ask `List the tools you have from the overseer server, then call get_board`. Also scripted: pipe `initialize`, `notifications/initialized` and `tools/list` to `OVERSEER_SLOT=S0 $OV mcp`; `grep -o mcp__overseer__[a-z_0-9]* $OVERSEER_HOME/slots/S0/settings.json \| sort -u \| wc -l` | The list names fourteen tools, among them `get_board`, `get_work_items`, `propose_plan`, `request_status`, `report_progress` and `set_board_summary`; `settings.json` allows `mcp__overseer__get_board`, `mcp__overseer__propose_plan`, `mcp__overseer__request_status`, `mcp__overseer__report_progress` and `mcp__overseer__set_board_summary` (14 names); the `get_board` call runs with no permission prompt and returns text starting `Summary: none yet` followed by seven slot lines S0 to S6, `Work items:` and `Locks:`; `slots/S0/role.md` mentions `propose_plan` and the folder `$V` | pending |
| P2 | A real start-of-day round ends in `propose_plan` and stops (real session) | Continue P1 (real S0, `use-real` still there, no other slot running). Type `Run the start-of-day round.` (or let the kickoff do it: stop S0, `$OV ctl start 0 --cwd $V` starts it with the kickoff `Run the start-of-day round: call get_board, read today's journal, and propose a plan with propose_plan.`). Watch S0 until it is idle again; then `$OV ctl plan` | S0 calls `get_board` and `get_work_items`, runs `date`, reads today's journal (the `DEMO-14` line is parked: it is listed apart as parked, not planned), then calls `propose_plan` once and stops with no further tool calls. It does not call `claim_target`, start or assign anything: `$OV ctl items` shows no owners, no worktree exists under `$WT`. `ctl plan` shows one `Plan <n>  pending  proposed HH:MM` with S0's summary and one line per slot it chose. The tool replied `Plan <n> is waiting for Jonathan.` | pending |

## The plan notice, the view and the three decisions (GUI)

Set up for each row: the app open, S0 as the stub, S1 running, then
`plan_a; $OV ctl plan propose $OVERSEER_HOME/plans/a.json` (as Jonathan, so
no `OVERSEER_SLOT`; it answers `Plan <n> is waiting for Jonathan.`).

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| P3 | The plan notification, the board and the `p` key | With another app in front, propose the plan. Then, with the board in front, `reset`, propose again. Press `p` on the board. Also press `p` with no plan pending (decide it first). Finally with the app in the background and a session screen open, propose again and click the notification | A banner titled `S0 proposed a plan` with the plan's summary line (`Two lanes: widget and docs`) as its body, once per plan (a repeat of the same plan posts nothing); the coordinator panel on the board reads `Plan <n> waiting for you (p)`, the coordinator panel shows the summary, `p` opens the plan view titled `Plan <n> from S0`; `p` with nothing pending leaves the status line `No plan is waiting`. Clicking the banner brings the app forward, shows the board (not S0's session or any slot) and opens the plan view for that plan | pending |
| P4 | Approve unedited: every outcome line, worktrees, claims, starts, kickoffs and S0's relay | The P3 plan pending (S1 holds `argo:uat/root`). `$OV ctl plan` first; then `$OV ctl plan approve <n>`; then `$OV ctl items`, `ls $WT`, `git -C $R worktree list`, `$OV ctl locks`, `cat $OVERSEER_HOME/claude-stub.log` | `ctl plan` before shows the two warnings `argo:uat/root is held by S1; S4 will queue for it` and `S1 is running; it will be told to work in <WT>/demo-api-DEMO-13 but not moved`. Approve prints ten lines in slot order: `S1: assigned DEMO-13, ov-5`, `S1: worktree <WT>/demo-api-DEMO-13 created`, `S1: kickoff queued`, `S3: assigned DEMO-12, ov-3`, `S3: worktree <WT>/demo-api-DEMO-12 created`, `S3: holds repo:demo-api:main`, `S3: started in <WT>/demo-api-DEMO-12`, `S4: assigned ov-4`, `S4: queued for argo:uat/root behind S1`, `S4: started in <W>`. `ctl items` shows `S1`, `S3`, `S4` as owners; `git worktree list` lists both new worktrees on branches `DEMO-12` and `DEMO-13`; `ctl locks` shows `S3` holding `repo:demo-api:main`, S4 `waiting: S4`, and a pending relay to S1 with S1's kickoff (`You are S1.` ... `Start with the README.`); the stub log has two starts (S3 in the DEMO-12 worktree, S4 in `$W`) whose last argument is the kickoff: S3's is the filled assignment text (`You are S3.`, both items, `You hold these targets: repo:demo-api:main`, `Work in <WT>/demo-api-DEMO-12.`, a line pointing at `CURRENT_SESSION.md`, `Rules for this work:` and `- Do not deploy to UAT before S2 merges`) followed by a blank line and `Wire the widget, then ask before merging.`. S0 has one new relay since the reset, `Jonathan approved plan <n>.` followed by the same ten lines. `ctl plan` shows the plan `approved HH:MM` with the outcome lines under it and the board's panel no longer shows the waiting line | pending |
| P5 | Approve with edits | Reset, propose again. In the plan view: drop the S4 entry, drop the item `ov-5` from S1, reword S3's kickoff to `Wire the widget. Edited.` (click the kickoff, Tab walks the fields), then Cmd+Enter. Then `$OV ctl plan`, `$OV ctl locks`, `tail -4 $OVERSEER_HOME/claude-stub.log` | The view shows `Approving` while it works and closes; the status line reads `S1: assigned DEMO-13` (the first outcome line); `ctl plan` shows `approved HH:MM (edited)`; there are no S4 lines (S4 owns nothing, no start), `ov-5` is unowned, S3's kickoff in the stub log ends `Edited.`; the relay to S0 reads `Jonathan approved plan <n> with edits.` then the outcome lines | pending |
| P6 | Reject with a reason | Reset, propose again. In the view type `S2 is merging first` in the Reject reason field and click Reject. Repeat with `$OV ctl plan reject <n>` and no reason | Nothing is assigned, created or started (`ctl items` unchanged, `ls $WT` empty). Status line `Plan <n> rejected`; `ctl plan` shows `rejected`; S0's relay reads `Jonathan rejected plan <n>: S2 is merging first`; with no reason `ctl` prints `Plan <n> rejected.` and S0 gets `Jonathan rejected plan <n>: no reason given` | pending |
| P7 | Re-validation after a hand assignment | Reset, propose a plan that gives `ov-4` to S3 (`{"slot":"S3","label":"demo lane","items":["ov-4"],"cwd":"/tmp/ov-hand6/work"}`). Then `$OV ctl assign 2 ov-4`. Then `$OV ctl plan approve <n>` and, on a second try, approve from the view | Approval is refused with `ov-4 is owned by S2`; the plan stays `pending` in `ctl plan`; nothing ran (no worktree, no start, no relay to S0); the same text shows in the view and the view stays open. After `$OV ctl release ov-4` the same plan approves and runs | pending |
| P8 | Double approval | Reset, propose `plan_a`. `$OV ctl plan approve <n> & $OV ctl plan approve <n>; wait`. Then once more from the view while the plan is already approved | Exactly one call prints the outcome lines; the other fails with `plan <n> is no longer pending` (exit 1). The stub log has one start of S3 and one of S4; S0 has one new `Jonathan approved plan <n>` relay since the reset; `bd.log` (emptied by `reset`) has one `argv: update ov-3 --add-label overseer:S3`. Approving from the view after that closes it with `Plan <n> was approved elsewhere` | pending |
| P9 | A worker is refused: propose, summary, status and `ctl plan approve` | As S1 (prefix each command with `OVERSEER_SLOT=S1`; do not `export` it, or every later row runs as S1): `$OV ctl plan propose $OVERSEER_HOME/plans/a.json`; `$OV ctl summary "x"`; `$OV ctl status-of 3`; `$OV ctl plan approve <n>` (with a plan pending); `$OV ctl plan reject <n> no`. Then the same through `OVERSEER_SLOT=S1 $OV mcp` with `tools/call` for `propose_plan` (`{"summary":"x","slots":[{"slot":3,"cwd":"/tmp/ov-hand6/work"}]}`), `set_board_summary` and `request_status` | Each fails (exit 1, MCP `isError` true) with `only the coordinator (S0) can propose a plan`, `only the coordinator (S0) can set the board summary`, `only the coordinator (S0) can ask for a status`, and `only Jonathan can approve or reject a plan` (twice). The plan is still `pending`, nothing changed. `get_board` and `get_work_items` work from S1 (read only); `OVERSEER_SLOT=S1 $OV ctl report "x"` works | pending |

## Status requests, the summary and the Progress timeline

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| P10 | Request status and the answer to S0 | S0 (stub) and S1 (stub, running) started. `$OV ctl status-of 1`; again; `$OV ctl status-of 5`; `$OV ctl status-of 0`; `$OV ctl locks`; then `OVERSEER_SLOT=S1 $OV ctl report "tests green, PR next"`; `$OV ctl locks`; then a second report with no request open | First call prints `Asked S1 for a status. Its answer will be typed in here.`; the second prints `already asked S1 at HH:MM` and adds no second relay; `status-of 5` fails with `S5 is not running` and `status-of 0` with `S0 is the coordinator; ask S1 to S6 for a status`. `ctl locks` lists one pending relay to S1: `S0 asks for a short status. Answer with the report_progress tool in two or three lines.` After the report there is a pending relay to S0: `S1 reports: tests green, PR next`. The second report is recorded (Progress, P12) but sends no relay. `ctl board` shows `status asked HH:MM` on the S1 line until the answer, then `last report HH:MM: tests green, PR next` under it (the GUI board does not render `status asked`; S1's Relays box in the panel lists the pending status relay until it is typed in, and the report appears in S1's Progress box) | pending |
| P11 | Summary on the board, and refresh on "what's going on?" (real session) | `$OV ctl board` and the board panel first. `$OV ctl summary "Two lanes running."`; `$OV ctl summary " "`; a 2001 character text. Then, with real S0 (`use-real`) and the plan from P4 applied, type `what's going on?` into S0. | Before: `ctl board` starts `Summary: none yet`, the panel says `No summary yet` with `Ask S0 "what's going on?" to get one.` After the first: `Summary (HH:MM): Two lanes running.` and the panel `Updated HH:MM` with the text; the empty text fails `the summary is empty`, the long one `the summary is longer than 2000 characters`, and the summary is unchanged. After the question S0 calls `get_board` and `set_board_summary` with two to four lines about the slots; the panel updates to the new text and time without a restart. `ctl board` lists S0 to S6 lines (`S3 demo lane  working 5m  owns DEMO-12, ov-3  holds repo:demo-api:main`, `waiting argo:uat/root` on S4), `Work items:`, `Locks:`, and `Plan <n> waiting for Jonathan since HH:MM` only while one is pending | pending |
| P12 | Progress timeline (real session for the activity rows) | A stub S1 first: start it, `OVERSEER_SLOT=S1 $OV ctl report "tests green"`, `hk S1 Notification '{"session_id":"s","message":"Claude needs your permission to use Bash","notification_type":"permission_prompt"}'`, `hk S1 Stop '{"session_id":"s"}'`, `$OV ctl stop 1`. Open S1's panel. Then with a real S1 (`use-real`): give it work, answer a permission prompt, let it finish | The Progress box lists newest first, one line each `HH:MM <text>`: `exited`, `idle`, `needs you: Claude needs your permission to use Bash`, `tests green`, `started`; state rows are muted, reports normal text; an empty slot says `Nothing reported yet`. The real S1 adds `working` and `idle` rows as it runs. Repeated `Stop` events produce one `idle` row, not a row per event. A restart of the app keeps the rows | pending |
| P13 | The done count in the header | With the stubs as set up: look at the board header. `sed -i '' 's/"To Do"/"Closed"/' $OVERSEER_HOME/jira.json`; `$OV ctl refresh` (or wait 5 s). Then empty the issue list in `jira.json` | Header reads `<n> running, done 1/3` (`DEMO-14` is Done; beads are not counted), with `, <k> need you` after the running count while a session needs Jonathan (use the P12 notification); after the edit `done 2/3`; with no tickets the count is left out entirely. There is no sprint name or day (phase 7) | pending |
| P14 | Manual kickoff text | After P4 (S3 owns `DEMO-12` and `ov-3`, runs in `<WT>/demo-api-DEMO-12`, holds `repo:demo-api:main`): `$OV ctl kickoff 3`; `$OV ctl kickoff 2`; then in the GUI open S3's panel, make it idle (`hk S3 Stop '{"session_id":"s"}'`) and press Send kickoff | `ctl kickoff 3` prints the assignment template filled: `You are S3.`, `- DEMO-12 (In Progress): Widget wiring` with `  - ov-3 (open): DEMO-12 wire the widget` under it, `Ticket folders in 2ndBrain: <V>/Sprints/sprint-26-01/DEMO-12-demo-lane`, `You hold these targets: repo:demo-api:main`, `Work in <WT>/demo-api-DEMO-12.`, `Keep your section of <CURRENT_SESSION> current with update_current_session.`; no `{...}` is left and, because the manual kickoff has no rules, neither `Rules for this work:` nor an empty line stands where they would be. `ctl kickoff 2` fails `S2 owns no work items`. Send kickoff is offered only while the session is idle and queues the same text once (a double click does not send it twice) | pending |

## Roles, restart and the rest

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| P15 | Role upgrade from the phase 5 files | `$OV ctl shutdown`; `mkdir -p $OVERSEER_HOME/roles`; copy `crates/overseer-daemon/tests/fixtures/roles-phase5/*` there; start the daemon; start a session in S0 and one in S3 (stubs; the role files are checked when a session starts); `grep 'role template' $OVERSEER_HOME/logs/daemon.log`; `ls $OVERSEER_HOME/roles`. Then edit `worker.md` (add one line), restart, start S3 again | The log says `upgraded role template <path> to the current default` for `coordinator.md`, `worker.md`, `kickoff.md` and `assignment.md`; no `.new` files; `roles/coordinator.md` has `propose_plan` and `request_status`; `roles/assignment.md` has `{worktree}` and `{rules}`; `slots/S0/role.md` names the tools. The edited `worker.md` is kept, `worker.md.new` holds the current default and the log has `role template <path>/worker.md was edited; the current default is in <path>/worker.md.new` | pending |
| P16 | Restart with a plan pending, and just after approving one | Reset, propose `plan_a`. `$OV ctl shutdown`; start the daemon again; `$OV ctl plan`. Then approve it, `$OV ctl shutdown` at once, start again, `$OV ctl plan`, `cat $OVERSEER_HOME/claude-stub.log \| grep -c '^--- '` | After the first restart the plan is still `pending` with both warnings and the board shows `Plan <n> waiting for you (p)`; no notification repeats at connect for a plan the app already announced (a fresh app start posts it once). After the second restart the plan is `approved` with its outcome lines; nothing ran again: no second start, no second `bd update`, the worktrees are as they were and S0 has no second approval relay (still one new relay since the reset) | pending |
| P17 | The plan view while approving, and a plan decided elsewhere (GUI) | Reset. Propose and open the view (`p`); click Approve and press Esc at once (approval creates worktrees and starts sessions, so it takes a moment; if it is too quick to catch, add S5 and S6 entries with their own tickets). Separately: propose, open the view, then in the terminal `$OV ctl plan approve <n>` while it is open; do the same with `ctl plan reject <n>` and with a second `propose` (replaces it) | Esc while `Approving` closes the view at once (it does not wait); the board is usable; when the daemon answers, the status line shows the result (`S1: assigned DEMO-13`, or `Plan <n> not decided: <error>` if it failed) and the approval ran once. Decided elsewhere: the view closes by itself and the status line reads `Plan <n> was approved elsewhere`, `Plan <n> was rejected elsewhere` or `Plan <n> was replaced elsewhere`; no error box is left | pending |
| P18 | A tab whose session needs input is orange (from commit 27b8979) | Start S1 (stub) and open its session so the tab strip shows. `hk S1 Notification '{"session_id":"s","message":"Claude needs your permission to use Bash","notification_type":"permission_prompt"}'`; look at the tabs; `hk S1 Stop '{"session_id":"s"}'`; look again. Repeat with S2 as the current tab while S1 needs you. Stop S1 while it needs you. Optionally repeat with a real S1 at a real permission prompt | While S1 needs you its tab text is orange (also when it is not the current tab); the current tab is marked by the accent border, not by orange; after `Stop` the text goes back to the normal colour; a stopped or exited slot is never orange; the board card for S1 is orange and clears at the same time | pending |
| P19 | A worktree folder that already exists | Reset. `git -C $R worktree add -b DEMO-12 $WT/demo-api-DEMO-12` (a real worktree of the repo), and `mkdir $WT/demo-api-DEMO-13` (a plain folder). Propose a plan with S3 on `DEMO-12` and S5 on ticket `DEMO-13`, both with `"worktree":{"repo":"<R>","ticket":...}`, and approve | `S3: worktree <WT>/demo-api-DEMO-12 reused`, then S3 goes on and starts; S5 fails with `S5: worktree <WT>/demo-api-DEMO-13 failed: <WT>/demo-api-DEMO-13 exists and is not a worktree of <R>; the rest of S5's plan was skipped` and does not start; the first slot's lines are unaffected; nothing is removed or forced | pending |

## Known flake to watch

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| X1 | The full suite, three times | In a fresh Terminal tab (or a subshell) with the rig left behind: `CLEAN_PATH=$(printf '%s' "$PATH" \| tr ':' '\n' \| grep -v ov-hand6 \| paste -sd: -)`, then run each cargo command as `env -u OVERSEER_CONFIG -u OVERSEER_TARGETS -u OVERSEER_SLOT -u OV -u V -u R -u WT -u W -u D -u J PATH="$CLEAN_PATH" OVERSEER_HOME=$(mktemp -d) cargo test --workspace` so no rig variable, stub or `OVERSEER_SLOT=S1` reaches the tests or the live rig socket. Run `cargo test --workspace` three times from the branch worktree; if a phase 4 timing test fails, run it alone (`env -u OVERSEER_CONFIG -u OVERSEER_TARGETS -u OVERSEER_SLOT PATH="$CLEAN_PATH" OVERSEER_HOME=$(mktemp -d) cargo test -p overseer --test hook`; the same prefix before `cargo test -p overseer-daemon --test server permission_prompt_never_gets_a_relay -- --test-threads=1`) three times | Three green full runs, or each failure is one of the known phase 4 timing tests (`hook.rs` with its 200 ms limits, `server.rs` `permission_prompt_never_gets_a_relay`) that passes alone every time. Any other failure, or the same one twice in a row, is a finding: record its output here | pending |

## Nothing leaked

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| Z1 | The real vault, `CURRENT_SESSION.md`, `~/Repos/.worktrees`, the installed app and its state are untouched | After every other row, before the cleanup (do not edit the real vault during the run): `git -C ~/Repos/2ndBrain status --short \| diff - /tmp/ov-hand6-real-status.before`; `git -C ~/Repos/2ndBrain log -3 --format='%h %s' \| diff - /tmp/ov-hand6-real-log.before`; `diff <(cat ~/.claude/CURRENT_SESSION.md) /tmp/ov-hand6-real-cs.before`; `ls -laR ~/Repos/.worktrees 2>&1 \| diff - /tmp/ov-hand6-real-wt.before`; `launchctl list 2>/dev/null \| grep uk.co.techize.overseer \| diff - /tmp/ov-hand6-real-agent.before`; `find ~/Library/Application\ Support/Overseer -type f -newer /tmp/ov-hand6-real-marker`; `find ~/Library/Application\ Support/Overseer -type f \| sort \| diff - /tmp/ov-hand6-real-state.before`; `ps aux \| grep -c '[o]verseer.*ov-hand6'` against what you started | Every diff is empty and both `find` commands print nothing; no new worktree or branch under `~/Repos/.worktrees`; the LaunchAgent line is the same; the installed app's socket (`~/Library/Application Support/Overseer/overseer.sock`) was never opened by the rig (the rig's daemon listens on `/tmp/ov-hand6/overseer.sock`). A running installed app may touch its own files: if so say so and check they are its own | pending |

## Known limitations

- **Ruling 2 is a guard rail.** `ctl` reads `OVERSEER_SLOT` to tell a worker
  from Jonathan, so a session that unsets the variable gets past the checks
  that keep `propose_plan`, `request_status`, `set_board_summary` and
  `ctl plan approve` to S0 and Jonathan (P9), as the hook is a guard rail and
  not a sandbox. The tools are pre-approved for every slot; the daemon is what
  refuses a worker.
- **A running slot is told its worktree, not moved.** A plan entry for a
  running slot with a worktree creates it and relays the kickoff naming it;
  the session's own folder does not change (the plan warns, P4).
- **Sprint name and day wait for phase 7.** The board header has the running
  count, the sessions that need Jonathan and the done count only (P13).
- **One plan is pending at a time.** A new `propose_plan` replaces the old
  one (`Plan <n> is waiting for Jonathan. It replaces plan <m>.`).
- **Relays to S0 are queued only while S0 has a live session**; a plan
  decided with no S0 running tells nobody.
- **A step that fails skips only that slot's later steps**; what the earlier
  steps did (an assignment, a worktree, a claim) is not undone.

## Sign-off

Phase 6 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by: (Jonathan, once the rows pass)
- Date: (when the hand checks were run)
- Still pending: every row
