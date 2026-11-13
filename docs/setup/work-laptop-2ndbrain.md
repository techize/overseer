# 2ndBrain writer: work laptop setup checklist

Phase 5 makes the Overseer daemon the only committer of the 2ndBrain vault and
the only writer of its shared files and of `CURRENT_SESSION.md`. The writer is
off by default (`brain_writer = false`), so installing the new build changes
nothing until you finish this list and turn it on in step 6. Nothing here is
done by code: each step changes something outside the Overseer repo (the
vault's git hooks, Obsidian's settings, the global hooks and skills) and is
applied by hand, with your approval, one at a time.

Run the acceptance list (`docs/acceptance/phase-5.md`) on the home Mac before
step 4: it starts real sessions, and a global hook that is not Overseer-aware
yet must not see them. Do the steps in order. Steps 1 to 4 are safe with the writer still off. Do not
do step 6 until 1 to 5 are done, or Obsidian Git and the daemon will both
commit the same vault.

The vault path below is `~/Repos/2ndBrain`; use yours if it differs, and make
the same path `brain_dir` in `config.toml`.

## 0. Look at this machine first

- `readlink ~/.claude/CURRENT_SESSION.md`. On the home Mac it is a symlink
  into the vault (`2ndBrain/Claude/MEMORY/State/CURRENT_SESSION.md`). It may
  differ on the work laptop (a plain file, or a link somewhere else). What
  this means for the writer:
  - If it points into the vault, `CURRENT_SESSION.md` is a vault file. The
    daemon writes the slot sections through the link and keeps it a link (it
    writes the link's target, it never replaces the link with a file). The
    hook judges both paths as `CURRENT_SESSION`, so a session cannot edit it
    through the vault path either. The timed backup may commit it like any
    other free file in the vault.
  - If it is a plain file outside the vault, it is never committed, as before.
  - If it does not exist, the first `update_current_session` creates it.
  - Do not point `current_session` in `config.toml` at a different file from
    the one your global hooks and skills use.
- `git -C ~/Repos/2ndBrain status` is clean, or you know what is dirty. The
  first backup after step 6 commits any free change in the vault.
- Close any Obsidian Git "commit all" you have running by hand. It is turned
  off properly in step 1.

## 1. Obsidian Git: stop it committing and pushing

In Obsidian, Settings, Community plugins, Git:

- Auto commit-and-sync interval: `0` (setting `autoSaveInterval: 0` in
  `.obsidian/plugins/obsidian-git/data.json`).
- Auto push interval: `0` (`autoPushInterval: 0`).
- Auto pull interval: `0`. The daemon pulls (`git pull --rebase --autostash`)
  whenever a push is rejected, under its own lock, so a second puller only
  races it.
- Leave "commit on close" and any "pull on startup" off as well.

Restart Obsidian, then confirm in `data.json` that the keys read `0`. Obsidian
Git can still be used by hand (the command palette); it just must not run on
a timer.

## 2. The vault's post-commit hook

Edit `~/Repos/2ndBrain/.git/hooks/post-commit`. The first line after the
shebang is:

```
[ "$OVERSEER_COMMIT" = 1 ] && exit 0
```

This stops the hook starting `sync-run.sh --auto` in the background after a
daemon commit. The daemon runs the sync itself, once, after its queue drains.
Your own commits in a terminal (without `OVERSEER_COMMIT=1`) behave as now.
`.git/hooks` is not part of the repo, so this change is not committed.

Check: `head -3 ~/Repos/2ndBrain/.git/hooks/post-commit`, and that the file is
still executable.

## 3. sync-run.sh stays as it is

Leave `05-Meta/Sync-System/sync-run.sh` alone. The daemon starts it with
`OVERSEER_COMMIT=1` set, with the vault lock held, once the commit queue has
been quiet for 10 seconds and no running session owns an uncommitted path.
Its own commit's `post-commit` then hits the line from step 2 and does not
start a second sync. The default `sync_command` is
`["05-Meta/Sync-System/sync-run.sh", "--auto"]`; set it in `config.toml` only
if the script lives elsewhere. A missing script is skipped.

## 4. The global hooks and skills

These write the vault or `CURRENT_SESSION.md` directly today. Each one needs a
branch for sessions under Overseer, which are the ones with `OVERSEER_SLOT`
set in their environment. With no `OVERSEER_SLOT` each stays exactly as it is
now. Change each in its own place, one at a time, with your approval; none of
them is in the Overseer repo.

If one is left unchanged, its own git calls and file writes are not seen by
the `PreToolUse` hook (they run in the hook or skill process, not as a tool
call), so under Overseer it would commit around the daemon's queue and lock.
That is what this phase exists to stop, so do all of them before step 6.

The list (spec 7.8.1):

| What | Where | Today | Under Overseer |
|------|-------|-------|----------------|
| `pai-autocompact-save.sh` | `~/.claude/hooks/` | commits 2ndBrain, writes `CURRENT_SESSION.md` and the journal | `ctl commit`, `ctl current-session`, `ctl journal` |
| `git-cmd.js` | `~/.claude/hooks/lib/` | runs `git add` and `git commit` in the vault | `ctl commit` |
| `session-compact` | `~/.claude/skills/` | journal, session log, `CURRENT_SESSION.md`, memory entries, commit | all three |
| `journal` | `~/.claude/skills/` | appends to the journal | `ctl journal` |
| `land-the-plane` | `~/.claude/skills/` | closing commit and push | `ctl commit` |
| Anything similar | `grep -rln 'CURRENT_SESSION\|2ndBrain' ~/.claude/hooks ~/.claude/skills` | | the same three |

Re-run that grep after the edits and look at every remaining hit by hand.

The commands the snippets call (all talk to the daemon over its socket and
take the slot from `OVERSEER_SLOT`):

- `overseer ctl commit -m "<message>" <path>...` queues a commit of exactly
  those paths. Paths are relative to the vault. It prints the daemon's reply
  and exits 0, including when the reply is `Queued` (the entry is saved and
  will be committed; do not retry it). Exits non-zero with the reason when
  the path belongs to another slot or is refused.
- `overseer ctl journal "<section>" "<text>"` adds text to a section of
  today's journal. Text `-` reads it from stdin.
- `overseer ctl current-session "<text>"` sets this slot's section of
  `CURRENT_SESSION.md` (`--slot S<n>` to name it when `OVERSEER_SLOT` is not
  set). Text `-` reads it from stdin. It replaces the slot's section; it does
  not append.
- `overseer ctl bd -- <bd args>` runs a bd write under the vault lock. A
  session's own `bd` writes need no change: the hook rewrites them.

Snippet for a shell hook or skill script. `ctl journal` commits its own file, so do not add a separate commit for the
journal. Keep the original commands in the `else` branch unchanged:

```sh
if [ -n "$OVERSEER_SLOT" ]; then
  overseer ctl commit -m "$MSG" Sessions/2026/11-November/2026-11-13-s1-demo.md   # a file you wrote, not the journal
  overseer ctl journal "Work Log" "$LINE"
  printf '%s\n' "$STATE" | overseer ctl current-session -
else
  # the existing commit, journal and CURRENT_SESSION code, unchanged
  :
fi
```

Snippet for `git-cmd.js` (Node); it goes at the top of the function in that
file that runs git, where `message` and `paths` are that function's inputs:

```js
const { spawnSync } = require('node:child_process');
if (process.env.OVERSEER_SLOT) {
  // paths relative to the vault
  const r = spawnSync('overseer', ['ctl', 'commit', '-m', message, ...paths],
    { encoding: 'utf8' });
  return r.status === 0 ? r.stdout : Promise.reject(new Error(r.stderr || r.stdout));
}
// existing git add / git commit path continues here, unchanged
```

Adjust the return and error handling to what `git-cmd.js` expects of its
callers; the branch on `OVERSEER_SLOT` and the `ctl commit` call are the
point. A `Queued` reply is success.

Notes for the edits:

- Use the vault's relative paths, not absolute ones, in `ctl commit`.
- Section text for `ctl current-session` is the slot's own state only. The
  file's old history below the slot sections is kept byte for byte.
- A hook that runs with no slot (Jonathan's own terminal) is untouched.

## 5. Check `config.toml` and the roles

`~/.config/overseer/config.toml` (or `$OVERSEER_CONFIG`). Add:

```
brain_writer = true
```

Nothing else is needed if the defaults suit the vault. The keys, with their
defaults:

- `brain_dir = "~/Repos/2ndBrain"`
- `shared_files = ["06-Daily/*/*/*-journal.md", "Sprints/*/SPRINT-DASHBOARD.md", "Sprints/*/sprint-*.md"]`
- `journal_path = "06-Daily/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-journal.md"`
- `session_note = "Sessions/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-{slot}-{label}.md"`
- `current_session = "~/.claude/CURRENT_SESSION.md"` (as the link target
  decides in step 0; a link into the vault is fine)
- `backup_mins = 5` (0 turns the timed backup off)
- `push = true`
- `sync_command`, `bd_command` (defaults `["05-Meta/Sync-System/sync-run.sh", "--auto"]`
  and `["bd"]`)

The daemon keeps two role files in its state folder,
`~/Library/Application Support/Overseer/roles/`. They are checked when a session starts in a slot, not when the daemon
starts. So after turning the writer on (step 6), start a session in S0 and one
in S1, then `grep -E 'role template|brain_rules' ~/Library/Logs/Overseer/daemon.log`:

- `worker.md` and `coordinator.md`, if you have never edited them, are
  upgraded to the current text (the log says `upgraded role template ...`).
  Nothing to do.
- If you have edited either one, it is kept as it is, and the new default is
  written beside it as `worker.md.new` or `coordinator.md.new`. The log has
  `role template <path> was edited; the current default is in <path>.new`.
  If that role also has no `{brain_rules}` line, you also see
  `role worker.md is edited and has no {brain_rules}: add it, see worker.md.new`,
  and its sessions get no 2ndBrain rules at all: they would not know about
  `commit_2ndbrain`, the shared-file tools or their own CURRENT_SESSION
  section, and would meet only the hook's deny texts. Add a line holding just
  `{brain_rules}` where the rules belong (see the `.new` file), by hand, and
  start new sessions.
- No such lines means both roles are fine.

## 6. Turn it on

1. Quit any running Overseer sessions that matter (a restart stops them; they
   come back as resumable).
2. Restart the daemon: `overseer ctl shutdown`, then open the app (or
   `launchctl kickstart -k gui/$(id -u)/uk.co.techize.overseer` if the
   LaunchAgent is installed).
3. `overseer ctl brain` should say `2ndBrain writer is on`.
4. Do the role check from step 5 now (start a session in S0 and one in S1,
   then grep the log).
5. The board's bottom strip should read `2ndBrain: nothing yet`, then
   `2ndBrain: committed HH:MM` after the first commit. See the acceptance list
   (`docs/acceptance/phase-5.md`) for the checks to run on the home Mac before
   doing this on the work laptop.

To turn it off again: `brain_writer = false` and restart. The hook stops
applying this phase's checks and the backup and sync stop; Obsidian Git and
the global hooks then need switching back by hand.

## Known limits

- The hook is best effort around the Bash tool. It sees redirects, `tee`,
  `sed -i`, `perl -i`, `cp`, `mv`, `rm`, `touch` and `truncate` aimed at the
  vault. A script that a session writes and runs can still write anywhere.
  This is guard rails, not a sandbox.
- Inside the vault the hook allows only `git status`, `log`, `diff`, `show`,
  `blame`, `rev-parse`, `ls-files`, `describe`, `remote -v` and `branch` with
  only listing flags (`-a`, `-r`, `-v`, `-vv`, `--list`, `--show-current`).
  Other read-only commands (`git grep`, `git reflog`, `git stash list`, ...)
  are denied there with `use the commit_2ndbrain tool; Overseer commits and
  pushes 2ndBrain`. Run them in your own terminal, which the hook never sees.
- A `bd` write on a line with `$(...)`, backticks, `${`, a file redirect or an
  env prefix is denied; run it as a plain line. Long text goes in with
  `--body-file -` and a quoted heredoc (`<<'EOF'`). `ctl bd` reads stdin only
  when an argument is `-` or ends in `=-`.
- If a Claude Code release stops honouring `updatedInput` from a hook, the
  `bd` rewrite stops working. The acceptance row B1 checks this on the
  installed version (2.1.259 at the time of writing); re-run it after a Claude
  Code upgrade.
- The push-problem notice reads `2ndBrain: push needs you`. It also shows for
  a push that failed for a retryable reason; the notice text says which.
