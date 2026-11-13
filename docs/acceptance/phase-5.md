# Phase 5 acceptance

Phase 5 (the 2ndBrain writer) is done only when every row below passes. Every
row runs on the home Mac against a copy of a test vault at
`/tmp/ov-hand5/vault` with a bare remote beside it, a temp state folder, a
temp socket and a temp copy of the app bundle. Never run any of it against the
real vault (`~/Repos/2ndBrain`), the installed app, the LaunchAgent
`uk.co.techize.overseer`, its socket, `~/Library/Application Support/Overseer`
or the real `~/.claude/CURRENT_SESSION.md`.

Unit and integration tests cover the logic (`cargo test --workspace`). They do
not replace a hand check where a row asks for one: rows marked "real session"
need a real Claude Code session, rows marked "GUI" need the app on screen,
and the rest are scripted from a terminal. The Result column is empty or
`pending` until Jonathan has run the row; write what was observed, and when.
The examples in this file (`DEMO-12`, `ov-3`, `demo-api`, `sprint-26-01`, the
`demo` label) are invented.

The setup checklist for the work laptop is `docs/setup/work-laptop-2ndbrain.md`.
The rows here check the build; the checklist is applied by hand there.

## Setup for the checks

Use one Terminal tab for every step so it keeps the variables.

**Risk, read first.** The real-session rows (D1 to D4, B1, B2, K1, M2, RL3,
G3) start a real `claude`. With your normal Claude settings that session would
also run your global hooks and skills (`pai-autocompact-save.sh`, `session-compact`,
`journal`, ...), which can commit `~/Repos/2ndBrain` or write the real
`~/.claude/CURRENT_SESSION.md`. So: run this whole list before checklist step 4
(the global hooks are not Overseer-aware yet and must not see these sessions),
and give the real sessions the isolation in step 5 (a throwaway
`CLAUDE_CONFIG_DIR` and a temp `HOME`, so none of your global settings, hooks
or `~/.claude.json` are read or written). Step 0 records the real vault and
`CURRENT_SESSION.md` so the last row (Z1) can prove nothing leaked.

### Step 0: record the real state

```
git -C ~/Repos/2ndBrain status --short > /tmp/ov-hand5-real-status.before
cp -L ~/.claude/CURRENT_SESSION.md /tmp/ov-hand5-real-cs.before
touch /tmp/ov-hand5-real-marker
find ~/Library/Application\ Support/Overseer -type f 2>/dev/null | sort > /tmp/ov-hand5-real-state.before
```

(`/tmp/ov-hand5-*` is outside `/tmp/ov-hand5/`, so `rm -rf /tmp/ov-hand5` at the
end does not remove them before Z1.)

### Step 1: state folder, binary, wrapper

Build from this branch's worktree before it is merged, or from the merged
`main` checkout after (`cd` to whichever you are testing):

```
export OVERSEER_HOME=/tmp/ov-hand5
export OVERSEER_CONFIG=$OVERSEER_HOME/config.toml
export OVERSEER_TARGETS=$OVERSEER_HOME/targets.toml
export OV=/tmp/ov-hand5/Overseer.app/Contents/MacOS/overseer
export V=$OVERSEER_HOME/vault
mkdir -p $OVERSEER_HOME/bin $OVERSEER_HOME/stubs $OVERSEER_HOME/work $OVERSEER_HOME/home $OVERSEER_HOME/claude-config
cd ~/overseer/.claude/worktrees/phase-5-plan   # or the main checkout once merged
cargo build --release -p overseer
scripts/bundle.sh target/release/overseer /tmp/ov-hand5/Overseer.app
```

The hook's `bd` rewrite runs the bare command `overseer ctl bd -- ...`, so a
session's shell must find a command named `overseer`:

```
printf '#!/bin/sh\nexec %s "$@"\n' "$OV" > $OVERSEER_HOME/bin/overseer
chmod +x $OVERSEER_HOME/bin/overseer
```

The daemon runs `bd` for its work list through the login shell's `PATH`, not
through `bd_command`, so put the stubs first on a temp `HOME`'s login `PATH`
(as phase 4 did) and use `SHELL=/bin/sh`:

```
printf 'export PATH=/tmp/ov-hand5/stubs:/tmp/ov-hand5/bin:$PATH\n' > $OVERSEER_HOME/home/.profile
export PATH=$OVERSEER_HOME/stubs:$OVERSEER_HOME/bin:$PATH
```

### Step 2: the test vault

A git repo with invented content, a bare remote, and a second clone that plays
"another machine":

