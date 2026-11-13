# Phase 5: 2ndBrain Writer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The daemon becomes the only committer to the 2ndBrain vault and the only writer of its shared files and of `CURRENT_SESSION.md`. Sessions commit through a serialised pathspec queue, edit shared files only through stale-safe tools, keep one section each in `CURRENT_SESSION.md`, and are stopped by the hook from writing around any of it. The daemon also runs the timed backup, the vault sync and `bd` writes under the same git lock, so the four daily symptoms (git failures, lost content, swept-up half edits, cross-session confusion) stop.

**Architecture:** A new `brain` module in the daemon. `git.rs` runs every git command in the vault behind one async mutex (pathspec commits, `index.lock` waits, rebase on a rejected push). `paths.rs` classifies a path as shared, owned by a slot, `CURRENT_SESSION`, free or outside the vault. `shared.rs` and `current.rs` apply journal appends, exact-replacement edits and section updates under per-file locks. `BrainService` (in `mod.rs`) owns the commit queue in the ledger (schema v4), the timed backup, the sync run after the queue drains and the `bd` runner, and announces a `BrainView`. The `PreToolUse` hook gains `Edit|Write|MultiEdit|NotebookEdit`, Bash redirects into the vault, git in the vault and `bd` writes. `overseer mcp` gains four tools and `ctl` gains the matching commands that the global hooks and skills will call.

**Tech Stack:** Rust, tokio (`process`, `sync`, `time`), rusqlite (bundled), `toml` 0.8, `regex` 1, `serde_json`, `libc` for local time, the `git` and `bd` binaries, GPUI 0.2.2.

**Spec:** `docs/superpowers/specs/2026-11-09-overseer-design.md`, section 7.8 and 7.8.1 (the core), 7.1 (`ctl` commands), 7.2 (role prompt rules), 7.9 (bottom strip queue status, `n` opens the session note), 7.10 (ledger, `config.toml` shared-files list), 8 (2ndBrain writer tests) and 9 item 5.

## Global Constraints

- The repo is public. No real company names, ticket keys, sprint names, hostnames or vault content in defaults, tests, fixtures or docs. Invented examples only (`DEMO-12`, `ov-3`, `demo-api`, `sprint-26-01`, `Sprints/sprint-26-01/DEMO-12-demo-lane/`).
- Never touch `~/.claude/settings.json`, `~/.claude.json`, the global hooks or skills, or the vault's `.git/hooks` from code (spec 4). Changes outside this repo are a setup checklist for Jonathan to apply (Task 10).
- Tests never touch the installed app, the LaunchAgent `uk.co.techize.overseer`, its socket, `~/Library/Application Support/overseer`, the real `~/Repos/2ndBrain` or the real `~/.claude/CURRENT_SESSION.md`. Every git test runs on a throwaway repo under the temp dir with a bare repo as its remote; `bd` and `sync-run.sh` are stubs.
- Read-only work is never blocked or slowed (spec 4). A read of any vault file, `git log`, `git status`, `git diff` and `bd list|show|ready|search|stats` are never matched.
- A broken daemon never stops work: the hook fails open exactly as in phase 4 (1 s budget, allow, no output, exit 0).
- Nothing acts silently (spec 4): every commit, push, backup, sync, deny and failure is in the ledger and on the board.
- Commits stage only the paths named, with a pathspec (`git add -A -- <paths>` then `git commit -- <paths>`), never `git add -A` or `git commit -a`.
- Commit messages the daemon writes are human style, with no AI language, no emojis and no em dashes. A session's own message is used as given apart from removing em dashes.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass at the end of every task.
- The protocol stays backward compatible: every new field on an existing struct or request uses `#[serde(default)]`.

## Rulings (gaps this plan fills)

1. **The writer is opt-in.** `brain_writer = false` by default in `config.toml`. While off, the four tools and `ctl` commands answer `the 2ndBrain writer is off (brain_writer in config.toml)`, the hook applies none of this phase's checks, and no backup or sync runs. Jonathan turns it on after the setup checklist (Task 10), so upgrading never starts committing a vault that Obsidian Git still commits too. Cost if wrong: one config line on the work laptop.
2. **Config keys** (all relative paths are under `brain_dir`):
   - `shared_files = ["06-Daily/*/*/*-journal.md", "Sprints/*/SPRINT-DASHBOARD.md", "Sprints/*/sprint-*.md"]`, globs where `*` matches one path segment.
   - `journal_path = "06-Daily/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-journal.md"`.
   - `session_note = "Sessions/{yyyy}/{mm}-{Month}/{yyyy}-{mm}-{dd}-{slot}-{label}.md"`, with `{slot}` lower case (`s3`) and `{label}` slugged (lower case, `[a-z0-9-]` only, empty becomes `session`).
   - `current_session = "~/.claude/CURRENT_SESSION.md"` (outside the vault, never committed).
   - `backup_mins = 5` (0 turns the backup off), `push = true`, `sync_command = ["05-Meta/Sync-System/sync-run.sh", "--auto"]` (skipped when the file does not exist), `bd_command = ["bd"]`.
