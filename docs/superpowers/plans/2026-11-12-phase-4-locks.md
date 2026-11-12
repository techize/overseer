# Phase 4: Locks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Overseer stops two sessions writing to the same shared target. Sessions claim and release targets through MCP tools, a synchronous `PreToolUse` hook on Bash denies colliding write commands and auto-claims free ones, blocked callers wait in a FIFO queue, and the daemon hands a released target to the next waiter with a relay typed into that session under the 7.6 delivery rules. Every lock, wait and relay is in the ledger and on the board.

**Architecture:** A new `locks` module in the daemon owns targets: `targets.rs` turns a Bash command into target strings from patterns (compiled-in defaults plus `targets.toml`), `table.rs` holds the claim, wait and relay rules as functions on a ledger `Connection` (schema v3), and `LockService` ties them to requests, hook events and slot exits. A `relay` module holds the courier task that types queued relays into idle sessions, using a daemon-side replay of the slot's ring into an `overseer-term` grid to check the input box. `overseer hook PreToolUse` asks the daemon and prints a deny decision; `overseer mcp` is a hand-rolled stdio MCP server wired per slot through a generated `mcp.json`. The protocol gains lock and relay requests and a `LocksChanged` event; the GUI adds lock pills, waits, pending counts, a relay strip and a Holds panel.

**Tech Stack:** Rust, tokio, rusqlite (bundled), `toml` 0.8, `regex` 1, `serde_json`, `alacritty_terminal` through `overseer-term`, GPUI 0.2.2.

**Spec:** `docs/superpowers/specs/2026-11-09-overseer-design.md`, sections 7.5 and 7.6 (the core), 7.1 (`overseer mcp`, `ctl locks`, `ctl send`), 7.2 and 7.3 (locks kept over restarts, "blocked by lock"), 7.9 (lock pills, pending relays, bottom strip, Holds, `m` and `r`, notification for a denied write), 7.10 (ledger tables, `config.toml`, `targets.toml`), 8 (testing) and 9 item 4.

## Global Constraints

- The repo is public. No real company names, cluster names, ArgoCD app names, ticket keys, account ids or hostnames in defaults, tests, fixtures or docs. Invented examples only (`DEMO-12`, `ov-3`, `demo-api`, `uat/root`, `staging-eu`).
- Never touch `~/.claude/settings.json`, `~/.claude.json` or global hooks (spec 4). Hooks, MCP server and tool permissions are added per slot only, through `slots/S<n>/settings.json` and `slots/S<n>/mcp.json`.
- Read-only work is never blocked or slowed (spec 4). A command no pattern matches costs one socket round trip and nothing else. Read-only commands (`git log`, `kubectl get`, `argocd app get`, `terraform plan`, any `--dry-run`) are never matched.
- A broken daemon never stops work. `overseer hook PreToolUse` fails open: daemon missing, slow (over 1 s) or answering junk means allow, no output, exit 0.
- Never type into a session that shows a permission or question prompt (spec 7.6). When in doubt, a relay waits; it never guesses.
- Nothing acts silently (spec 4): every claim, deny, wait, handoff and relay is stored in the ledger and shown on the board.
- Locks, waits and pending relays survive a daemon restart (spec 7.2).
- Tests never touch the installed app, the LaunchAgent `uk.co.techize.overseer`, its socket, `~/Library/Application Support/overseer`, or the real `~/Repos/2ndBrain` beads. Temp daemons live under `/tmp` with `OVERSEER_HOME` set; `claude`, `bd`, `git`, `kubectl` and `argocd` are stubbed where a test needs them.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass at the end of every task.
- No em dashes in any human-readable text the app writes or the plan's code adds. No emojis. Commit messages human style, no AI language, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- The protocol stays backward compatible: every new field on an existing struct or request uses `#[serde(default)]`.

## Rulings (the controller's decisions, plus the gaps this plan fills)

Decisions already made:

1. `claim_target`, `release_target`, `notify_session` and a read-only `list_targets` are MCP tools served by `overseer mcp` (JSON-RPC 2.0 over stdio, hand-rolled: `initialize`, `notifications/initialized`, `ping`, `tools/list`, `tools/call`). It reads `OVERSEER_SLOT` and talks to the daemon over the existing socket. It is wired per slot only through `slots/S<n>/mcp.json` passed with `--mcp-config`.
2. The `PreToolUse` hook (matcher `Bash`) is synchronous with a 1 s budget (hook `timeout` 5 in settings). It prints the Claude Code decision JSON only on deny, nothing on allow, and always exits 0.
3. `targets.toml` resolves like `config.toml` (`$OVERSEER_TARGETS`, else `~/.config/overseer/targets.toml`). Defaults are compiled in and generic (git push, argocd set/sync and pins, trigger.dev deploy, terraform/tofu apply, kubectl apply/delete/patch).
4. Auto-claims are released on that command's `PostToolUse` (by `tool_use_id`, else every command claim of that slot). Releasing a work item releases the targets claimed for it. Long-held locks are flagged after `lock_idle_mins` (config.toml), never dropped.
5. A denied command or a held `claim_target` queues the caller FIFO on that target. A release grants the target to the first waiter and queues the relay `S1 has released <target>. You now hold it.`
6. Delivery per 7.6, with the input-box rule in Task 6.
7. Ledger schema v3 adds `claims`, `waits` and `relays`, migrating from v1 and v2.
8. `ctl` gains `locks`, `claim`, `release <slot> <target>`, `force-release`, `send`. The GUI gains pills, waits, pending counts, the relay strip, Holds with release, and claim and force-release.
9. Worktree creation (7.5 "Worktrees") is out of scope for this phase. So are kickoffs that pre-assign targets (7.7, phase 6), the Locks view (7.9, phase 7) and the `Edit|Write` and 2ndBrain blocks of the hook (7.8, phase 5). The hook code keeps the tool check in one place so phase 5 can add matchers.
10. The role templates tell sessions about the tools, and the phase 3 shipped role hashes join the `PREVIOUS_*` tables.

Filled in by this plan:

- **Pin rule.** `argocd app set <app> --revision <rev>` takes a pin claim recording `<rev>`. The pin ends when its holder runs `argocd app set <app> --revision <r>` with `<r>` in the pattern's `unpin` list (default `main`, `master`, `HEAD`), released on that command's `PostToolUse`, or on an explicit release. The revision in force before the pin cannot be learned from the command line inside the hook budget, so "set back to the recorded previous target" becomes "set back to a default-branch revision". Cost if wrong: a pin to restore a non-default revision must be released by hand.
- **Command claims are also released on the slot's `Stop` and on slot exit.** A failed or interrupted command may send no `PostToolUse`, and a turn's end means the command is over. Cost if wrong: a `run_in_background` push releases when the tool returns, not when the push finishes.
- **Grants.** A waiter queued by a denied command gets a `Grant` claim. It becomes a command claim when that slot's next matching command passes the hook, and is released at the first `Stop` after its grant relay was delivered if unused. A waiter queued by `claim_target` gets an ordinary `Claim`, held until released.
- **Which item a claim is for.** The `item` argument of `claim_target`, else the slot's only owned ticket or standalone bead when it owns exactly one, else none.
- **Resolution runs in the daemon**, which owns the patterns. The hook sends the command, cwd and an allowlist of env vars (`KUBECONFIG`, `TF_WORKSPACE`, `ARGOCD_SERVER`, `ARGOCD_CONFIG`, `HOME`). The daemon has 700 ms; past that it answers allow and records nothing, so a late deny never leaves a stray wait.
- **Repo name** is the folder name of `git rev-parse --git-common-dir`'s parent, so pushes from two worktrees of one repo share `repo:<name>:<branch>`.
- **Input box not found means wait.** The relay shows as pending with the reason. Cost if wrong: a new Claude Code layout stalls relays visibly until the reader is fixed.
- **Several pending relays for one slot go in one paste**, separated by a blank line, and are all marked delivered.
- **Phase 3 notices are now delivered.** The notices that tell a session something (move, overlap warning, promotion) also queue a relay `Overseer: <notice text>` in the same transaction. Label failures stay notices for Jonathan only.
- **`ctl send` becomes a relay** under the delivery rules, sent as `$OVERSEER_SLOT` when set (so S0's sends obey the same rules as `notify_session`) and as Jonathan otherwise. `--raw` keeps the old immediate typing.
- **`ctl release` keeps its phase 3 meaning** with one argument (`release <key>` frees a work item); two arguments (`release <slot> <target>`) release a target.
- **Per-slot `settings.json` allows the Overseer MCP tools** (`permissions.allow`), so sessions are not prompted to use them. It is merged by Claude on top of Jonathan's settings, not written into them.
- **Workers reply once.** A worker may `notify_session` slot X only to answer a delivered relay from X that it has not answered yet. Since only S0 and Jonathan can start, workers can answer S0 and nothing else; a worker cannot start a chain.
- **`tf:<workspace>` follows the spec**, which means every stack in workspace `default` shares one lock. A `{dir}` placeholder is available for `targets.toml` to split them.
- **Long-held** means `now - last_used > lock_idle_mins` (default 60), where `last_used` is the claim time or the last command by the holder that matched the target.

## Review Focus

1. **The daemon is down, hung or slow while Claude runs a Bash command.** The hook allows within about 1 s, prints nothing and exits 0; the command runs. Test in Task 4.
2. **A relay is due while Claude shows a permission or question prompt, or while Jonathan has text in the input box.** Nothing is typed; the relay stays pending with a reason and goes once the prompt is answered and the box is empty. Tests in Tasks 6 and 7.
3. **Write-shaped text that is not a write command**: `git commit -m "then git push"`, a heredoc body containing `kubectl apply`, `echo 'argocd app sync x'`, `git push --dry-run`, `kubectl apply --dry-run=client`. None is matched. And real writes in compound forms (`cd ../demo-api && git push`, `git -C ../demo-api push origin HEAD:main`, `FOO=1 terraform apply`) are. Tests in Task 3.
4. **A daemon restart with held locks, waiters and pending relays.** The same claims, queue order and pending relays are back before any session reconnects, and the pending relays deliver once the slot is resumed and idle. Tests in Tasks 2 and 7.
5. **The holder exits, or Jonathan stops it, while another slot waits.** Its command and grant claims are released and handed on at once; its explicit claims and pins stay (flagged in time) until Jonathan force-releases; an exited waiter leaves the queue. Test in Task 4.

Each line has its test in the owning task below.

---

### Task 1: Lock and relay protocol

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs`, `src/lib.rs` (re-exports), `crates/overseer-daemon/src/server.rs` and `crates/overseer-gui/src/app.rs` (placeholder arms only)

**Interfaces (all `Serialize, Deserialize, Debug, Clone, PartialEq`):**
- `enum ClaimKind { Command, Grant, Claim, Pin }`
- `struct Claim { target: String, slot: SlotId, kind: ClaimKind, reason: Option<String>, item: Option<String>, since: i64, last_used: i64, pin_revision: Option<String> }`
- `struct Wait { target: String, slot: SlotId, at: i64, reason: Option<String> }` (a target's waits are listed in queue order)
- `#[serde(tag = "type")] enum Sender { Overseer, You, Slot { slot: SlotId } }` (`You` is Jonathan through the GUI or `ctl`)
- `enum RelayState { Pending, Delivered }`
- `struct Relay { id: i64, at: i64, from: Sender, to: SlotId, text: String, state: RelayState, delivered_at: Option<i64>, held: Option<String> }`. `held` is why a pending relay is waiting (`session is working`), filled by the courier.
- `struct LockView { claims: Vec<Claim>, waits: Vec<Wait>, relays: Vec<Relay>, flag_after_secs: i64 }`, with `fn flagged(&self, c: &Claim, now: i64) -> bool`, `fn waiting_on(&self, slot: SlotId) -> Vec<&str>`, `fn pending_for(&self, slot: SlotId) -> usize`. `relays` holds every pending relay plus the 50 most recent delivered ones, newest first.
- `struct BashCheck { slot: SlotId, session_id: Option<String>, tool_use_id: Option<String>, command: String, cwd: Option<PathBuf>, env: Vec<(String, String)> }`
- `Request::ListLocks`, `Request::ClaimTarget { slot: SlotId, target: String, #[serde(default)] reason: Option<String>, #[serde(default)] item: Option<String> }`, `Request::ReleaseTarget { slot: SlotId, target: String }`, `Request::ForceRelease { target: String }`, `Request::SendRelay { #[serde(default)] from: Option<SlotId>, to: SlotId, text: String }`, `Request::CheckBash(BashCheck)`
- `Response::Locks(LockView)`, `Response::Verdict { deny: Option<String> }`, `Response::Queued { message: String }`
- `Event::LocksChanged(LockView)`
- `HookEvent` gains `#[serde(default)] tool_use_id: Option<String>` and `#[serde(default)] notification_type: Option<String>`. `Activity` gains `#[serde(default)] idle_wait: bool` (true for a `Notification` that is an idle-input wait, see Task 7).

- [ ] **Step 1: Tests** round-trip every new request, response and event. `claim_target_defaults` parses `{"type":"ClaimTarget","slot":2,"target":"repo:demo-api:main"}` with `reason` and `item` `None`. `old_hook_event_parses` parses a phase 3 `HookEvent` JSON without the new fields. `sender_shapes` checks `Sender::Slot { slot: 3 }` is `{"type":"Slot","slot":3}`. `lock_view_helpers`: `flagged` true only past `flag_after_secs` since `last_used`, `waiting_on(2)` lists the targets S2 waits on, `pending_for` counts pending relays to that slot only.
- [ ] **Step 2: Run** `cargo test -p overseer-proto` and see them fail.
- [ ] **Step 3: Implement.** The daemon answers the new requests with `Error { message: "not supported yet" }`; the GUI ignores `LocksChanged`; every `HookEvent` and `Activity` literal in the workspace gains the new fields.
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Add locks and relays to the protocol".

### Task 2: Ledger schema 3 and the lock table

**Files:**
- Create: `crates/overseer-daemon/src/locks/mod.rs` (module root only in this task), `locks/table.rs`
- Modify: `crates/overseer-daemon/src/ledger.rs`, `src/lib.rs`

**Interfaces:**
- Migration: `VERSION` 3. `V3_SCHEMA`, applied like `V2_SCHEMA` (so a v1 or v2 file reaches v3 in one transaction and a fresh file is created at v3):

```sql
CREATE TABLE IF NOT EXISTS claims (target TEXT PRIMARY KEY, slot INTEGER NOT NULL, kind TEXT NOT NULL,
  reason TEXT, item TEXT, tool_use_id TEXT, pin_revision TEXT, relay_id INTEGER,
  since INTEGER NOT NULL, last_used INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS waits (id INTEGER PRIMARY KEY, target TEXT NOT NULL, slot INTEGER NOT NULL,
  via TEXT NOT NULL, reason TEXT, at INTEGER NOT NULL, UNIQUE(target, slot));
CREATE TABLE IF NOT EXISTS relays (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, sender TEXT NOT NULL,
  from_slot INTEGER, to_slot INTEGER NOT NULL, text TEXT NOT NULL, reply_to INTEGER, delivered_at INTEGER);
```

- In `table.rs`, all `pub(crate)` and taking `conn: &Connection` (the `*_on` style), called inside `Ledger::transaction`:
  - `claims_on(conn) -> Result<Vec<Claim>>`, `waits_on(conn) -> Result<Vec<Wait>>` (by `id`), `relays_on(conn, recent: usize) -> Result<Vec<Relay>>` (all pending plus `recent` delivered, newest first).
  - `enum Via { Hook, Tool }`; `struct NewClaim<'a> { target: &'a str, slot: SlotId, kind: ClaimKind, reason: Option<&'a str>, item: Option<&'a str>, tool_use_id: Option<&'a str>, pin_revision: Option<&'a str>, via: Via }`
  - `enum ClaimOutcome { Granted, AlreadyHeld, Queued { holder: Claim, position: usize } }`
  - `claim_on(conn, c: &NewClaim, now) -> Result<ClaimOutcome>`: free target, insert. Held by the same slot: `AlreadyHeld`, `last_used = now`. Held by another: add a wait (`INSERT OR IGNORE`, so a second deny does not move the slot back in line) and return its 1-based position.
  - `enum Releaser { Slot(SlotId), Force }`; `struct Handoff { target: String, from: SlotId, to: Option<SlotId> }`
  - `release_on(conn, target, by: Releaser, back_on: Option<&str>, now) -> Result<Option<Handoff>>`: a slot that does not hold the target but waits on it leaves the queue (`Ok(None)`); neither holds nor waits is `Err` with the text `<target> is not held by S2`. On release the first waiter becomes the holder (`Grant` if it queued through the hook, `Claim` through the tool, `relay_id` set) and a relay is queued to it, in the same transaction. Relay texts, exact:
    - `S1 has released argo:uat/root. You now hold it.`, with ` (back on main)` before the full stop when `back_on` is `Some("main")`
    - forced: `Jonathan released argo:uat/root. You now hold it.` to the waiter, and `Jonathan released argo:uat/root, which you held.` to the old holder
  - `touch_on(conn, target, slot, now)`, `bind_grant_on(conn, target, slot, tool_use_id)` (a `Grant` or the holder's own `Pin` being unpinned becomes `Command` with that id).
  - `release_commands_on(conn, slot, tool_use_id: Option<&str>, now) -> Result<Vec<Handoff>>`: with an id, the slot's `Command` claims with that id; without, all its `Command` claims.
  - `release_on_stop_on(conn, slot, now) -> Result<Vec<Handoff>>`: the slot's `Command` claims, and its `Grant` claims whose relay is delivered.
  - `drop_slot_on(conn, slot, now) -> Result<Vec<Handoff>>`: what `release_on_stop_on` releases plus every `Grant`, and all of the slot's waits.
  - `release_item_on(conn, item, now) -> Result<Vec<Handoff>>`: every claim with that `item`.
  - `queue_relay_on(conn, from: &Sender, to: SlotId, text: &str, reply_to: Option<i64>, now) -> Result<i64>`, `pending_relays_on(conn, to) -> Result<Vec<Relay>>`, `mark_delivered_on(conn, ids: &[i64], now)`, `unanswered_from_on(conn, worker: SlotId, from: SlotId) -> Result<Option<i64>>` (the oldest delivered relay from slot `from` to `worker` with no relay from `worker` naming it in `reply_to`).
- `Ledger` gains `lock_view(&self, recent: usize, flag_after_secs: i64) -> Result<LockView>`.

- [ ] **Step 1: Tests** in `table.rs` and `ledger.rs`: `version_2_file_migrates_to_3_keeping_assignments` (v2 file with an assignment and a notice), `version_1_file_migrates_to_3`, `claim_free_target`, `claim_own_target_is_already_held`, `claim_held_target_queues_fifo` (S2 then S3 queue, positions 1 and 2, S2 queuing again stays 1), `release_hands_to_first_waiter_with_relay` (relay text exact), `release_with_back_on`, `force_release_tells_both`, `release_by_a_waiter_leaves_the_queue`, `release_not_held_is_an_error`, `hook_waiter_gets_a_grant_tool_waiter_a_claim`, `post_tool_use_releases_by_id_or_all_commands` (a `Pin` and a `Claim` of the same slot stay), `stop_releases_delivered_grants_only`, `drop_slot_hands_on_and_leaves_queues`, `release_item_releases_its_claims`, `locks_waits_and_relays_survive_reopen` (Review Focus 4, ledger half: order of waits and pending state kept), `unanswered_from_counts_one_reply`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon locks:: ledger::` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests and `cargo test --workspace`.
- [ ] **Step 5: Commit** "Keep claims, waits and relays in the ledger".

### Task 3: Target patterns

**Files:**
- Create: `crates/overseer-daemon/src/locks/targets.rs`, `locks/default-targets.toml` (compiled in with `include_str!`), `locks/context.rs`, `crates/overseer-daemon/tests/fixtures/commands.tsv` (invented commands only)
- Modify: `crates/overseer-proto/src/paths.rs` (`targets_path() -> PathBuf`: `$OVERSEER_TARGETS`, else `$HOME/.config/overseer/targets.toml`)

**Interfaces:**
- File format (the defaults file uses it too):

```toml
# replace_defaults = true   drops the compiled-in patterns; otherwise these are checked first
[[pattern]]
name = "git push"
command = '^git\s+(?:-C\s+(?P<dir>\S+)\s+)?push\b'
skip = '\s(?:--dry-run|-n)(?:\s|$)'
target = "repo:{repo}:{branch}"
# kind = "pin" and unpin = ["main", "master", "HEAD"] for pins
# fields = { env = '(?:--env|-e)[=\s]+(\S+)' }   extra values: first group of each regex in the segment
# defaults = { env = "prod" }                     used when a field or capture is missing
```

- Defaults, in order (first match per segment wins): git push to `repo:{repo}:{branch}`; argocd pin (`^argocd\s+app\s+set\s+(?P<app>[^\s-]\S*).*--revision[=\s]+(?P<revision>\S+)`, kind pin, `unpin` main, master, HEAD) to `argo:{argo_ctx}/{app}`; argocd set or sync to `argo:{argo_ctx}/{app}` (skip `--dry-run`); `npx|bunx|pnpm dlx trigger.dev[@<v>] deploy` to `deploy:trigger.dev/{env}` (field `env` from `--env`/`-e`, default `prod`); `terraform|tofu [-chdir=<dir>] apply|destroy` to `tf:{tf_workspace}`; `kubectl [--context X] [-n Y] apply|delete|patch` to `kube:{kube_ctx}/{namespace}` (field `namespace` from `-n`/`--namespace`, default `default`; skip `--dry-run=(client|server)`).
- `struct Pattern` (parsed and compiled); `fn load_patterns(path: &Path) -> (Vec<Pattern>, Option<String>)`: a missing file gives the defaults; a bad file or a bad regex gives the defaults and the error text (logged at daemon start, like `Config::load`).
- `struct Segment { words: Vec<String>, text: String, cwd: PathBuf }`; `fn segments(command: &str, cwd: &Path) -> Vec<Segment>`: quote-aware split on `;`, `&&`, `||`, `|`, `&` and newlines; heredoc bodies (`<<EOF` and `<<'EOF'` to the closing line) are dropped; leading `VAR=value` words and an `env` prefix are dropped; a `cd <dir>` segment changes `cwd` for the segments after it. Patterns match `text`, which starts at the command word, so a write inside quotes or a heredoc never starts a segment.
- `trait Context { fn repo(&self, dir: &Path) -> Option<String>; fn branch(&self, dir: &Path) -> Option<String>; fn argo_ctx(&self) -> Option<String>; fn kube_ctx(&self, flag: Option<&str>) -> Option<String>; fn tf_workspace(&self, dir: &Path) -> Option<String>; }`. `SystemContext { env: Vec<(String, String)>, deadline: Instant, base: BaseEnv }` runs `git -C <dir> rev-parse --git-common-dir` and `--abbrev-ref HEAD`, reads `current-context:` from `$ARGOCD_CONFIG` or `~/.config/argocd/config` (or `--argocd-context`/`$ARGOCD_SERVER`), runs `kubectl config current-context` (a `--context` flag wins), and reads `TF_WORKSPACE`, else `<dir>/.terraform/environment`, else `default`. Each command is killed at the deadline.
- `fn push_branches(words: &[String], current: Option<&str>) -> Vec<String>`: positional words after `push` (options skipped, `-o`/`--push-option`/`--repo` take a value), first is the remote; each refspec gives its destination (`HEAD:main` to `main`, `+main` to `main`, `refs/heads/x` to `x`); none, or `HEAD`, gives the current branch; `--all` or `--mirror` gives `*`.
- `enum HitKind { Command, Pin { revision: String }, Unpin { revision: String } }`; `struct Hit { target: String, kind: HitKind, pattern: String }`
- `fn match_command(patterns: &[Pattern], command: &str, cwd: &Path, ctx: &dyn Context) -> Vec<Hit>`: one hit per target (a push of two refspecs gives two); a placeholder that cannot be filled drops that hit (allow).
- `fn valid_target(t: &str) -> bool`: prefix `repo:`, `argo:`, `deploy:`, `tf:`, `kube:` or `custom:`, a non-empty rest, no whitespace, at most 200 chars.

- [ ] **Step 1: Tests** with a fake `Context` (repo `demo-api`, branch `feature-x`, argo ctx `uat`, kube ctx `staging-eu`, workspace `default`): one test per default pattern; `push_refspecs` (`git push`, `git push -u origin HEAD`, `git push origin HEAD:main`, `git push origin +main feature-y`, `git push --force-with-lease origin feature-x`); `pin_and_unpin` (`--revision abc123` is `Pin`, `--revision main` is `Unpin`); `quoted_and_heredoc_writes_do_not_match` (Review Focus 3: `git commit -m "then git push"`, `echo 'argocd app sync x'`, `cat <<EOF\nkubectl apply -f x\nEOF`, `grep "terraform apply" notes.md`); `dry_runs_and_reads_do_not_match` (`git push --dry-run`, `git push -n`, `kubectl apply --dry-run=client -f x`, `kubectl get pods`, `argocd app get x`, `terraform plan`); `compound_writes_match` (`cd ../demo-api && git push` resolves against `../demo-api`, `git -C ../demo-api push origin HEAD:main`, `FOO=1 terraform apply`, `make build && git push`); `unfillable_placeholder_drops_the_hit`; `user_file_comes_first_and_can_replace_defaults`; `bad_regex_falls_back_with_an_error`; `fixture_corpus` (each line of `commands.tsv` is `<expected targets, comma separated, or ->` TAB `<command>`); `local_corpus` (the same check over `$OVERSEER_CORPUS`, else `~/.config/overseer/corpus/*.tsv`, read-only, returning early when the folder is missing so CI skips it); and a `SystemContext` test against a temp git repo and a temp worktree of it (both give the main repo's folder name).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon targets::` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests.
- [ ] **Step 5: Commit** "Resolve shared write targets from Bash commands".

### Task 4: Lock service, the hook and the launch files

**Files:**
- Modify: `crates/overseer-daemon/src/locks/mod.rs` (`LockService`), `src/config.rs` (`lock_idle_mins: u64`, default 60), `src/server.rs`, `src/launch.rs`, `src/manager.rs` (pass the mcp file into `claude_argv`), `crates/overseer/src/hook.rs`, `crates/overseer/src/main.rs` (load `targets.toml` at daemon start), `crates/overseer-daemon/tests/server.rs`, `crates/overseer/tests/hook.rs`

**Interfaces:**
- `LockService::new(ledger: Arc<Ledger>, patterns: Vec<Pattern>, events: broadcast::Sender<Event>, env: BaseEnv, flag_after: Duration) -> Arc<LockService>`. Every change sends `Event::LocksChanged(view)`.
- `fn view(&self) -> LockView`
- `fn claim(&self, slot, target, reason, item, owned: &[String], now) -> Response`: invalid target or slot is `Error`; `Granted` is `Text { text: "You hold repo:demo-api:main." }`; `AlreadyHeld` is `Text { text: "You already hold repo:demo-api:main." }`; `Queued` is `Queued { message }` with the exact text `repo:demo-api:main is held by S1 since 08:40 for DEMO-33. You are number 1 in the queue; Overseer will tell you here when you hold it.` (local `HH:MM`; ` for <item or reason>` left out when the claim has neither).
- `fn release(&self, slot, target, now) -> Response` (`Text { "Released <target>." }`, plus ` S2 holds it now.` on a handoff), `fn force_release(&self, target, now) -> Response`.
- `async fn check_bash(&self, check: BashCheck, owned: Vec<String>) -> Response`: always `Verdict`. Resolution under a 700 ms deadline, else `deny: None` with nothing recorded. Any hit held by another slot: every held hit queues the slot (`Via::Hook`) and the answer is a deny with the exact text `Overseer: repo:demo-api:main is held by S1 since 08:40 for DEMO-33. You are in its queue and Overseer will tell you here when you hold it. Do not run this again until then.` Otherwise, in one transaction: free hits are claimed (`Command` with the `tool_use_id`, or `Pin` with the revision), the slot's own grants are bound, an `Unpin` of its own pin is bound, and its other holds are touched.
- `fn on_hook(&self, ev: &HookEvent, now)`: `PostToolUse` calls `release_commands_on`, `Stop` calls `release_on_stop_on`. `fn on_slot_exit(&self, slot, now)` calls `drop_slot_on`, ignored once `fn shutdown(&self)` was called. `fn on_item_released(&self, key, now)` calls `release_item_on`.
- Server: `ListLocks`, `ClaimTarget`, `ReleaseTarget`, `ForceRelease` answered in `handle_request`; `CheckBash` answered from a spawned task like `RefreshWorkItems`; the `Hook` arm also calls `locks.on_hook`; a task watching `Event::SlotChanged` calls `on_slot_exit` for `Exited`; `Release` and a successful move in `Assign` call `on_item_released`. `owned` comes from `store.view()`.
- Hook: `overseer hook PreToolUse` reads `tool_name`, `tool_input.command`, `cwd`, `session_id`, `tool_use_id` from stdin; anything but `Bash` returns at once. It sends `CheckBash` within `CHECK_WITHIN = 1 s` (connect included) and on `Verdict { deny: Some(r) }` prints `deny_json(&r)`. `fn deny_json(reason: &str) -> String` gives `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"<reason>"}}`. `parse_hook` also reads `tool_use_id` and `notification_type`.
- Launch: `HOOK_EVENTS` gains `PreToolUse` with matcher `Bash` and `timeout` 5 (the others keep 2). `settings.json` gains `"permissions": {"allow": ["mcp__overseer__claim_target", "mcp__overseer__release_target", "mcp__overseer__notify_session", "mcp__overseer__list_targets"]}`. `write_slot_files` also writes `slots/S<n>/mcp.json`: `{"mcpServers":{"overseer":{"type":"stdio","command":"<bin>","args":["mcp"],"env":{"OVERSEER_SLOT":"S<n>"}}}}`. `LaunchFiles` gains `mcp: PathBuf`; `claude_argv` adds `--mcp-config <mcp.json>` after `--settings`.

- [ ] **Step 1: Tests.** In `tests/server.rs` (temp daemon, stub git on the slot env `PATH` where a repo is needed): `check_bash_claims_a_free_target_and_post_tool_use_releases_it`, `check_bash_denies_a_held_target_and_queues` (deny text exact, `ListLocks` shows the wait), `compound_command_with_one_held_target_claims_nothing`, `unmatched_command_is_allowed_without_records`, `release_grants_the_waiter_and_queues_a_relay`, `pin_survives_post_tool_use_and_unpin_releases_it`, `stop_releases_command_claims`, `holder_exit_hands_on_and_waiter_exit_leaves_the_queue` (Review Focus 5: S1 holds a command claim and a pin, S2 waits on both, S3 waits on the command target; S3 exits, then S1 exits: the command target goes to S2, the pin stays with S1, S3 is in no queue), `releasing_a_work_item_releases_its_claims`, `concurrent_claims_one_wins` (two clients claim one target at once: one `Text`, one `Queued`), `locks_survive_restart` (claims, waits and a pending relay come back the same after shutdown and start), `slow_resolution_allows_and_records_nothing` (stub `git` that sleeps 5 s). In `crates/overseer/tests/hook.rs`: `pre_tool_use_allows_silently_without_a_daemon` and `pre_tool_use_allows_when_the_daemon_never_answers` (Review Focus 1: exit 0, empty stdout, under 1.5 s), `pre_tool_use_prints_the_deny` (a fake daemon answering `Verdict { deny: Some(..) }`; stdout parses as the JSON above), `pre_tool_use_ignores_other_tools`. In `launch.rs`: `settings_file_has_six_hooks` (PreToolUse matcher `Bash`, timeout 5), `settings_allow_the_overseer_tools`, `mcp_file_names_the_slot`, `argv_passes_mcp_config`; `restart_with_listed_command_does_not_double_the_flags` also counts `--mcp-config` once.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon --test server` and `cargo test -p overseer --test hook` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Claim and release targets, and block colliding writes".

### Task 5: Relay requests and the reply rule

**Files:**
- Create: `crates/overseer-daemon/src/relay/mod.rs`
- Modify: `src/server.rs`, `src/work/store.rs` (notices that tell a session also queue a relay), `tests/server.rs`

**Interfaces:**
- `fn may_send(from: Option<SlotId>, to: SlotId, unanswered: Option<i64>) -> Result<Option<i64>, String>`: `None` (Jonathan) and `Some(0)` always pass with no `reply_to`; `from == to` is `Err("you cannot message yourself")`; a worker passes only with an `unanswered` relay from `to`, returning it as `reply_to`; otherwise `Err` with the exact text `S2 can message S1 only to answer a message from S1.`
- `SendRelay { from, to, text }`: validates slots, a non-empty text of at most 4000 chars, then `may_send` with `unanswered_from_on`, then `queue_relay_on` with `Sender::You` or `Sender::Slot`. Answers `Text { text: "Queued for S3. It will be typed in when S3 is idle." }`.
- Store: the move, overlap and promotion notices from phase 3 call `queue_relay_on(conn, &Sender::Overseer, slot, &format!("{notice}"), None, now)` in the same transaction. The `could not label` notices do not.

- [ ] **Step 1: Tests:** unit tests for each `may_send` rule; server tests `jonathan_and_s0_can_send_to_anyone`, `worker_can_answer_s0_once` (deliver a relay from S0 to S2 with `mark_delivered_on`, S2 to S0 passes once, the second is refused), `worker_cannot_start_a_conversation`, `move_notice_queues_a_relay`, `label_failure_queues_no_relay`.
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Queue relays from Jonathan, S0 and replying workers".

### Task 6: Reading Claude Code's input box

**Files:**
- Create: `crates/overseer-daemon/src/relay/screen.rs`
- Modify: `crates/overseer-daemon/Cargo.toml` (`overseer-term` moves from dev-dependencies to dependencies)

**Interfaces:**
- `fn screen_of(snapshot_bytes: &[u8], cols: u16, rows: u16) -> (Snapshot, TermModes)`: replays `Slot::snapshot()` (preamble plus ring) into `TermBridge::new(cols, rows, 0)`. Built only when a relay is due, never on every output chunk.
- `enum InputBox { Empty, Typed, NotFound }`; `fn input_box(snap: &Snapshot) -> InputBox`. The rule, best effort, for Claude Code 2.1's prompt:
  - From the bottom up, find the lowest two rows made mostly of `─` (U+2500, at least 80 percent of the row's non-blank cells and at least half the width). The rows between them are the input box.
  - The first input row starts (after blanks and an optional `│`) with `>` or `❯`. If the rules or the prompt mark are missing, the answer is `NotFound`.
  - The input text is every cell after the prompt mark on the first row and every cell on the rows below it, inside the rules, leaving out `│` borders, cells drawn `dim` (the placeholder hint) and blanks. Any character left gives `Typed`, none gives `Empty`.
  - It cannot detect: a placeholder drawn without the dim attribute (a theme that does this reads as `Typed`, so relays wait), input that is only spaces (reads as `Empty`), text hidden in vim normal mode or scrolled out of a box taller than the screen, and a layout change in a future Claude Code (reads as `NotFound`, so relays wait). Permission and question dialogs replace the box, so they read as `NotFound`, which is the second guard behind the activity check.
- Fixtures are invented ANSI byte strings in the test module that draw the layout: an empty box with a dim placeholder, typed text, a two-line input, a permission dialog (`Do you want to proceed?` with numbered choices and no box), and a narrow 40-column terminal.

- [ ] **Step 1: Tests:** `empty_box_with_dim_placeholder_is_empty`, `typed_text_is_typed`, `second_line_text_is_typed`, `permission_dialog_is_not_found` (Review Focus 2, screen half), `no_rules_is_not_found`, `narrow_terminal_still_found`, `replay_follows_bracketed_paste_mode` (`screen_of` on bytes that set `?2004h` reports `bracketed_paste`).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon relay::screen` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests.
- [ ] **Step 5: Commit** "Read whether Claude's input box is empty".

### Task 7: The courier

**Files:**
- Create: `crates/overseer-daemon/src/relay/courier.rs`
- Modify: `src/activity.rs`, `src/server.rs` (start the task; merge `held` into `LockView`), `tests/server.rs`

**Interfaces:**
- Activity: a `Notification` sets `idle_wait` true when `notification_type` is `idle_prompt`, or, with no type, when the message contains `waiting for your input`. Any other event sets it false.
- `fn ready(state: &SlotState, activity: Option<&Activity>) -> Result<(), &'static str>`: `Ok` only when running and `Idle`, or `NeedsYou` with `idle_wait`. Otherwise one of `slot is not running`, `no activity reported yet`, `session is working`, `waiting at a prompt for Jonathan`.
- `fn compose(relays: &[Relay], to: SlotId) -> String`: each relay on its own paragraph, joined by a blank line. From a slot: `Message from S0 via Overseer: <text>`; from `You`: `Message from Jonathan via Overseer: <text>`; from `Overseer`: the text as stored when it already starts with `S<n> has released` or `Jonathan released`, else `Overseer: <text>`. A worker receiving from a slot gets the line `(You can answer once with notify_session.)` after that relay.
- `Courier::spawn(ledger, manager, locks: Arc<LockService>, events) -> Arc<Courier>`, with `fn poke(&self)`. The loop wakes on `poke`, on `SlotChanged`, and every 1 s while any relay is pending. For each slot with pending relays: `ready`, then `input_box` on `screen_of`, then write `encode_paste(compose(..), &modes)`, wait 100 ms, write `\r`, and `mark_delivered_on` every relay that went. After a delivery the slot is skipped until its activity changes or 10 s pass, so one idle state never gets two pastes. The reason a relay waits (`ready`'s text, `input box has text`, `input box not found`) is kept in memory per slot and returned as `Relay.held`; a change of reason sends `LocksChanged`.
- `SendRelay`, `ClaimTarget`, releases and `on_hook` poke the courier.

- [ ] **Step 1: Tests:** unit tests for `ready` (every state and kind, `idle_wait` both ways), the activity `idle_wait` rule, and `compose` texts. Server tests with a stub `claude` that draws an empty or typed box from a file it rereads on a signal and logs what it reads from the PTY: `idle_slot_gets_the_relay_with_bracketed_paste_then_enter`, `working_slot_waits_until_stop`, `permission_prompt_never_gets_a_relay` (Review Focus 2: `Notification` of type `permission_prompt`, nothing written for 3 s, `held` says `waiting at a prompt for Jonathan`; after `Stop` it goes), `idle_input_wait_gets_the_relay`, `typed_input_box_waits_until_empty`, `two_pending_relays_go_in_one_paste`, `pending_relay_delivers_after_restart_and_resume` (Review Focus 4, delivery half).
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Deliver relays to idle sessions".

### Task 8: `overseer mcp`

**Files:**
- Create: `crates/overseer/src/mcp.rs`, `crates/overseer/tests/mcp.rs`
- Modify: `crates/overseer/src/main.rs` (`Cmd::Mcp`, no logger, nothing on stdout but protocol)

**Interfaces:**
- Newline-delimited JSON-RPC 2.0 on stdin and stdout. `fn handle(msg: &serde_json::Value, slot: SlotId, call: &mut dyn FnMut(Request) -> Result<Response, String>) -> Option<serde_json::Value>`: `None` for notifications.
  - `initialize`: `protocolVersion` echoes the client's when it is `2025-06-18`, `2025-03-26` or `2024-11-05`, else `2025-06-18`; `capabilities: {"tools": {}}`; `serverInfo: {"name": "overseer", "version": <crate version>}`.
  - `ping`: `{}`. `tools/list`: the four tools below with JSON Schema inputs. `tools/call`: `{"content":[{"type":"text","text":...}],"isError":bool}`. Unknown method: error `-32601`. Bad params: error `-32602`.
  - `claim_target {target: string, reason: string, item?: string}` sends `ClaimTarget`; `Queued` is not an error (its message is the text).
  - `release_target {target: string}` sends `ReleaseTarget`.
  - `notify_session {slot: string like "S3" or integer, text: string}` sends `SendRelay { from: Some(own slot) }`.
  - `list_targets {}` sends `ListLocks` and prints one line per claim (`repo:demo-api:main  S1  since 08:40  for DEMO-33`) and per wait (`  waiting: S2`), or `No targets are held.`
  - Daemon `Error` becomes `isError: true` with its message. The daemon is connected per call with a 5 s timeout (it may restart while the session lives); unreachable is `isError: true`, `Overseer is not reachable: <reason>`.
- No `OVERSEER_SLOT`, or a bad one: `initialize` still answers, and every tool call is `isError: true`, `This session is not in an Overseer slot.`

- [ ] **Step 1: Tests:** unit tests for `handle` (initialize version echo and fallback, notification gives `None`, unknown method, bad params, each tool against a fake `call`); `tests/mcp.rs` runs the built binary against a temp daemon: `claim_then_list_then_release`, `held_claim_is_queued_text`, `worker_notify_without_a_message_is_an_error`, `no_daemon_is_a_tool_error_not_a_crash`.
- [ ] **Step 2: Run** `cargo test -p overseer mcp` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Serve the lock and relay tools over MCP".

### Task 9: `ctl` commands and the roles

**Files:**
- Modify: `crates/overseer/src/ctl.rs`, `crates/overseer/tests/cli.rs`, `crates/overseer-daemon/src/roles/worker.md`, `roles/coordinator.md`, `src/launch.rs`
- Create: `crates/overseer-daemon/tests/fixtures/roles-phase3/worker.md` and `coordinator.md` (copies of the current files, made before editing them)

**Interfaces:**
- `ctl locks [--json]`: one line per claim, `repo:demo-api:main  S1  command  since 08:40  for DEMO-33`, with `  held long` when flagged; under it `  waiting: S2 since 08:41`; then pending relays, `relay  S3  pending (session is working)  <text cut to 80>`. `--json` prints the `LockView`.
- `ctl claim <slot> <target> [reason]`: prints the text; `Queued` prints the message and exits 2; `Error` exits 1.
- `ctl release <key>` (unchanged) and `ctl release <slot> <target>`: `Release { first: String, target: Option<String> }`; with a target, `first` is the slot number.
- `ctl force-release <target>`.
- `ctl send <slot> <text> [--raw]`: `SendRelay` with `from` = `$OVERSEER_SLOT` parsed as a slot, else `None`; prints the text. `--raw` keeps the phase 3 behaviour. `ctl_start_send_status` uses `--raw`.
- Worker role gains, exactly:

```
Overseer locks shared write targets (a repo's default branch, an ArgoCD app or pin, a deploy, a Terraform workspace, a Kubernetes namespace) so two sessions never write to one at once.
- Before you push, deploy, apply or pin, call the `claim_target` tool with the target and why. If another session holds it you are queued, and Overseer tells you here when you hold it. Wait for that message; do not retry first.
- Call `release_target` as soon as you are done with a target you claimed. A pin stays yours until you set the app back to main or release it.
- If Overseer denies a command because a target is held, you are already in its queue. Work on something else or stop and wait.
- Messages that start with "Message from" or "Overseer:" come from Overseer. You may answer a message from another session once, with `notify_session`. You cannot start a conversation with another session.
- `list_targets` shows who holds what.
```

- Coordinator role: the `ctl send` rule becomes "To give work to a running slot, call `notify_session` with the slot and the text, or run `"{overseer}" ctl send <n> '<text>'`. It is typed in when that session is idle." It gains "`list_targets` shows who holds which target and who is waiting. Never claim targets on a worker's behalf." The closing `ctl send 0` line gains `--raw`.
- `PREVIOUS_WORKER` gains `0xbd3ab0020d4d25b1` and `PREVIOUS_COORDINATOR` gains `0x7ddaa223f4bdd1f6` (the phase 3 shipped texts; the test checks them).

- [ ] **Step 1: Tests:** `cli.rs`: `claim_held_exits_2`, `locks_lists_holder_and_waiter`, `release_two_args_is_a_target`, `send_queues_a_relay` (`ctl locks` shows it pending). `launch.rs`: `phase_3_roles_upgrade` (the fixture texts hash into the `PREVIOUS_*` tables, and `ensure_template` replaces a fixture copy with the new default), `worker_role_names_the_tools`, and the existing `placeholders_are_filled` and no-em-dash checks updated for the new text.
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Add lock and relay commands to ctl and the roles".

### Task 10: Board pills, blocked state and the relay strip

**Files:**
- Modify: `crates/overseer-gui/src/app.rs` (hold `LockView`, fetch with `ListLocks` on connect, apply `LocksChanged`), `board.rs`, `notify.rs`

**Interfaces:**
- `Tone` gains `Blocked` (red, `tone_color` and `edge_color`).
- `card_model(info, now, home, work, locks: &LockView)`. `CardModel` gains `locks: Vec<String>`, `waiting_on: Option<String>`, `pending: Option<String>`.
- `fn lock_pills(view: &LockView, slot: SlotId, now: i64) -> Vec<String>`: the targets the slot holds, pins first; a flagged one ends with ` · held long`; more than three become three plus `+N`.
- A slot with waits whose activity is not `Working` gets `Tone::Blocked`, badge `blocked by lock`, and `waiting_on` `waiting on repo:demo-api:main` (`and 1 more` after the first). While `Working` the tone is unchanged and `waiting_on` still shows.
- `fn pending_label(n: usize) -> Option<String>`: `None`, `1 relay pending`, `2 relays pending`.
- `fn strip_lines(view: &LockView, clock: impl Fn(i64) -> String, max: usize) -> Vec<String>`: newest first; delivered `14:05 told S3: <text>`, pending `14:05 for S3, pending: <text>`, text cut to 80 chars on one line. The board's bottom strip shows 5.
- Notification: a new wait in `LocksChanged` whose slot was not waiting on that target before fires `S2 blocked: repo:demo-api:main is held by S1` (spec 7.9).

- [ ] **Step 1: Tests** (pure): `lock_pills_pins_first_and_flagged`, `lock_pills_collapse_after_three`, `waiting_slot_is_blocked_when_not_working`, `working_slot_keeps_its_tone_but_shows_the_wait`, `pending_label_counts`, `strip_lines_newest_first_and_cut`, `new_wait_notifies_once`.
- [ ] **Step 2: Run** `cargo test -p overseer-gui` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests, then launch the app against a temp daemon with claims, a wait and relays made through `ctl`, and take a screenshot of the board for the report.
- [ ] **Step 5: Commit** "Show locks, waits and relays on the board".

### Task 11: Holds and relays in the session panel

**Files:**
- Modify: `crates/overseer-gui/src/session.rs`, `app.rs` (key bindings, small text-entry rows reusing `TextField`)

**Interfaces:**
- `struct HoldRow { target: String, kind: String, since: String, flagged: bool, waiters: Vec<SlotId> }`; `fn holds_rows(view: &LockView, slot: SlotId, now: i64, clock) -> Vec<HoldRow>`; `fn waits_rows(view, slot) -> Vec<String>` (`waiting on argo:uat/root, number 2`).
- `fn relay_rows(view: &LockView, slot: SlotId, clock) -> Vec<String>`: relays to or from the slot, newest first: `14:05 from S0: <text>`, `14:06 to S0: <text>`, a pending one ending `(pending: session is working)`.
- The Holds placeholder becomes the Holds box: each hold with a Release button (sends `ForceRelease { target }`, since Jonathan acts for the holder, after an inline `Release <target>? Enter / Esc` confirm), the waits, and a `Claim…` button opening a one-line entry `<target> [reason]` that sends `ClaimTarget` for this slot (a `Queued` answer is shown inline). A Relays box lists `relay_rows` with a `Message…` button opening an entry that sends `SendRelay { from: None, to: slot }`.
- Keys in the session view, through the modifier rule (spec 7.9): `cmd-shift-m` opens the message entry, `cmd-shift-r` opens the release confirm for the first hold (Up and Down pick another when there are several). Neither reaches the terminal; plain keys still do.

- [ ] **Step 1: Tests** (pure): `holds_rows_list_kind_since_and_waiters`, `relay_rows_both_directions_with_pending_reason`, and in the `routed()` table: `cmd-shift-m` and `cmd-shift-r` route in the session view and not in dialogs, `m` and `r` still go to the terminal.
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`, then in the real app against a temp daemon: claim from the panel, see the pill on the card, release from the panel, send a message to an idle stub session.
- [ ] **Step 5: Commit** "Add Holds and relays to the session panel".

### Task 12: Acceptance list

**Files:**
- Create: `docs/acceptance/phase-4.md` (the same layout as `phase-3.md`: Setup, then tables of ID, Check, Steps, Pass, Result, then Findings, Known limitations, Sign-off)

Cover: `targets.toml` loading, a bad file falling back, and `OVERSEER_TARGETS`; each default pattern against invented commands in a temp git repo; the read-only and quoted cases (Review Focus 3); hook allow, deny, auto-claim and release on `PostToolUse` and `Stop`; the hook with no daemon and with a hung daemon (Review Focus 1, timed); pins and unpin; FIFO handoff with the relay text; force-release; work-item release releasing claims; holder and waiter exit (Review Focus 5); restart keeping claims, waits and pending relays (Review Focus 4); relay delivery when idle, waiting while working, never at a permission prompt, waiting on a typed input box (Review Focus 2); the reply rule; `ctl` commands; the MCP tools seen and called from a real `claude` session under a temp `OVERSEER_HOME` (that `OVERSEER_SLOT` and the socket reach the MCP server, and that the tools need no permission prompt); the input-box reader against the real Claude Code 2.1 prompt (empty, typed, multi-line, a permission dialog); the GUI pills, blocked card, strip, Holds and keys; the denied-write notification. Run every check that does not need Jonathan through a temp daemon and stubs and record the results; leave the GUI and real-session rows for Jonathan. Known limitations to record: background commands release early, pins only end on a default-branch revision, the input-box rule's blind spots (Task 6), `tf:<workspace>` sharing `default`.

- [ ] **Step 1: Write** the list, run the scriptable checks, record the results.
- [ ] **Step 2: Commit** "Add the phase 4 acceptance list".