```
git init -q --bare $OVERSEER_HOME/remote.git
git init -q -b main $V && cd $V
git config user.name demo && git config user.email demo@example.test
mkdir -p 06-Daily Sprints/sprint-26-01/DEMO-12-demo-lane Sessions 05-Meta/Sync-System Claude/MEMORY/State
printf '# Demo sprint\n' > Sprints/sprint-26-01/SPRINT-DASHBOARD.md
printf '# demo lane\n' > Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md
printf 'old history line 1\nold history line 2\n' > Claude/MEMORY/State/CURRENT_SESSION.md
printf 'one\ntwo\nthree\n' > scratch.md
printf 'free file\n' > free.md
ln -s $V/Claude/MEMORY/State/CURRENT_SESSION.md $OVERSEER_HOME/CURRENT_SESSION.md
```

`CURRENT_SESSION.md` is a symlink into the vault, as on the home Mac's real
setup. The two stubs (`bd` and the sync script) and the post-commit hook,
whose first line after the shebang is the one the checklist adds:

```
cat > $OVERSEER_HOME/stubs/bd <<'STUB'
#!/bin/sh
{ echo "argv: $*"; if [ "$1" != list ] && [ "$1" != sync ]; then echo "stdin: $(cat 2>/dev/null)"; fi; } >> /tmp/ov-hand5/bd.log
if [ "$1" = list ]; then
  echo '[{"id":"ov-3","title":"DEMO-12 wire the widget","status":"open","priority":1},{"id":"ov-4","title":"Standalone chore","status":"open","priority":2}]'
else echo "bd stub ok: $*"; fi
STUB
cat > 05-Meta/Sync-System/sync-run.sh <<'STUB'
#!/bin/sh
echo "sync at $(date +%H:%M:%S) OVERSEER_COMMIT=$OVERSEER_COMMIT" >> /tmp/ov-hand5/sync.log
STUB
printf '#!/bin/sh\n[ "$OVERSEER_COMMIT" = 1 ] && exit 0\necho "post-commit ran at $(date +%%H:%%M:%%S)" >> /tmp/ov-hand5/post-commit.log\n' > .git/hooks/post-commit
chmod +x $OVERSEER_HOME/stubs/bd 05-Meta/Sync-System/sync-run.sh .git/hooks/post-commit
git add -A && git commit -q -m 'seed' && git remote add origin $OVERSEER_HOME/remote.git && git push -q -u origin main
git clone -q $OVERSEER_HOME/remote.git $OVERSEER_HOME/other
git -C $OVERSEER_HOME/other config user.name other && git -C $OVERSEER_HOME/other config user.email other@example.test
: > $OVERSEER_HOME/bd.log; : > $OVERSEER_HOME/sync.log; : > $OVERSEER_HOME/post-commit.log
```

The stub `bd` never touches a real beads database. It logs every call (the
work list polls `bd list` every `beads_poll_secs`, so `grep -v 'argv: list'
$OVERSEER_HOME/bd.log` is the readable view). `sync-run.sh` is a tracked file
in the test vault, committed in the seed, so it does not show in `git status`;
if you change it, it shows as a free file and a backup may commit it.

### Step 3: config

`backup_mins = 1` keeps the backup rows short. `ticket_pattern` is set to the
invented `DEMO-` keys (the default matches `PLAT-<n>`, so without it the
stub's `DEMO-12` bead has no ticket and S1 would not own the demo lane
folder). The literal-string quotes on it keep the backslash:

```
cat > $OVERSEER_HOME/config.toml <<EOF2
brain_dir = "$V"
beads_dir = "$V"
jira_command = []
ticket_pattern = 'DEMO-\d+'
lock_idle_mins = 1
brain_writer = true
backup_mins = 1
current_session = "$OVERSEER_HOME/CURRENT_SESSION.md"
bd_command = ["$OVERSEER_HOME/stubs/bd"]
EOF2
```

`targets.toml` does not exist, so the compiled-in patterns are used.

### Step 4: names the rows use

```
D=$(date +%Y-%m-%d); J=$V/06-Daily/$(date +%Y)/$(LC_ALL=C date +%m-%B)/$D-journal.md
SD=$V/Sprints/sprint-26-01/SPRINT-DASHBOARD.md
N1=Sessions/$(date +%Y)/$(LC_ALL=C date +%m-%B)/$D-s1-demo.md   # S1's session note
N2=Sessions/$(date +%Y)/$(LC_ALL=C date +%m-%B)/$D-s2-demo.md
pre() { python3 -c 'import json,sys;print(json.dumps({"session_id":"s","tool_name":sys.argv[1],"tool_use_id":"tu-"+sys.argv[4],"cwd":sys.argv[3],"tool_input":json.loads(sys.argv[2])}))' "$2" "$3" "$V" "$RANDOM" | OVERSEER_SLOT=$1 $OV hook PreToolUse; }
```