3. **Ownership** (spec 7.8 item 6). A slot owns its session note (fixed when the slot starts, stored in the ledger, kept on resume) and, under `Sprints/*/`, the folder of every ticket or standalone bead it owns (`WorkItem.folder`). A bead's owner also owns its parent ticket's folder, since a split-off bead works in that folder. A path two slots own is owned by both. A path nobody owns is free. Ownership is checked against slots that are running or resumable.
4. **The shared tools commit their own edits.** `append_journal` and `edit_shared_file` queue a commit of that file at once, with the message `S3: journal, Work Log` or `S3: edit SPRINT-DASHBOARD.md`. Jonathan's own Obsidian edits to a shared file are picked up by the backup.
5. **Queue.** One FIFO in the ledger (`brain_queue`). Each entry is committed alone, in order, with the vault mutex held for the commit and its push. A commit whose paths have no changes is recorded as `nothing to commit` and is not an error. The caller's tool call returns once its entry is committed (or failed), not when it is pushed; the push result shows on the board.
6. **Push.** After each commit, if `push`: `git push`. If rejected, `git pull --rebase --autostash` then push once more. If the rebase stops on a conflict: `git rebase --abort`, keep the local commit, mark the queue entry `pushed: false` with the error, raise a notice for Jonathan (`2ndBrain push needs you: <first error line>`), and try again after the next commit or backup. Content is never dropped and the working tree is never reset.
7. **`index.lock`.** Wait up to 5 s for it to go. If it is still there and its mtime is more than 60 s old, remove it and record a notice saying so (age is the only test; no reliable way exists to tell which process holds it). Under 60 s, the entry fails with `index.lock held, will retry` and is retried after 10 s, at most 3 times.
8. **Sync.** Daemon commits run with `OVERSEER_COMMIT=1` in git's environment, so a `post-commit` hook patched per the checklist does nothing. Once the queue has been empty for 10 s after a commit, the daemon runs `sync_command` once, with the mutex held and `OVERSEER_COMMIT=1` set, but only when `git status --porcelain` shows no path owned by a running slot (else it waits for the next drain or backup tick). Its exit status and last output line are recorded.
9. **Backup.** Every `backup_mins`, with the mutex held: list changed paths (`git status --porcelain -z`, renames as both paths), keep those that are free, or owned only by slots that are idle, exited, resumable or empty, and not under a per-file lock; commit them as `vault backup: 2026-11-13 09:30` and push. Paths owned by a working or needs-you slot are left for later.
10. **`bd` writes go through the lock by rewriting the command.** The hook matches `bd <write subcommand>` (`create`, `update`, `close`, `reopen`, `dep`, `label`, `comment`, `comments add`, `delete`, `sync`, `edit`, `assign`) run anywhere, and returns `hookSpecificOutput.updatedInput` with the command `overseer ctl bd -- <original args>` (shell-quoted). `ctl bd` asks the daemon, which runs `bd_command` with those args in `beads_dir` under the vault mutex (when `beads_dir` is inside `brain_dir`; else under no lock), then queues a `bd sync` for the next drain. A command line with more than the `bd` call (pipes, `&&`) is denied with `run bd on its own so Overseer can run it under the 2ndBrain lock`. Cost if wrong: if a Claude Code release drops `updatedInput`, the hook must deny instead; the hand check (Task 10) catches it.
11. **Bash writes the hook catches** (best effort, vault and `CURRENT_SESSION` paths only): `>` and `>>` redirect targets, `tee [-a] <files>`, `sed -i`/`perl -i` file arguments, the last argument of `cp` and `mv`, every argument of `rm`, `touch`, `truncate`. Paths resolve against the hook's cwd and `~`. Anything else is allowed.
12. **Git in the vault.** Any `git` command whose effective repo (cwd, `-C <dir>`, `--git-dir`, `cd <dir> &&` prefix, as phase 4 resolves it) is the vault, with a subcommand other than `status`, `log`, `diff`, `show`, `blame`, `branch` (no args), `remote -v`, `rev-parse`, `ls-files`, is denied with `use the commit_2ndbrain tool; Overseer commits and pushes 2ndBrain`.
13. **Deny texts** (exact):
    - shared file: `<rel> is a shared 2ndBrain file. Use append_journal for the journal or edit_shared_file for an exact replacement.`
    - owned by another slot: `<rel> belongs to S<n> (<what>). Ask Jonathan before writing it.` where `<what>` is `session note` or `DEMO-12`.
    - `CURRENT_SESSION.md`: `CURRENT_SESSION.md is written by Overseer. Use update_current_session for your section.`
14. **`CURRENT_SESSION.md` layout.** At the top, a line `<!-- overseer:sections -->`, then one block per slot that has written, in slot order: `<!-- overseer:S3 -->`, `## S3 <label>`, the text, `<!-- /overseer:S3 -->`, then `<!-- /overseer:sections -->`. Everything that was in the file before the first write stays below, byte for byte. A slot's section stays after it stops until a new session starts in that slot, which clears it.
15. **Callers without a slot.** `ctl commit|journal|current-session` without `OVERSEER_SLOT` act as Jonathan: no ownership check, sender `You` in the ledger, and `current-session` needs `--slot`.
16. **`edit_shared_file` stale rule.** `old_text` must occur exactly once in the current file. Zero matches: `old_text was not found; the file has changed. Current text:` followed by the file (cut to the last 16 KB with a note if longer). More than one: `old_text occurs <n> times; include more context.` The file is unchanged in both cases.
17. **`append_journal(section, text)`.** Today's journal from `journal_path` (local date). Missing file: created with `# Journal - Friday, November 13, 2026` (weekday and month names from the date, computed by `libc::localtime_r`, never by hand). The text goes at the end of the `## <section>` block (before the next `## ` heading), preceded by a blank line. A missing section is appended at the end of the file as `## <section>`. Em dashes in `text` become `-`.

## Review Focus

1. **Two sessions commit while Jonathan or Obsidian has unrelated edits in the vault.** Each commit contains exactly its own paths, deletions included, and the other edits stay uncommitted and untouched. Test in Task 2.
2. **The remote rejects the push because another machine pushed.** The daemon rebases and pushes; on a conflict it aborts, keeps the local commit and the working tree, and tells Jonathan. Tests in Task 2.
3. **Jonathan edits the journal in Obsidian between a session reading it and calling `edit_shared_file`.** The edit fails with the current text and the file keeps Jonathan's change. Test in Task 4.
4. **A path written to the tools in an odd form**: `../` escaping the vault, an absolute path, a symlink out of the vault, spaces and non-ASCII in the name, a directory. Each is either normalised to a vault-relative file or refused with a reason; nothing outside the vault is committed. Tests in Task 3.
5. **The daemon restarts with queued commits.** The queue survives in the ledger, entries that were mid-commit are retried once, and nothing is committed twice. Test in Task 5.

Each line has its test in the owning task below.

---

### Task 1: Config, protocol and ledger v4

**Files:**
- Modify: `crates/overseer-daemon/src/config.rs`, `crates/overseer-proto/src/msg.rs`, `crates/overseer-proto/src/lib.rs`, `crates/overseer-daemon/src/ledger.rs`, placeholder arms in `crates/overseer-daemon/src/server.rs` and `crates/overseer-gui/src/app.rs`

**Interfaces:**
- `Config` gains `brain_writer: bool`, `shared_files: Vec<String>`, `journal_path: String`, `session_note: String`, `current_session: PathBuf` (tilde expanded), `backup_mins: u64`, `push: bool`, `sync_command: Vec<String>`, `bd_command: Vec<String>`, defaults per Ruling 2.
- Proto (all `Serialize, Deserialize, Debug, Clone, PartialEq`):
  - `enum QueueState { Queued, Committed, NothingToCommit, Failed }`
  - `struct QueueEntry { id: i64, at: i64, from: Sender, paths: Vec<String>, message: String, state: QueueState, commit: Option<String>, pushed: Option<bool>, error: Option<String> }`
  - `struct BrainView { on: bool, entries: Vec<QueueEntry>, last_backup: Option<i64>, last_sync: Option<(i64, bool)>, push_problem: Option<String> }` (`entries`: every queued entry plus the 20 most recent others, newest first)
  - `struct WriteCheck { slot: SlotId, tool: String, paths: Vec<PathBuf>, cwd: Option<PathBuf> }`
  - `Request::Commit { from: Option<SlotId>, paths: Vec<String>, message: String }`, `Request::AppendJournal { from: Option<SlotId>, section: String, text: String }`, `Request::EditShared { from: Option<SlotId>, path: String, old_text: String, new_text: String }`, `Request::UpdateCurrentSession { slot: SlotId, text: String }`, `Request::RunBd { from: Option<SlotId>, args: Vec<String> }`, `Request::BrainStatus`, `Request::CheckWrite(WriteCheck)`
  - `Response::Brain(BrainView)`, `Response::BdOutput { code: i32, stdout: String, stderr: String }`, `Response::Verdict` gains `#[serde(default)] rewrite: Option<String>` (a replacement Bash command)
  - `Event::BrainChanged(BrainView)`