`pre S1 Write '{"file_path":"'$SD'","content":"x"}'` sends a `PreToolUse`
event the way Claude does; a deny comes back as one JSON line. The note paths
follow the default `session_note` template; the board shows the real one in the
session panel (the line ending `(press n to open)`), and with a real `claude`
the role file names it (`grep 'session note' $OVERSEER_HOME/slots/S1/role.md`).
Use the real one if they differ.

### Step 5: start the app, isolated

Start from this tab with the isolation variables (a temp `HOME`, a login shell
that reads its `.profile`, and a throwaway Claude config folder):

```
HOME=$OVERSEER_HOME/home SHELL=/bin/sh CLAUDE_CONFIG_DIR=$OVERSEER_HOME/claude-config $OV &
```

The app starts the temp daemon itself with that environment. Scripted rows can
instead start the daemon alone with the same prefix and `$OV daemon &`. (The
`cargo` and `$OV` calls above still ran under your real `HOME`; only the app
and daemon get the temp one.)

A real session in this setup has no login of its own: in the first real
session's terminal view, run `/login` and sign in yourself (Overseer never
types credentials); that writes only under `$OVERSEER_HOME/claude-config`.
Answer Claude's folder trust prompt there too. No global settings, hooks or
skills from your real `~/.claude` are read, so nothing can commit the real vault.
In a real session run `env | grep OVERSEER` once: `OVERSEER_SLOT` and
`OVERSEER_HOME` must be there and `which overseer` must be the wrapper.

Stub sessions, to own a note without a real Claude:
`$OV ctl start 1 --cwd $OVERSEER_HOME/work --label demo -- sleep 3600` (and
`2`). Real sessions: the same without the `-- sleep 3600`.

**Pre-flight, before any assign.** `$OV ctl items` must show the stub's items,
not a real beads list, and no error:

```
beads  ok  2 items  HH:MM
ov-3  open  -  DEMO-12 wire the widget
ov-4  open  -  Standalone chore
```

(`ov-3` may be indented under a `DEMO-12` ticket line; the source line and the
two item lines with owner `-` are what matter.) If it shows anything else, the
daemon is not using the stub `PATH`: stop, fix, do not assign. Then
`$OV ctl assign 1 ov-3`; `ctl items` shows `ov-3  open  S1  ...`, `bd.log`
gets `argv: update ov-3 --add-label overseer:S1`, and S1 owns
`Sprints/sprint-26-01/DEMO-12-demo-lane/`. Rows that rely on that folder (Q1,
Q7, D3) check it first: `pre S2 Write` to a file in the folder must be denied
naming `S1 (DEMO-12)`.

### Step 6: finish

`$OV ctl shutdown`, quit the app, close the vault in Obsidian (row G3), then
run row Z1 before `rm -rf /tmp/ov-hand5 /tmp/ov-hand5-real-*`.

## Commit queue and git (Review Focus 1, 2, 5)

Scripted. Slots S1 and S2 running as stubs, `ov-3` assigned to S1, the writer
on. `OVERSEER_SLOT=S1 $OV ctl commit -m "<msg>" <paths>` is the session
caller; without `OVERSEER_SLOT` it acts as Jonathan.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| Q1 | Concurrent commits with unrelated edits in the vault (Review Focus 1) | In `$V`: edit tracked `scratch.md`, add untracked `unrelated.md`, delete tracked `free.md`. Write `$N1` and `Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md` (S1's) and `$N2` (S2's). Run in parallel: `OVERSEER_SLOT=S1 $OV ctl commit -m "S1 notes" $N1 Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md &` and `OVERSEER_SLOT=S2 $OV ctl commit -m "S2 note" $N2 &`; `wait` | Two commits, one per call. `git show --stat HEAD~1 HEAD` lists exactly each call's paths and nothing else. `git status --short` still shows only `scratch.md` modified (the tracked `sync-run.sh` is clean), `unrelated.md` untracked and ` D free.md`, byte for byte as left. No `index.lock` error. A deletion named in `paths` (repeat with `free.md`) lands in that commit | pending |
| Q2 | Rejected push, then rebase and push (Review Focus 2) | In `$OVERSEER_HOME/other`: commit a change to a file the vault does not touch, `git push`. Then `OVERSEER_SLOT=S1 $OV ctl commit -m "S1 after remote" $N1` with `$N1` edited | The call returns committed; `git log origin/main` in the vault shows both the other clone's commit and the S1 commit; `ctl brain` shows no push problem; the strip does not go orange | pending |
| Q3 | Conflicting push keeps everything and tells Jonathan (Review Focus 2) | In `other`: change line 1 of `Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md` (in that clone) and push. In the vault change the same line of the same file and `OVERSEER_SLOT=S1 $OV ctl commit` it | The rebase is aborted (`git status` shows no rebase in progress), the local commit stays (`git log -1`), the working tree has the S1 text, no conflict markers. `ctl brain` prints `push problem  2ndBrain push needs you: ...`; the strip reads `2ndBrain: push needs you` in orange. Fix by hand (`git pull --rebase`, resolve, `git push`); the next daemon commit or backup push succeeds and the problem clears | pending |
| Q4 | Autostash conflict is reported, not hidden | Like Q3 but the remote change is to `scratch.md` and the vault holds an uncommitted edit to the same line of `scratch.md`; then commit an unrelated path through the daemon | The push problem text says to resolve the conflicted files and drop the stash; Jonathan's text is still in the file (resolve by hand to recover); nothing reports a clean push | pending |
| Q5 | Odd paths (Review Focus 4) | `ctl commit` of, in turn: `../outside.md` (create it beside the vault); an absolute path outside (`/tmp/ov-hand5/outside.md`); a symlink inside the vault pointing out (`ln -s /tmp/ov-hand5/outside.md $V/link.md`); `"My Notes ü.md"` (spaces and non-ASCII, a real file); the folder `Sprints`; a vault-absolute path `$V/free.md`; a path with `n[1].md` next to a real `n1.md` | Outside, link and `../` are each refused with `<path as given> is outside 2ndBrain` and nothing outside the vault is committed or touched; the spaced and non-ASCII file commits under its exact name; the folder is refused with `Sprints is a folder`; the absolute vault path is normalised to `free.md`; `n[1].md` stages only `n[1].md`, never `n1.md` | pending |
| Q6 | Restart with a queued commit (Review Focus 5) | `touch $V/.git/index.lock`; in the background `OVERSEER_SLOT=S1 $OV ctl commit -m "S1 restart" $N1` (edit `$N1` first); within 5 s `$OV ctl shutdown`; `rm $V/.git/index.lock`; start the app or daemon again | The entry survives in the ledger and is committed exactly once after the restart: `git log --oneline \| grep -c "S1 restart"` prints 1, and the commit carries the `Overseer-Queue` trailer. `ctl brain` shows it committed, nothing queued | pending |
| Q7 | A path another slot owns is refused (S1 owns the DEMO-12 folder only after the Step 5 pre-flight) | As S2: `ctl commit -m x $N1`; as S2: `ctl commit -m x Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md` | Each fails with `<path> belongs to S1 (session note). Ask Jonathan before writing it.` and with `(DEMO-12)` for the ticket folder; as Jonathan (no `OVERSEER_SLOT`) the same paths commit | pending |
| Q8 | A stale `index.lock` is cleared after 60 s, a young one is waited on | `touch $V/.git/index.lock`, wait 65 s, `ctl commit` a changed path; then `touch` it again and `rm` it after 3 s while a `ctl commit` waits; then hold a fresh one for about 70 s with the loop under this table, so its mtime never gets old enough to be removed as stale | The old lock is removed, the commit lands, and the ledger log gets the line `removed a stale .git/index.lock (older than 60 s)` (source `2ndBrain`, kind `index.lock`); the lock released within 5 s is waited out, the commit lands and no such line is written; the one held about 70 s fails the first attempt with `index.lock held, will retry` and is retried after 10 s; with the first try that is 4 attempts (MAX_ATTEMPTS = 4, each waiting up to 5 s), so it ends with `index.lock still held after 4 attempts` (release it and the next commit works) | pending |

The refreshed lock for Q8 (run in a second tab, start the `ctl commit` just after it). It holds about 70 s because the four attempts start about 0, 15, 30 and 45 s (a 5 s lock wait, then a 10 s retry), so the lock must still exist at about 50 s:

```
touch $V/.git/index.lock
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14; do touch $V/.git/index.lock; sleep 5; done
rm -f $V/.git/index.lock
```

## Shared files and the journal (Review Focus 3)

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| J1 | `append_journal` creates and appends | As S1: `$OV ctl journal "Work Log" "first line"`; then `... "Work Log" "second line"` and `... "Decisions" "a decision"` | `$J` exists with the header `# Journal - <weekday>, <month> <day>, <year>` for today (read the weekday from `date`, not from this file), `## Work Log` with both lines in order, each preceded by a blank line, and a new `## Decisions` section at the end. Each call commits that file as `S1: journal, Work Log` / `S1: journal, Decisions` | pending |
| J2 | Em dashes in text become hyphens | `$OV ctl journal "Work Log" "$(printf 'a \xe2\x80\x94 b')"` (an em dash built with printf) | The file holds a hyphen; `grep -c $'\xe2\x80\x94' $J` prints 0 | pending |
| J3 | Stale `edit_shared_file` fails with the current text and keeps Jonathan's edit (Review Focus 3) | Read `$J`; then `printf '\nJonathan wrote this in Obsidian\n' >> $J`; then over MCP or a real session as S1: `edit_shared_file` on the journal with `old_text` from the earlier read | Error `old_text was not found; the file has changed. Current text:` followed by the file, which includes Jonathan's line; the file on disk is unchanged by the call | pending |
| J4 | An ambiguous `old_text` is refused | `edit_shared_file` on `$SD` with `old_text` `#` (occurs more than once after adding a second heading by hand) | `old_text occurs <n> times; include more context.`; file unchanged | pending |
| J5 | A good edit is committed | `edit_shared_file` on `$SD` with an exact one-time `old_text` | File changed; a commit `S1: edit SPRINT-DASHBOARD.md` holds only that file | pending |
| J6 | Two edits at once do not lose either | Two `ctl journal` calls to the same section from two terminals at the same instant (a loop of ten each) | The journal holds all twenty lines; none is torn or lost | pending |