- Ledger `VERSION = 4`, `V4_SCHEMA`: `brain_queue(id INTEGER PRIMARY KEY, at, sender TEXT json, paths TEXT json, message, state, commit_sha, pushed INTEGER NULL, error, attempts INTEGER DEFAULT 0)`, `brain_runs(kind TEXT PRIMARY KEY, at, ok, detail)` for `backup` and `sync`, and `ALTER TABLE sessions ADD COLUMN session_note TEXT`. Functions: `queue_push_on`, `queue_next_on`, `queue_set_on(id, state, sha, pushed, error)`, `queue_unpushed_on`, `queue_recent_on(n)`, `set_session_note(slot, rel)`, `session_note(slot) -> Option<String>`.

- [ ] **Step 1: Write the failing tests**: `config::tests::missing_file_is_defaults` asserts every new default from Ruling 2 (`brain_writer == false`, `backup_mins == 5`, three `shared_files`); `partial_file_overrides_only_its_keys` sets `brain_writer = true`; `msg` round-trip tests for `QueueEntry`, `BrainView`, `WriteCheck` and a `Verdict` without `rewrite` decoding to `rewrite: None`; `ledger::tests::version_3_file_migrates_to_4` (sessions and claims rows kept, `brain_queue` empty, `session_note` NULL) and `queue_is_fifo_and_survives_reopen`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon config ledger && cargo test -p overseer-proto`. Expected: FAIL to compile.
- [ ] **Step 3: Implement** the fields, types and v4 migration (v1, v2 and v3 files migrate in one transaction, as v3 did).
- [ ] **Step 4: Run** the same commands. Expected: PASS. Then fmt, clippy, full tests.
- [ ] **Step 5: Commit** `Add the 2ndBrain writer's config, protocol and queue table`.

### Task 2: The vault's git runner

**Files:**
- Create: `crates/overseer-daemon/src/brain/mod.rs` (module root, `pub mod git;` for now), `crates/overseer-daemon/src/brain/git.rs`
- Test: unit tests in `git.rs` with a helper `TestVault` (temp dir, `git init`, bare remote, `user.name`/`user.email` set locally, one initial commit pushed)

**Interfaces:**
- `pub struct VaultGit { root: PathBuf, lock: tokio::sync::Mutex<()>, push: bool }`
- `pub async fn lock(&self) -> VaultGuard<'_>`; every method below takes `&VaultGuard`, so callers hold the mutex across commit plus push.
- `pub async fn commit_paths(&self, g: &VaultGuard<'_>, paths: &[String], message: &str) -> Result<Option<String>, GitError>` (`None` when nothing changed)
- `pub async fn push(&self, g: &VaultGuard<'_>) -> Result<(), GitError>` (Ruling 6)
- `pub async fn changed_paths(&self, g: &VaultGuard<'_>) -> Result<Vec<String>, GitError>`
- `pub async fn run(&self, g: &VaultGuard<'_>, args: &[&str]) -> Result<String, GitError>`, every git call with `OVERSEER_COMMIT=1`, `GIT_TERMINAL_PROMPT=0`, cwd the vault.
- `pub enum GitError { IndexLocked, Conflict(String), Failed(String) }`

- [ ] **Step 1: Write the failing tests:**
  - `commit_takes_only_its_paths`: write `a.md`, `b.md`, delete the tracked `old.md`, leave `jonathan.md` modified; `commit_paths(["a.md", "old.md"], "S1: notes")`; `git show --name-status HEAD` is exactly `M a.md` (or `A`) and `D old.md`; `b.md` and `jonathan.md` are still dirty. (Review Focus 1)
  - `two_concurrent_commits_do_not_mix`: two tasks commit `s1.md` and `s2.md` at once through `lock()`; two commits, each with one file.
  - `unchanged_paths_are_nothing_to_commit`: returns `Ok(None)`.
  - `rejected_push_rebases_and_pushes`: a second clone pushes `other.md` first; `push` succeeds and the remote has both commits. (Review Focus 2)
  - `conflicting_push_keeps_the_local_commit`: the other clone changes the same line; `push` returns `Err(Conflict(_))`, `git status` shows no rebase in progress, the local commit is still `HEAD`, and an unrelated dirty file is unchanged. (Review Focus 2)
  - `fresh_index_lock_fails_then_stale_is_removed`: a 1 s old `.git/index.lock` gives `Err(IndexLocked)` after about 5 s; one with mtime 2 minutes ago is removed and the commit succeeds.
  - `post_commit_hook_sees_overseer_commit`: a test `post-commit` hook writes `$OVERSEER_COMMIT` to a file; it reads `1`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon brain::git`. Expected: FAIL.
- [ ] **Step 3: Implement** with `tokio::process::Command`; commit is `git add -A -- <paths>` then `git commit -m <msg> -- <paths>`; "nothing to commit" is detected by `git diff --cached --quiet -- <paths>` after the add.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `Run 2ndBrain git under one lock with pathspec commits`.

### Task 3: Vault paths and ownership

**Files:**
- Create: `crates/overseer-daemon/src/brain/paths.rs`
- Modify: `crates/overseer-daemon/src/brain/mod.rs`

**Interfaces:**
- `pub enum PathClass { Outside, CurrentSession, Shared, Owned(Vec<(SlotId, String)>), Free }` (the `String` is the deny's `<what>`)
- `pub struct Owners { pub notes: Vec<(SlotId, String)>, pub folders: Vec<(SlotId, String, String)> }` (slot, vault-relative folder, item key), built by `pub fn owners(work: &WorkView, notes: &[(SlotId, String)], live: &[SlotId], brain: &Path) -> Owners` per Ruling 3.
- `pub fn vault_rel(brain: &Path, cwd: Option<&Path>, p: &Path, home: &Path) -> Result<String, String>`: `~` and cwd resolved, `.`/`..` folded, symlinks resolved for the existing prefix; `Err("<p> is outside 2ndBrain")` or `Err("<p> is a folder")`.
- `pub fn classify(rel_or_abs: &Path, ...) -> PathClass` and `pub fn shared_match(globs: &[String], rel: &str) -> bool`
- `pub fn fill_template(t: &str, now: i64, slot: SlotId, label: &str) -> String` for `journal_path` and `session_note`.

- [ ] **Step 1: Write the failing tests:** `shared_globs_match_one_segment` (`06-Daily/2026/11-November/2026-11-13-journal.md` matches, `06-Daily/2026/x.md` does not, `Sprints/sprint-26-01/SPRINT-DASHBOARD.md` matches, `Sprints/sprint-26-01/DEMO-12-demo-lane/notes.md` does not); `ticket_folder_is_owned_by_owner_and_bead_owner`; `two_owners_are_both_listed`; `exited_slots_own_nothing`; `odd_paths` (Review Focus 4: `../outside.md` refused, absolute in-vault path normalised, a symlink pointing out refused, `Sessions/notes/demo api é.md` kept as is, a folder refused); `current_session_is_its_own_class`; `templates_fill` (`{yyyy}-{mm}-{dd}`, `{Month}` = `November`, `{slot}` = `s3`, label `Demo API!` slugs to `demo-api`, empty label to `session`).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon brain::paths`. Expected: FAIL.
- [ ] **Step 3: Implement.** Dates come from `libc::localtime_r` as `local_hhmm` does; month and weekday names from fixed English tables indexed by `tm_mon`/`tm_wday`.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `Classify 2ndBrain paths by owner`.

### Task 4: Shared files and CURRENT_SESSION sections

**Files:**
- Create: `crates/overseer-daemon/src/brain/shared.rs`, `crates/overseer-daemon/src/brain/current.rs`

**Interfaces:**
- `pub struct FileLocks` (a `Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>`) with `pub async fn lock(&self, p: &Path) -> OwnedMutexGuard<()>` and `pub fn is_locked(&self, p: &Path) -> bool`.
- `pub fn append_section(file: &str, section: &str, text: &str) -> String` (pure, Ruling 17)
- `pub fn journal_header(now: i64) -> String`
- `pub fn replace_once(file: &str, old: &str, new: &str) -> Result<String, String>` (pure, Ruling 16 messages)
- `pub fn set_section(file: &str, slot: SlotId, label: &str, text: &str) -> String` and `pub fn clear_section(file: &str, slot: SlotId) -> String` (pure, Ruling 14)
- Each file write is atomic: write `<name>.overseer-tmp` beside it, then rename.

- [ ] **Step 1: Write the failing tests:** `append_goes_at_the_end_of_its_section` (between `## Work Log` and `## Context / Notes`), `missing_section_is_added_at_the_end`, `new_journal_gets_its_header` (`# Journal - Friday, November 13, 2026` for a fixed timestamp), `em_dashes_become_hyphens`; `replace_once_rules` (one match replaced, zero gives the "not found" text containing the current file, two gives `occurs 2 times`); `stale_edit_keeps_jonathans_change` (Review Focus 3: write the file, change it as Jonathan would, then call the service-level edit with the old text; the error carries the new text and the file still holds it); `sections_sit_above_the_history` (a 2,500-line history file keeps its bytes below the markers after S3 and S1 write; S1's block comes first); `second_write_replaces_only_that_section`; `clear_section_removes_the_block`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon brain::shared brain::current`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `Edit shared 2ndBrain files and session sections safely`.

### Task 5: The brain service, queue, backup, sync and `bd`

**Files:**
- Modify: `crates/overseer-daemon/src/brain/mod.rs`, `crates/overseer-daemon/src/server.rs`, `crates/overseer-daemon/src/manager.rs` (session note fixed at slot start and cleared section on a fresh start), `crates/overseer-daemon/src/lib.rs`
- Test: `crates/overseer-daemon/tests/brain.rs` (temp vault, bare remote, stub `bd` and stub sync script that log their args and env)

**Interfaces:**
- `pub struct BrainService` built by `BrainService::new(cfg: &Config, ledger: Arc<Ledger>, events: broadcast::Sender<Event>) -> Option<Arc<BrainService>>` (`None` when `brain_writer` is off).
- `pub async fn commit(&self, from: Sender, paths: Vec<String>, message: String, owners: &Owners) -> Response` (ownership check for slot senders: refuse any path owned by another slot only, or shared, or outside; then queue and wait for the result)
- `pub async fn append_journal(&self, from: Sender, section: &str, text: &str) -> Response`, `pub async fn edit_shared(&self, from: Sender, rel: &str, old: &str, new: &str) -> Response`, `pub async fn update_current_session(&self, slot: SlotId, label: &str, text: &str) -> Response`, `pub async fn run_bd(&self, args: Vec<String>) -> Response`
- `pub fn check_write(&self, check: &WriteCheck, owners: &Owners) -> Option<String>` (the deny text, Ruling 13; `None` allows)
- `pub fn view(&self) -> BrainView`; a background task runs the queue, the 10 s drain then sync (Ruling 8), the backup timer (Ruling 9) and pending `bd sync` runs, and sends `Event::BrainChanged` after each change.
- On start, entries left `Queued` are retried; every daemon commit message ends with a trailer `Overseer-Queue: <id>`, and an entry whose trailer is already in `git log` is marked `Committed` without a second commit.

- [ ] **Step 1: Write the failing tests** in `tests/brain.rs`:
  - `session_commit_is_queued_committed_and_pushed`: S1 commits its session note; one commit, trailer present, remote has it, view shows `Committed`, `pushed: Some(true)`.
  - `commit_refuses_another_slots_folder_and_shared_files` with the Ruling 13 texts.
  - `journal_append_commits_the_journal` with message `S2: journal, Work Log`.
  - `sync_runs_once_after_the_queue_drains`: three quick commits, one sync run, with `OVERSEER_COMMIT=1`; not run while a working slot has an uncommitted owned path.
  - `backup_takes_free_and_idle_paths_only`: free file and an idle slot's note committed as `vault backup: ...`; a working slot's file and a file under a held `FileLocks` guard left.
  - `queue_survives_restart_without_double_commits` (Review Focus 5): kill the service after the commit but before the state update; the reopened service marks it `Committed` by trailer and commits nothing more.
  - `bd_runs_under_the_lock_then_syncs`: `run_bd(["update", "ov-3", "--status", "in_progress"])` runs the stub in `beads_dir`, and a `bd sync` follows the drain.
  - `writer_off_answers_with_the_config_hint`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon --test brain`. Expected: FAIL.
- [ ] **Step 3: Implement** and wire the requests in `server.rs` (the `Owners` come from the work store and the ledger's session notes on each call). The session note is fixed by `fill_template(session_note, start_time, slot, label)` on a fresh start and kept on resume.
- [ ] **Step 4: Run** the tests, then the full suite 3 times (the queue is timing sensitive). Expected: PASS every time.
- [ ] **Step 5: Commit** `Serialise 2ndBrain commits, backups, sync and bd in the daemon`.

### Task 6: The hook's write checks

**Files:**
- Modify: `crates/overseer/src/hook.rs`, `crates/overseer-daemon/src/launch.rs` (`hook_shape("PreToolUse")` matcher `Bash|Edit|Write|MultiEdit|NotebookEdit`), `crates/overseer-daemon/src/locks/mod.rs` (`check_bash` also asks the brain service), `crates/overseer-daemon/src/brain/mod.rs`
- Create: `crates/overseer-daemon/src/brain/bash.rs` (pure command parsing for Rulings 10 to 12)

**Interfaces:**
- `pub fn write_paths(command: &str) -> Vec<String>` (Ruling 11), `pub fn vault_git(command: &str, cwd: &Path, brain: &Path) -> Option<String>` (the denied subcommand, Ruling 12), `pub fn bd_call(command: &str) -> BdCall` with `enum BdCall { None, Read, Write(Vec<String>), Compound }`.
- `hook::parse_pre_tool_use` returns `Request::CheckWrite` for the file tools (the path is `tool_input.file_path`, or `notebook_path` for `NotebookEdit`) and `Request::CheckBash` as now.
- A `Verdict { rewrite: Some(cmd) }` prints `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":cmd}}}`; a deny prints the phase 4 shape.

- [ ] **Step 1: Write the failing tests:** in `bash.rs`, `redirects_and_writers_are_found` (`echo x >> 06-Daily/.../j.md`, `tee -a a.md b.md`, `sed -i '' s/a/b/ f.md`, `cp x.md Sessions/y.md`, `rm -f a.md`), `quoted_text_is_not_a_write` (`echo "a > b"`, `git commit -m "x >> y"`), `vault_git_rules` (`git -C ~/Repos/2ndBrain push` denied, `git log` and `git status` allowed, a repo outside the vault ignored), `bd_rules` (`bd ready --json` is `Read`, `bd update ov-3 --status done` is `Write`, `bd close ov-3 && git push` is `Compound`); in `hook.rs`, `edit_of_a_file_tool_becomes_check_write`, `rewrite_prints_updated_input`; in `crates/overseer-daemon/tests/server.rs`, `shared_file_write_is_denied_with_the_tool_name` and `writer_off_allows_everything`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon brain::bash && cargo test -p overseer hook && cargo test -p overseer-daemon --test server shared_file`. Expected: FAIL.
- [ ] **Step 3: Implement.** The daemon answers within the phase 4 budget; the hook stays fail-open.
- [ ] **Step 4: Run** the tests. Expected: PASS. Then fmt, clippy, full tests.
- [ ] **Step 5: Commit** `Stop sessions writing around the 2ndBrain writer`.

### Task 7: MCP tools and `ctl` commands

**Files:**
- Modify: `crates/overseer/src/mcp.rs`, `crates/overseer/src/ctl.rs`, `crates/overseer-daemon/src/launch.rs` (`permissions.allow` gains the four tools)
- Test: `crates/overseer/tests/cli.rs`, unit tests in `mcp.rs`

**Interfaces:**
- MCP tools: `commit_2ndbrain {paths: string[], message: string}`, `append_journal {section: string, text: string}`, `edit_shared_file {path: string, old_text: string, new_text: string}`, `update_current_session {text: string}`. Errors from the daemon are tool errors (`isError: true`), as in phase 4.
- `ctl commit -m <message> <paths>...`, `ctl journal <section> <text>` (text `-` reads stdin), `ctl current-session [--slot S<n>] <text|->`, `ctl bd -- <args>...` (prints the daemon's stdout and stderr and exits with bd's code), `ctl brain [--json]` (queue, last backup, last sync, push problem). Sender is `$OVERSEER_SLOT` when set (Ruling 15).

- [ ] **Step 1: Write the failing tests:** `tools_list_names_the_eight_tools`; argument errors for each new tool (missing `paths`, non-string `text`); `ctl_bd_passes_args_and_exit_code` against a temp daemon with a stub `bd` that exits 3; `ctl_commit_without_slot_is_jonathan`; `settings_allow_the_overseer_tools` updated for eight tools.
- [ ] **Step 2: Run** `cargo test -p overseer`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `Give sessions and scripts the 2ndBrain tools`.

### Task 8: Roles

**Files:**
- Modify: `crates/overseer-daemon/src/roles/worker.md`, `roles/coordinator.md`, `crates/overseer-daemon/src/launch.rs` (placeholders `{session_note}`, `{current_session}`; previous shipped hashes join `PREVIOUS_WORKER` and `PREVIOUS_COORDINATOR`)

**Interfaces:**
- Worker role adds, only when the writer is on (a `{brain_rules}` block, empty when off): write your own session note at `{session_note}` and your ticket folders directly; commit with `commit_2ndbrain`, never `git commit`; the journal, sprint dashboard and sprint hub only through `append_journal` or `edit_shared_file`; read and write only your own section of `{current_session}`, through `update_current_session`; run `bd` writes as single commands.
- Coordinator role adds the same shared-file and journal rules.

- [ ] **Step 1: Write the failing tests:** `worker_role_names_the_brain_tools` (all four tool names and the session note path appear when on, none when off), `phase_4_roles_upgrade` (the hashes of the roles shipped in 4a18dd8 are upgraded, an edited file is kept).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon launch`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `Tell sessions how to write 2ndBrain under Overseer`.

### Task 9: Board strip and session panel

**Files:**
- Modify: `crates/overseer-gui/src/board.rs` (bottom strip), `crates/overseer-gui/src/session.rs` (`n` opens the slot's session note), `crates/overseer-gui/src/app.rs` (holds `BrainView`, handles `Event::BrainChanged`), `crates/overseer-gui/src/notify.rs` (a notification for a push problem)
- `SlotInfo` gains `#[serde(default)] session_note: Option<String>` (vault-relative) in `crates/overseer-proto/src/msg.rs`

**Interfaces:**
- Strip text, right side: `2ndBrain: 2 queued` / `2ndBrain: committed 09:41` / `2ndBrain: push needs you` (orange) / `2ndBrain: writer off` (grey). Hover or click shows the last 5 entries (`09:41 S1  Sessions/.../s1-demo.md  committed, pushed`).
- `n` in the session view runs `open "obsidian://open?path=<abs path, url-encoded>"`; with no note yet, the key does nothing and the panel says `No session note yet`.

- [ ] **Step 1: Write the failing tests:** pure functions `strip_text(&BrainView) -> (String, Tone)` and `entry_line(&QueueEntry) -> String` in `board.rs` with one test per state above; `obsidian_url_encodes_spaces_and_unicode` in `session.rs`.
- [ ] **Step 2: Run** `cargo test -p overseer-gui`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `Show the 2ndBrain queue on the board`.

### Task 10: Setup checklist and acceptance list

**Files:**
- Create: `docs/setup/work-laptop-2ndbrain.md`, `docs/acceptance/phase-5.md`

**Interfaces:**
- The checklist, for Jonathan to apply by hand (nothing here is done by code):
  1. Obsidian Git: auto commit and auto push off (`autoSaveInterval: 0`, `autoPushInterval: 0`); auto pull off, since the daemon pulls on push.
  2. The vault's `.git/hooks/post-commit`: first line after the shebang `[ "$OVERSEER_COMMIT" = 1 ] && exit 0`.
  3. `sync-run.sh` is left as it is: the daemon starts it with `OVERSEER_COMMIT=1` set, so its own commit's `post-commit` does not start a second sync.
  4. The global hooks and skills listed in spec 7.8.1, each with the snippet that routes to `overseer ctl commit|journal|current-session` when `OVERSEER_SLOT` is set, and stays as now otherwise. Each one is changed in its own place with Jonathan's approval.
  5. `brain_writer = true` in `config.toml`, then restart the daemon.
- The acceptance list in the phase 4 format (ID, check, steps, expected, result), run on the home Mac against a copy of a test vault at `/tmp/ov-hand5/vault` with a bare remote, never the real vault. Rows: concurrent commits (Review Focus 1), rejected and conflicting push (2), stale edit after an Obsidian-style edit (3), odd paths (4), restart with a queued commit (5), each deny text from Ruling 13 with a real Claude session, the `bd` rewrite through `updatedInput` on Claude Code 2.1.259, the backup leaving a working slot's file alone, the sync running once, `CURRENT_SESSION` sections with two sessions, the board strip and `n`.

- [ ] **Step 1: Write both documents.** Check them for em dashes (`grep -c $'\u2014'` gives 0) and real names.
- [ ] **Step 2: Commit** `Add the phase 5 setup checklist and acceptance list`.