## The hook: denies and what stays open

Scripted with `pre`, or from a real session where the row says so. A deny is
one JSON line with `permissionDecision` `deny` and the exact reason below;
allow is empty output and exit 0. S1 owns `ov-3`; S2 is a stub.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| D1 | Shared file, `Write` and `Edit` (real session) | In a real S1: ask `Use the Write tool to overwrite SPRINT-DASHBOARD.md in the sprint-26-01 folder of the vault with the word hello`; repeat with `Edit` on the journal | Denied with `Sprints/sprint-26-01/SPRINT-DASHBOARD.md is a shared 2ndBrain file. Use append_journal for the journal or edit_shared_file for an exact replacement.` (relative path as in the vault); Claude then uses a tool; the file is unchanged | pending |
| D2 | Another slot's session note (script, then real session) | `pre S1 Write '{"file_path":"'$V/$N2'","content":"x"}'`; then in a real S1 (S2 running) ask `Use the Write tool to write the word hi to <the vault path of S2's session note>` | Denied both ways: `<N2> belongs to S2 (session note). Ask Jonathan before writing it.`; in the real session Claude shows that reason and the file is not written | pending |
| D3 | Another slot's ticket folder (this is also the Step 5 check that S1 owns it) | First `$OV ctl items` shows `ov-3  open  S1`. Then `pre S2 Write '{"file_path":"'$V'/Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md","content":"x"}'` | Denied: `... belongs to S1 (DEMO-12). Ask Jonathan before writing it.`; the same write as S1 is allowed (empty output). In a real S2, ask `Use the Write tool to write hi to Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md in the vault`: Claude shows the same reason | pending |
| D4 | `CURRENT_SESSION.md` by either path (script, then real session) | `pre S1 Edit '{"file_path":"'$OVERSEER_HOME'/CURRENT_SESSION.md",...}'` and again with `'$V'/Claude/MEMORY/State/CURRENT_SESSION.md`; then in a real S1 ask `Use the Edit tool to change the word old to new in <that file>` once per path | Both denied: `CURRENT_SESSION.md is written by Overseer. Use update_current_session for your section.`; Claude shows it for each path; the file is unchanged (`cmp` with its copy) | pending |
| D5 | Own note and a free file are open | `pre S1 Write` to `$V/$N1` and to `$V/free.md` | Both allowed, empty output | pending |
| D6 | Bash writes into the vault | `pre S1 Bash` with `echo x >> $SD`, `tee -a $J`, `sed -i s/a/b/ $SD`, `cp /tmp/x $J`, `mv /tmp/x $SD`, `rm $SD`, `touch $J`, `truncate -s 0 $J`, `echo x > $OVERSEER_HOME/CURRENT_SESSION.md`; also `rm Sprints/sprint-26-01/*.md` from `cwd` `$V` | Each denied with the matching shared-file or `CURRENT_SESSION` text (the glob case is judged by what it expands to); `echo x > /tmp/ov-hand5/work/out.txt` is allowed | pending |
| D7 | Git in the vault | `pre S1 Bash` with `git commit -m x`, `git push`, `git add -A`, `git -C $V commit -m x`, `cd $V && git push`; then `git status`, `git log --oneline`, `git diff`, `git show HEAD`, `git describe`, `git branch -a`, `git remote -v`, `cat $J` | The first five denied with `use the commit_2ndbrain tool; Overseer commits and pushes 2ndBrain`; the read-only ones allowed; a `git push` in `$OVERSEER_HOME/work` (not the vault) follows the phase 4 rules, not this deny | pending |
| D8 | Known limit: other read-only git in the vault is denied | `pre S1 Bash` with `git grep a`, `git reflog`, `git stash list`, `git branch -d x` from `$V` (the allow list is `status`, `log`, `diff`, `show`, `blame`, `rev-parse`, `ls-files`, `describe`, `remote -v`, and `branch` with only listing flags) | Each denied with the same text. This is a documented limit (the checklist), not a bug: run them in your own terminal | pending |
| D9 | Read-only work is never blocked or slowed | `cat $J`, `ls $V`, `git log` from `$V` as S1, 20 runs of `time` | Allowed, empty output, each under 200 ms | pending |
| D10 | A broken daemon fails open | Stop the daemon; run `pre S1 Write` to `$SD` | Empty output, exit 0 within 1 s (phase 4 behaviour kept) | pending |

## bd writes (Claude Code 2.1.259)

The rewrite relies on `hookSpecificOutput.updatedInput` from a `PreToolUse`
hook. Record `claude --version` in the first Result. The stub `bd` logs
`argv:` and `stdin:` lines to `$OVERSEER_HOME/bd.log`.

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| B1 | A plain `bd` write is rewritten and run under the lock (real session) | In a real S1: `Run exactly: bd update ov-3 --status in_progress`. Also scripted: `pre S1 Bash '{"command":"bd close ov-3","timeout":5000,"description":"close it"}'` | The hook output holds `updatedInput` with `command` `overseer ctl bd -- close ov-3` and the original `timeout` and `description` kept; in the real session Claude shows `bd stub ok: update ov-3 --status in_progress`, `bd.log` has one `argv: update ov-3 --status in_progress` line (ignore `argv: list` polls), no permission prompt appeared for the rewritten command beyond the ordinary one | pending |
| B2 | A heredoc with `--body-file -` lands with its body | Ask S1 to run, as one command: `bd create "demo" --body-file - <<'EOF'` with two lines of text and `EOF` | `bd.log` shows `argv: create demo --body-file -` and a `stdin:` holding both lines | pending |
| B3 | `$(...)` on a `bd` write line is denied | `pre S1 Bash '{"command":"bd create \"x $(date)\""}'`; also a backtick form, `${HOME}`, `bd create x > /tmp/f`, `FOO=1 bd create x` | Each denied with `Overseer: run bd writes on their own, without $(...), backticks, redirects or env prefixes, so the daemon can run them under the vault lock`; no `argv: create` line in `bd.log` | pending |
| B4 | `ctl bd` with no `-` argument does not hang | `sleep 30 \| $OV ctl bd -- update ov-3 --status open` and `time` it | Returns within a second with `bd stub ok: ...` and the stub's exit code; `stdin:` in the log is empty | pending |
| B5 | A compound line is denied | `pre S1 Bash '{"command":"bd close ov-3 && echo done"}'` | Denied (the same text as B3) | pending |
| B6 | Reads are untouched | `pre S1 Bash` with `bd list`, `bd show ov-3`, `bd ready` | Allowed, empty output (no rewrite) | pending |
| B7 | `bd sync` runs after a write | After B1, wait 15 s | `bd.log` has an `argv: sync` line after the write, once | pending |
| B8 | Rewrite stops working: the hook must deny | Only if B1 fails on a future Claude Code: record what Claude did with the rewritten command | Documented here; the fallback is a deny, not an unlocked write | pending |

## The tools over MCP

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| M1 | The tool list | Pipe `initialize`, `notifications/initialized`, `tools/list` to `OVERSEER_SLOT=S1 $OV mcp` (as phase 4 M1) | The list has `commit_2ndbrain`, `append_journal`, `edit_shared_file`, `update_current_session` besides the phase 4 tools | pending |
| M2 | Real session uses them | In a real S1: `Use append_journal to add "hand check" to the Work Log, then update_current_session with "working on the demo", then commit_2ndbrain for your session note with the message "note"` | Each call succeeds without a permission prompt; the journal line, the S1 section (row C1) and a commit `note` are there | pending |
| M3 | A `Queued` reply is success | Make a commit wait (`touch $V/.git/index.lock` for a few seconds) and call `ctl commit`; repeat through MCP | `ctl` prints the reply and exits 0; MCP returns it as text with `isError` false; after the lock goes the commit lands once | pending |
| M4 | No slot | `$OV mcp` with `OVERSEER_SLOT` unset, call `update_current_session`; `ctl current-session "x"` with no `--slot` | The MCP tool says the text `update_current_session needs a slot; use overseer ctl current-session --slot S<n>`; `ctl current-session "x"` fails with `name the slot with --slot S<n>` | pending |

## CURRENT_SESSION.md sections

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| C1 | Two sessions, one section each, history kept | `cp $V/Claude/MEMORY/State/CURRENT_SESSION.md /tmp/ov-hand5/cs.before`; `OVERSEER_SLOT=S1 $OV ctl current-session "S1 text"`; then S2 with `"S2 text"` | The file starts `<!-- overseer:sections -->`, then `<!-- overseer:S1 -->`, `## S1 demo`, text, `<!-- /overseer:S1 -->`, the same for S2, then `<!-- /overseer:sections -->`; everything below equals `cs.before` byte for byte (`tail -c $(wc -c < /tmp/ov-hand5/cs.before)` and `cmp`) | pending |
| C2 | The link stays a link, and the file is in the vault | `readlink $OVERSEER_HOME/CURRENT_SESSION.md`; `git -C $V status --short` | Still a symlink into the vault; the vault file is modified; no stray `.tmp` file beside it | pending |
| C3 | A section is replaced, not appended | `OVERSEER_SLOT=S1 $OV ctl current-session "S1 newer"` | S1's block holds only the new text; S2's block untouched | pending |
| C4 | A new session in the slot clears its section | `$OV ctl stop 1` then start S1 again | S1's block is gone; S2's stays; history intact | pending |
| C5 | Marker text in a section cannot break the layout | `ctl current-session` with a text containing `<!-- /overseer:S1 -->` and `<!-- overseer:S9 -->` | The file still parses (a second write by S2 works and S1's block is whole) | pending |
| C6 | The backup may commit it | Wait for the backup tick after C1 | `CURRENT_SESSION.md` appears in a `vault backup: ...` commit like any free vault file; this is accepted on this setup | pending |

## Backup and sync

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| K1 | The backup leaves a working slot's file alone | Real S1 working (`Run sleep 150 with Bash`); S1's note has an uncommitted edit; `free.md` also edited; wait for a tick (`backup_mins = 1`) | A `vault backup: <date> <HH:MM>` commit holds `free.md` and not S1's note; S1's note is committed by a later tick once S1 is idle | pending |
| K2 | A queued entry's paths are not swept into the backup | Hold an entry queued (`touch $V/.git/index.lock`), edit its path, wait for a tick, release the lock | The queued entry's own message (`S1 ...`) is on the commit, not `vault backup` | pending |
| K3 | The sync runs once after the queue drains | Q1's two commits, then wait 30 s; read `sync.log` and `post-commit.log` | One `sync at ... OVERSEER_COMMIT=1` line for the burst; `post-commit.log` has no new line from the daemon's commits | pending |
| K4 | Your own commit still runs the hook as before | In `$V`: `git commit --allow-empty -m by-hand` in a terminal | `post-commit.log` gets a line (no `OVERSEER_COMMIT`); the daemon does not mind | pending |
| K5 | The sync waits while a running slot owns an uncommitted path | Leave an uncommitted edit in S1's note (S1 running); let a queue drain | No sync line until the path is committed or S1 stops; then one | pending |
| K6 | Writer off: nothing runs | `brain_writer = false`, restart; edit the journal by hand; `ctl commit`; `pre S1 Write` to `$SD`; wait two minutes | `ctl commit` and the tools answer `the 2ndBrain writer is off (brain_writer in config.toml)`; the write is allowed; no backup or sync; the strip reads `2ndBrain: writer off` (grey) | pending |

## Roles and resume

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| RL1 | Role upgrade from the phase 4 role | Stop the daemon; `mkdir -p $OVERSEER_HOME/roles`; copy `crates/overseer-daemon/tests/fixtures/roles-phase4/*` there; start; then start a session in S0 and one in S1 (the role files are checked when a session starts, not at daemon start); `grep -E 'role template|brain_rules' $OVERSEER_HOME/logs/daemon.log` | `$OVERSEER_HOME/logs/daemon.log` says `upgraded role template ...` for both files; no `.new` files; `$OVERSEER_HOME/slots/S1/role.md` has the 2ndBrain rules, S1's note path and the `bd` line with `${`; no `{brain_rules}` text left | pending |
| RL2 | An edited role is kept, with a warning | Copy the phase 4 `worker.md` back, add one line of your own; restart; start a session in S0 and one in S1; `grep -E 'role template|brain_rules' $OVERSEER_HOME/logs/daemon.log` | `worker.md` is unchanged; `worker.md.new` holds the current default; the log has `role template <path>/worker.md was edited; the current default is in <path>/worker.md.new` and `role worker.md is edited and has no {brain_rules}: add it, see worker.md.new`; S1's `role.md` has no 2ndBrain rules (the hook's deny texts still apply). Adding a `{brain_rules}` line and restarting gives the rules | pending |
| RL3 | A resumed slot is told its original note path | Start a real S1 and note the path in `role.md`; `$OV ctl shutdown`; `sqlite3 $OVERSEER_HOME/ledger.db "update sessions set session_note='Sessions/2026/11-November/2026-11-12-s1-demo.md' where slot=1"` (yesterday's date; this simulates the next day); start, accept the resume | `slots/S1/role.md` names the ledger's path (`...2026-11-12-s1-demo.md`), not today's; the hook allows S1 to write that note and denies S2 | pending |
| RL4 | The coordinator role gets the rules too | Start a session in S0 | `slots/S0/role.md` has the coordinator's 2ndBrain rules | pending |

## Board, notification and keys (GUI, by hand)

Setup steps 1 to 5 with the app open; S1 running (real, for `n`).

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| G1 | The strip text in each state | Fresh start; after a commit; with an entry held queued (`touch $V/.git/index.lock`); during a push problem (Q3); with `brain_writer = false` | `2ndBrain: nothing yet`, `2ndBrain: committed HH:MM`, `2ndBrain: 2 queued` (count as held), `2ndBrain: push needs you` in orange, `2ndBrain: writer off` in grey | pending |
| G2 | Hover shows the last five entries | Make seven commits; hover the strip | The newest five, one line each `HH:MM S1  path  committed, pushed` (or the failure), newest first; empty shows `Nothing queued` | pending |
| G3 | `n` opens the session note in Obsidian | Open `$V` once as a vault in Obsidian first. Focus S1's panel and press `n`; then Cmd+Shift+N from the board | Obsidian opens S1's note; the panel line reads `<note> (press n to open)`; with `brain_dir` unset (or the writer off) the line has no hint and the key does nothing | pending |
| G4 | A push problem notifies once per problem | Cause the Q3 conflict with the app in the background | One notification titled `2ndBrain` with the first error line; a second failed push for the same problem does not repeat it; clicking it brings the board forward and does not open S0 or any slot; a new, different problem notifies again | pending |
| G5 | A push problem present at app start notifies once | With the Q3 problem still unresolved, quit the app (daemon keeps running) and reopen it | Exactly one `2ndBrain` notification at connect; none repeated while it stays open | pending |
| G6 | Idle board | With the writer on and nothing happening for 5 min | The strip stays steady; no flicker under the pointer when hovering | pending |

## Nothing leaked

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| Z1 | The real vault, the real `CURRENT_SESSION.md` and the installed app's state are untouched | After every other row, before the cleanup (do not edit the real vault in Obsidian during the run, or the diff is a false alarm): `git -C ~/Repos/2ndBrain status --short \| diff - /tmp/ov-hand5-real-status.before`; `diff <(cat ~/.claude/CURRENT_SESSION.md) /tmp/ov-hand5-real-cs.before`; `git -C ~/Repos/2ndBrain log -3 --format=%h\ %s` against what you saw before; `find ~/Library/Application\ Support/Overseer -type f -newer /tmp/ov-hand5-real-marker`; `find ~/Library/Application\ Support/Overseer -type f \| sort \| diff - /tmp/ov-hand5-real-state.before` | Both diffs empty; no new commit in the real vault; both `find` commands print nothing (no file changed or appeared; a running installed app may touch its own files, in which case say so and check they are its own) | pending |

## Known flake to watch

| ID | Check | Steps | Pass | Result |
|----|-------|-------|------|--------|
| X1 | Phase 4 timing tests fail once under full-suite load and pass alone | `cargo test --workspace` three times; then `cargo test -p overseer --test hook` and `cargo test -p overseer-daemon --test server permission_prompt_never_gets_a_relay -- --test-threads=1` three times | Passes alone every time. Known: the `hook.rs` tests with their 200 ms limits and `server.rs` `permission_prompt_never_gets_a_relay` (line 2775 when it failed) each failed once during a full workspace run in phase 5 and passed alone. They are phase 4 timing tests, not phase 5 behaviour. Record any further failure here with its output; two more in a row means the limits need loosening | pending |

## Known limitations

- **The hook is guard rails.** It watches the tool calls and a set of Bash
  shapes; a script a session writes and runs can write anywhere.
- **Read-only git outside the allow list is denied in the vault**
  (`git grep`, `git reflog`, `git stash list`, ...): D8. The allow list is
  `status`, `log`, `diff`, `show`, `blame`, `rev-parse`, `ls-files`,
  `describe`, `remote -v` and `branch` with only listing flags.
- **The `bd` rewrite depends on `updatedInput`.** B1 and B8; re-run after a
  Claude Code upgrade.
- **`CURRENT_SESSION.md` may be in the vault** (it is a link into it on this
  machine), so the backup can commit it (C6). `readlink` it on the work
  laptop first.
- **The strip says `push needs you` for a retryable push failure as well**;
  the notification text says which.
- **A held sync or commit refusal is in the daemon log, not the ledger.**
- **Sync is skipped while a running slot owns an uncommitted path**; it runs
  at the next drain or backup tick.

## Sign-off

Phase 5 is done when every Result above reads as passed. Record the date and
any failures here.

- Signed off by:
- Date:
- Open follow-ups:
