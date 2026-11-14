# Phase 6: Coordinator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** S0 becomes a real coordinator. It reads the whole board through MCP tools, proposes a structured plan that Jonathan approves, edits or rejects in the GUI, asks workers for status and gets their answers back, and keeps a summary at the top of the board. Once a plan is approved, the daemon assigns the items, creates worktrees, pre-claims targets, starts empty slots and sends each kickoff.

**Architecture:** A new `coord` module in the daemon. `CoordService` (`coord/mod.rs`) owns the board summary, progress reports, status requests, a per-slot state timeline and the plans, all in the ledger (schema v5), and announces a `CoordView`. `coord/plan.rs` validates a plan against the live slots, work items and locks, and runs an approved plan through the existing `WorkStore::assign`, `LockService::claim`, `SlotManager::start` and `relay::send`. `coord/worktree.rs` runs `git worktree add`. The MCP server gains six tools, `ctl` gains `board`, `plan`, `summary`, `status-of` and `report`, and the GUI gains the summary panel, a Progress timeline, a plan notification and a plan approval view.

**Tech Stack:** Rust, tokio (`process`, `sync`), rusqlite (bundled), `serde_json`, `toml` 0.8, the `git` binary, GPUI 0.2.2.

**Spec:** `docs/superpowers/specs/2026-11-09-overseer-design.md`, section 7.7 (the core), 7.4 (ownership rules the plan applies), 7.5 (targets and worktrees), 7.6 (relays and delivery), 7.9 (board header, coordinator summary panel, Progress, plan approval view), 7.10 (ledger) and 9 item 6.

## Global Constraints

- The repo is public. No real company names, ticket keys, sprint names, hostnames or vault content in defaults, tests, fixtures or docs. Invented examples only (`DEMO-12`, `ov-3`, `demo-api`, `sprint-26-01`, `argo:uat/root`).
- Tests never touch the installed app, the LaunchAgent `uk.co.techize.overseer`, its socket, `~/Library/Application Support/overseer`, the real `~/Repos/2ndBrain`, the real `~/Repos/.worktrees` or the real `~/.claude/CURRENT_SESSION.md`. Worktree tests run on a throwaway repo under the temp dir.
- Never touch `~/.claude/settings.json`, `~/.claude.json`, or global hooks and skills from code.
- Jonathan approves every allocation (spec 7.7, phase 2 ruling). No code path lets a session approve a plan, and nothing in a plan runs before approval.
- Nothing acts silently (spec 4): every approval, rejection, assignment, worktree, claim, start and kickoff is recorded in the plan's outcome, shown in the GUI and relayed to S0.
- Text Overseer writes (relays, outcomes, summaries shown) has no AI language, no emojis and no em dashes. Em dashes in a summary or report become `-`.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass at the end of every task.
- The protocol stays backward compatible: new fields on existing structs use `#[serde(default)]`; new `Request`, `Response` and `Event` variants go at the end of their enums; new types are re-exported from `crates/overseer-proto/src/lib.rs`.

## Rulings (gaps this plan fills)

1. **One pending plan at a time.** A new `propose_plan` while one is pending marks the old one `Superseded` and returns `Plan <n> is waiting for Jonathan. It replaces plan <m>.` (or without the second sentence). Cost if wrong: S0 can't keep two alternatives open, which it can put in one plan's summary instead.
2. **Who may do what.** `propose_plan`, `request_status` and `set_board_summary` come from S0, or from Jonathan through `ctl` (no `OVERSEER_SLOT`). From S1 to S6 they fail with `only the coordinator (S0) can <propose a plan|ask for a status|set the board summary>`. `report_progress` comes from any slot. `get_board` and `get_work_items` are open to every slot (read only). `DecidePlan` with a slot set fails with `only Jonathan can approve or reject a plan`. `ctl` reads `OVERSEER_SLOT`, so `ctl plan approve` run inside a session is refused. A session that unsets the variable gets past this, which goes in the acceptance list's known limitations (guard rails, as with the hook).
3. **Plan shape** (JSON for the MCP tool, the same structs on the wire):
   `{"summary": "...", "slots": [{"slot": "S3", "label": "demo lane", "items": ["DEMO-12", "ov-3"], "targets": [{"target": "repo:demo-api:main", "reason": "merge"}], "worktree": {"repo": "/path/to/demo-api", "ticket": "DEMO-12"}, "cwd": "/path", "kickoff": "...", "rules": ["Do not deploy to UAT before S2 merges"]}]}`. `slot` takes `"S3"` or `3`. Every field but `slot` is optional. `rules` are the spec's collision rules: plain lines put into the kickoff.
4. **Validation** runs at propose time and again at approval, against the state at that moment. A refusal blocks the whole plan with one message; a warning is listed on the plan and approval counts as confirming it. Refusals, in this order, with exact texts:
   - no slots: `the plan has no slots`
   - `slot` 0 or over 6: `S0 is the coordinator; a plan assigns S1 to S6` / `invalid slot id <n>`
   - the same slot twice: `S3 appears twice in the plan`
   - a resumable slot: `S3 has a session to resume; resume or stop it before planning for it`
   - an unknown item: `no work item <key>`
   - one item in two slots: `<key> appears in S2 and S3`
   - an item owned by a slot other than the plan's: `owned_by_message(key, owner)` (the existing `<key> is owned by S<n>`)
   - an invalid target (`locks::valid_target` false): `<target> is not a target (repo:, argo:, deploy:, tf:, kube:, custom:)`
   - a worktree repo that is not a git repository: `<repo> is not a git repository`
   - a worktree ticket not matching `^[A-Za-z0-9._-]{1,64}$`: `<ticket> cannot be a worktree name`
   - a `cwd` that is not a directory: `<cwd> is not a folder`
   - an empty or exited slot with no `cwd` and no `worktree`: `S4 is empty and the plan gives it no cwd or worktree`
   - a kickoff over 2000 characters: `the kickoff for S3 is longer than 2000 characters`
   Warnings: `ownership::decide` returning `Warn{message}` (its message, verbatim); a target held by another slot: `<target> is held by S1; S3 will queue for it`; a running slot given a worktree: `S3 is running; it will be told to work in <path> but not moved`.
5. **Running an approved plan**, slot by slot in slot order, each step's result one outcome line. A failed step skips that slot's later steps (`S3: <step> failed: <error>; the rest of S3's plan was skipped`) and the next slot goes on. Steps:
   1. assign each item with `WorkStore::assign(key, slot, confirm=true, move_owner=false)`: `S3: assigned DEMO-12, ov-3`
   2. worktree (Ruling 6): `S3: worktree <path> created` or `S3: worktree <path> reused`
   3. claim each target with `LockService::claim(slot, target, reason, None, &owned, now)`: `S3: holds repo:demo-api:main` or `S3: queued for argo:uat/root behind S1`
   4. kickoff text (Ruling 7), then for an empty or exited slot `SlotManager::start(slot, label, dir, None, Some(kickoff))` where `dir` is the worktree, else `cwd`: `S3: started in <dir>`; for a running slot `relay::send(Sender::Overseer, slot, kickoff)`: `S3: kickoff queued`. A label left empty becomes the first item key, else `work`.
   The plan is marked `Approved` (with `edited` true when the approved plan differs from the proposed one) before step 1, inside the same ledger transaction that checks it is still `Pending`, so a second approval gets `plan <n> is no longer pending` and nothing runs twice. Then S0 gets a relay from `Sender::Overseer`: `Jonathan approved plan <n>` + ` with edits` when edited + `.` + newline + the outcome lines, cut to 4000 characters. A rejection: `Jonathan rejected plan <n>: <reason>` (`no reason given` when empty). Relays to S0 are queued only when S0 has a live session.
6. **Worktrees.** `worktree_root = "~/Repos/.worktrees"` in `config.toml`. Path: `<worktree_root>/<repo folder name>-<ticket>`. If the path exists and `git -C <path> rev-parse --git-common-dir` resolves to the same common dir as the repo's, it is reused. If it exists and does not, the step fails with `<path> exists and is not a worktree of <repo>`. Else `git -C <repo> worktree add -b <ticket> <path>`, and when that branch exists already, `git -C <repo> worktree add <path> <ticket>`. Never `--force`, never removes anything.
7. **Kickoff text.** The manual template (`assignment.md`) gains placeholders and a plan kickoff is that template filled for the slot, followed by a blank line and the plan's own `kickoff` text when it has one. Lines whose placeholder expands to nothing are dropped. Over 4000 characters (the relay limit) the running-slot step fails with `kickoff is <n> characters; the limit is 4000`. The same template serves the GUI's manual kickoff, so spec 7.7's "manual assignment" gets held targets, worktree and the CURRENT_SESSION pointer too.
8. **Status requests.** `request_status(slot)` needs the slot running, else `S3 is not running`. It records a pending request and queues a relay from `Sender::Overseer`: `S0 asks for a short status. Answer with the report_progress tool in two or three lines.` The next `report_progress` from that slot clears the request and, when S0 is live, relays `S3 reports: <text>` to S0. A second request while one is pending replaces its time and does not send a second relay (`already asked S3 at 14:05`).
9. **Limits.** Summary and report text: 1 to 2000 characters after trimming (`the summary is empty` / `the summary is longer than 2000 characters`, same for `report`). Em dashes become `-`.
10. **Timeline.** `CoordService` watches `SlotChanged` and records a `State` row when a slot's state variant or activity kind changes: `started`, `working`, `needs you: <reason>` (reason when present), `idle`, `exited`. Kept: the newest 200 rows per slot (reports and states together). `CoordView.reports` carries the newest 100 overall.
11. **Board header.** Spec 7.9 wants sprint name and day; no source gives them yet, so they wait for phase 7. This phase adds the done count: `done <d>/<n>` over tickets in the list, where done means status `done`, `closed` or `resolved` (case-insensitive). Cost if wrong: one header field.
12. **`get_board` text** comes from `ctl::format_board(&BoardView, now) -> String`, shared by the MCP tool and `ctl board`. Sections in this order: `Summary (HH:MM): <text>` or `Summary: none yet`; one line per slot S0 to S6; `Work items:` then `format_work`; `Locks:` then `format_locks`; `Plan <n> waiting for Jonathan since HH:MM` when one is pending. A slot line: `S3 <label>  <state>  owns DEMO-12, ov-3  holds repo:demo-api:main  waiting argo:uat/root  <k> relays pending  status asked HH:MM`, each part present only when non-empty, then an indented `last report HH:MM: <text>` when the slot has one. `<state>` is `empty`, `exited`, `resumable`, `working 5m`, `needs you 2m`, `idle 12m`.

## Review Focus

1. **The board changes between proposing and approving** (Jonathan assigns an item by hand, a slot starts). Approval re-validates and refuses with the current reason; the plan stays pending and nothing runs. Test in Task 4.
2. **Approval arrives twice** (double click, GUI and `ctl` at once). Exactly one runs; the other gets `plan <n> is no longer pending`. Test in Task 4.
3. **A worktree folder already exists** from an earlier day. A real worktree of the repo is reused; any other folder fails that slot only and the other slots still run. Tests in Task 3 and Task 4.
4. **A worker tries to steer** (S3 calls `propose_plan`, `set_board_summary` or `request_status`, or runs `ctl plan approve`). Each is refused with the Ruling 2 text. Tests in Task 2, Task 4 and Task 6.
5. **The daemon restarts with a plan pending, or just after approving one.** The pending plan is still pending with its warnings; an approved plan is never run again. Test in Task 4.

---

### Task 1: Ledger v5, protocol and config

**Files:**
- Modify: `crates/overseer-daemon/src/ledger.rs` (migrate, V5 schema, coord helpers)
- Modify: `crates/overseer-proto/src/msg.rs`, `crates/overseer-proto/src/lib.rs`
- Modify: `crates/overseer-daemon/src/config.rs`

**Interfaces:**
- Produces (proto, all `Serialize, Deserialize, Clone, Debug, PartialEq`):
  - `struct PlanTarget { target: String, #[serde(default)] reason: Option<String> }`
  - `struct PlanWorktree { repo: PathBuf, ticket: String }`
  - `struct PlanSlot { slot: SlotId, #[serde(default)] label: String, #[serde(default)] items: Vec<String>, #[serde(default)] targets: Vec<PlanTarget>, #[serde(default)] worktree: Option<PlanWorktree>, #[serde(default)] cwd: Option<PathBuf>, #[serde(default)] kickoff: String, #[serde(default)] rules: Vec<String> }`
  - `struct Plan { #[serde(default)] summary: String, slots: Vec<PlanSlot> }`
  - `enum PlanState { Pending, Approved, Rejected, Superseded }`
  - `struct PlanRecord { id: i64, at: i64, plan: Plan, state: PlanState, decided_at: Option<i64>, edited: bool, warnings: Vec<String>, outcome: Vec<String> }`
  - `enum ReportKind { Report, State }`; `struct Report { id: i64, at: i64, slot: SlotId, kind: ReportKind, text: String }`
  - `struct Summary { at: i64, text: String }`
  - `struct CoordView { summary: Option<Summary>, plans: Vec<PlanRecord>, reports: Vec<Report>, status_asked: Vec<(SlotId, i64)> }` with `fn pending_plan(&self) -> Option<&PlanRecord>` and `fn reports_for(&self, slot: SlotId) -> impl Iterator<Item = &Report>` (newest first)
  - `struct BoardView { slots: Vec<SlotInfo>, work: WorkView, locks: LockView, coord: CoordView }`
  - `enum PlanDecision { Approve { plan: Plan }, Reject { #[serde(default)] reason: String } }`
  - `Request::{GetBoard, CoordStatus, ProposePlan { from: Option<SlotId>, plan: Plan }, DecidePlan { id: i64, from: Option<SlotId>, decision: PlanDecision }, RequestStatus { from: Option<SlotId>, slot: SlotId }, ReportProgress { slot: SlotId, text: String }, SetBoardSummary { from: Option<SlotId>, text: String }}`
  - `Response::{Board(BoardView), Coord(CoordView)}`; `Event::CoordChanged(CoordView)`
- Produces (daemon): `Config.worktree_root: PathBuf` (default `~/Repos/.worktrees`, `~` expanded like `brain_dir`). Ledger methods: `add_plan(&self, plan: &Plan, warnings: &[String], now) -> Result<(i64, Option<i64>)>` (new id, superseded id), `plans(&self, limit: usize) -> Result<Vec<PlanRecord>>` (pending first, then newest), `decide_plan_on(conn, id, state: PlanState, plan: &Plan, edited: bool, now) -> Result<bool>` (false when not pending), `set_plan_outcome(&self, id, outcome: &[String]) -> Result<()>`, `add_report(&self, slot, kind, text, now) -> Result<i64>` (prunes to 200 per slot), `reports(&self, limit) -> Result<Vec<Report>>`, `set_summary(&self, text, now)`, `summary(&self) -> Result<Option<Summary>>`, `ask_status(&self, slot, now) -> Result<Option<i64>>` (previous time if already asked), `answer_status(&self, slot) -> Result<bool>`, `status_asked(&self) -> Result<Vec<(SlotId, i64)>>`.

- [ ] **Step 1: Write the failing tests**
  - `ledger.rs`: `v4_file_migrates_to_v5_keeping_rows` (build a v4 file with the old batch, add a session with `session_note` and a queue row, reopen, assert `user_version` 5, rows kept, `plans` table usable); `v3_file_migrates_to_v5`; `fresh_file_is_v5`; `second_plan_supersedes_the_pending_one` (`add_plan` twice returns `(2, Some(1))`, `plans(10)[..]` states Pending then Superseded); `deciding_twice_runs_once` (`decide_plan_on` true then false); `reports_keep_200_per_slot` (insert 205 for S3 and 3 for S4, `reports(1000)` has 200 for S3 and 3 for S4); `status_ask_and_answer` (`ask_status` None then Some(t1); `answer_status` true then false).
  - `msg.rs`: `plan_round_trips_with_defaults` (`{"slots":[{"slot":3}]}` parses to a `PlanSlot` with empty label, items, targets, kickoff); round trips for the new `Request`, `Response` and `Event` variants.
  - `config.rs`: `worktree_root_defaults_and_expands`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon --lib -- ledger:: config:: && cargo test -p overseer-proto`. Expected: compile errors for the missing items.
- [ ] **Step 3: Implement.** `migrate` runs each step only when `version < N`: `< 1` sessions, `< 2` V2, `< 3` V3, `< 4` V4, `< 5` V5, all in one transaction, then sets `user_version = 5`. V5 schema: `plans(id INTEGER PRIMARY KEY, at INTEGER NOT NULL, state TEXT NOT NULL, plan TEXT NOT NULL, proposed TEXT NOT NULL, decided_at INTEGER, edited INTEGER NOT NULL DEFAULT 0, warnings TEXT NOT NULL DEFAULT '[]', outcome TEXT NOT NULL DEFAULT '[]')`, `reports(id INTEGER PRIMARY KEY, at INTEGER NOT NULL, slot INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL)`, `board_summary(id INTEGER PRIMARY KEY CHECK (id = 1), at INTEGER NOT NULL, text TEXT NOT NULL)`, `status_requests(slot INTEGER PRIMARY KEY, at INTEGER NOT NULL)`. `plan` and `proposed` hold JSON (`proposed` never changes after insert).
- [ ] **Step 4: Run** the Step 2 command and `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Add the ledger v5 tables and coordinator types`.

### Task 2: Coordinator service: summary, reports, status requests, timeline, board

**Files:**
- Create: `crates/overseer-daemon/src/coord/mod.rs`
- Modify: `crates/overseer-daemon/src/lib.rs`, `crates/overseer-daemon/src/server.rs`
- Test: `crates/overseer-daemon/tests/server.rs`

**Interfaces:**
- Consumes: Task 1 ledger methods and types; `relay::send`, `LockService`, `Sender::Overseer`.
- Produces: `pub struct CoordService` with `new(ledger: Arc<Ledger>, events: broadcast::Sender<Event>) -> Arc<Self>`, `view(&self) -> CoordView`, `set_summary(&self, from: Option<SlotId>, text: &str, now) -> Response`, `report(&self, slot, text: &str, live_s0: bool, locks: &LockService, now) -> Response`, `request_status(&self, from: Option<SlotId>, slot, running: bool, locks: &LockService, now) -> Response`, `watch(self: &Arc<Self>, events: broadcast::Receiver<Event>)` (spawns the timeline task), `pub fn clean_text(text: &str, what: &str) -> Result<String, String>` (Ruling 9). Server: `Request::GetBoard` answers `Response::Board(BoardView{..})`, `Request::CoordStatus` answers `Response::Coord`.

- [ ] **Step 1: Write the failing tests** (`tests/server.rs`, using `start_daemon`, `box_stub` and the relay helpers):
  - `summary_from_s0_shows_on_the_board`: `SetBoardSummary{from: Some(0), text: "Two lanes \u{2014} both green"}` gives `Ok`; `GetBoard` has `coord.summary.text == "Two lanes - both green"`; a `CoordChanged` event arrived.
  - `summary_from_a_worker_is_refused`: `from: Some(3)` gives `Error{message: "only the coordinator (S0) can set the board summary"}`.
  - `summary_limits`: empty gives `the summary is empty`; 2001 characters gives `the summary is longer than 2000 characters`.
  - `status_request_relays_and_the_answer_comes_back`: S0 and S3 running box stubs; `RequestStatus{from: Some(0), slot: 3}` gives `Text` and S3 receives the Ruling 8 text; `ReportProgress{slot: 3, text: "Tests pass, PR next"}` gives `Ok` and S0 receives `S3 reports: Tests pass, PR next`; `status_asked` is empty again.
  - `second_status_request_sends_one_relay`: the second answers `already asked S3 at HH:MM`; S3 has one relay.
  - `status_of_a_stopped_slot_is_refused`: `S3 is not running`.
  - `timeline_records_state_changes`: start S3, send `UserPromptSubmit` then `Stop` hooks; `CoordStatus` reports for S3 include `started`, `working`, `idle` in that order (oldest first when reversed).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon --test server -- summary_ status_ timeline_`. Expected: FAIL (unknown requests).
- [ ] **Step 3: Implement** `CoordService` and wire it in `server::run` beside `LockService` (shared `Arc`, event sender clone, `watch` on its own receiver). `GetBoard` builds the view from `SlotManager::list()`, `WorkStore::view()`, `LockService::view()` and `CoordService::view()`. Poke the courier after `RequestStatus` and `ReportProgress`, as `handle_frame` does after `SendRelay`.
- [ ] **Step 4: Run** the Step 2 command, then `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Add the coordinator service with summary, reports and status requests`.

### Task 3: Worktrees

**Files:**
- Create: `crates/overseer-daemon/src/coord/worktree.rs`

**Interfaces:**
- Produces: `pub enum Worktree { Created(PathBuf), Reused(PathBuf) }`; `pub fn worktree_path(root: &Path, repo: &Path, ticket: &str) -> PathBuf`; `pub async fn ensure_worktree(root: &Path, repo: &Path, ticket: &str) -> Result<Worktree, String>`; `pub fn is_git_repo(path: &Path) -> bool` (used by validation).

- [ ] **Step 1: Write the failing tests** (module `tests`, throwaway repo under `tempfile::tempdir()` with one commit):
  - `path_is_root_repo_name_ticket`: `worktree_path("/w", "/r/demo-api", "DEMO-12") == "/w/demo-api-DEMO-12"`.
  - `creates_a_worktree_on_a_new_branch`: result `Created(p)`; `git -C p branch --show-current` prints `DEMO-12`.
  - `reuses_its_own_worktree`: second call gives `Reused(p)`.
  - `uses_an_existing_branch`: `git branch DEMO-13` first; result `Created`, branch `DEMO-13`.
  - `refuses_a_folder_that_is_not_its_worktree`: a plain folder at the path gives `Err("<path> exists and is not a worktree of <repo>")`; the folder is untouched.
  - `not_a_repo`: `is_git_repo` false for a plain folder, true for the repo.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon coord::worktree`. Expected: FAIL.
- [ ] **Step 3: Implement** with `tokio::process::Command` per Ruling 6, creating `root` with `create_dir_all` first. Errors carry git's first stderr line.
- [ ] **Step 4: Run** the Step 2 command. Expected: PASS.
- [ ] **Step 5: Commit** `Create or reuse a worktree for a planned ticket`.

### Task 4: Plans: propose, validate, approve, reject, run

**Files:**
- Create: `crates/overseer-daemon/src/coord/plan.rs`
- Modify: `crates/overseer-daemon/src/coord/mod.rs`, `crates/overseer-daemon/src/server.rs`, `crates/overseer-daemon/src/roles/assignment.md`, `crates/overseer-daemon/src/launch.rs`, `crates/overseer-daemon/src/work/store.rs` (`kickoff_text`)
- Test: `crates/overseer-daemon/tests/server.rs`

**Interfaces:**
- Consumes: Tasks 1 to 3; `ownership::decide`, `owned_by_message`, `locks::valid_target`, `LockService::{claim, view, owned_items}`, `WorkStore::{assign, view}`, `SlotManager::{list, start}`, `relay::send`, `relay::MAX_TEXT`.
- Produces:
  - `pub struct Facts<'a> { slots: &'a [SlotInfo], work: &'a WorkView, locks: &'a LockView, owners: &'a HashMap<String, SlotId> }`
  - `pub fn validate(plan: &Plan, facts: &Facts) -> Result<Vec<String>, String>` (warnings, or the first refusal; Ruling 4 order and texts)
  - `pub fn kickoff_for(template: &str, slot: &PlanSlot, items: &[WorkItem], held: &[String], worktree: Option<&Path>, brain_dir: &Path, current_session: &Path) -> String` (Ruling 7)
  - `CoordService::propose(&self, from: Option<SlotId>, plan: Plan, facts: &Facts, now) -> Response` (`Text` with Ruling 1 wording, or `Error`)
  - `CoordService::decide(self: &Arc<Self>, id: i64, from: Option<SlotId>, decision: PlanDecision, ctx: &PlanCtx, now) -> Response` where `PlanCtx` holds the `Arc`s for `WorkStore`, `LockService`, `SlotManager`, the config (`worktree_root`, `brain_dir`, `current_session`) and the assignment template path. Approve answers `Text` with the outcome lines joined by newlines; reject answers `Ok`.
  - `kickoff_text` keeps its signature for the GUI path and gains the new placeholders through a `KickoffFacts { held: Vec<String>, worktree: Option<PathBuf>, rules: Vec<String>, current_session: PathBuf }` argument.
  - New `assignment.md` default: the current text with these lines added after the folders line, each dropped when empty: `You hold these targets: {targets}`, `Work in {worktree}.`, `Keep your section of {current_session} current with update_current_session.`, then `Rules for this work:` followed by `{rules}` (one `- ` line each). Add the old text's `text_hash` to `PREVIOUS_ASSIGNMENT`.
- [ ] **Step 1: Write the failing tests**
  - `plan.rs` unit tests for `validate`, one per Ruling 4 refusal (exact text) and per warning, plus `valid_plan_has_no_warnings`; `kickoff_for` tests: `kickoff_lists_items_targets_worktree_rules_and_plan_text` and `kickoff_drops_empty_lines`.
  - `tests/server.rs` (with `stub_sources`, `box_stub`, a throwaway repo for worktrees):
    - `propose_from_a_worker_is_refused`: `only the coordinator (S0) can propose a plan`.
    - `proposed_plan_is_pending_and_announced`: `CoordChanged` with one `Pending` plan; warnings from a split-off bead are listed.
    - `new_plan_supersedes_the_old`: text ends `It replaces plan 1.`; plan 1 `Superseded`.
    - `approve_runs_every_step`: plan for empty S4 (items `DEMO-12`, target `repo:demo-api:main`, worktree on the throwaway repo, kickoff `Start with the parser.`) and running S3 (item `ov-4`, kickoff `Pick this up next.`). Approve with the same plan. Assert: `ctl items` shows `DEMO-12` owned by S4 and `ov-4` by S3; S4 holds `repo:demo-api:main`; the worktree exists; S4 started with cwd the worktree and its stub argv's last arg contains `DEMO-12`, the worktree path and `Start with the parser.`; S3 got a relay containing `Pick this up next.`; S0 got `Jonathan approved plan 1.` with the outcome lines; the plan is `Approved`, `edited` false.
    - `approve_with_edits_says_so`: approve with S3 removed; only S4 runs; S0's relay starts `Jonathan approved plan 1 with edits.`; `edited` true.
    - `approval_revalidates`: propose, then `Assign{key: "DEMO-12", slot: 2}` by hand, then approve: `Error{"DEMO-12 is owned by S2"}`; plan still `Pending`; S4 not started.
    - `second_approval_is_refused`: two `DecidePlan` approve requests on two connections at once: one `Text`, one `Error{"plan 1 is no longer pending"}`; S4 started once (stub log has one line).
    - `a_failing_slot_does_not_stop_the_others`: S4's worktree path is a plain folder; S4's outcome has `worktree ... failed: ... exists and is not a worktree of ...; the rest of S4's plan was skipped`; S5 in the same plan still starts.
    - `reject_tells_s0`: `Reject{reason: "not today"}`; S0 gets `Jonathan rejected plan 1: not today`; state `Rejected`.
    - `decide_from_a_slot_is_refused`: `from: Some(0)` gives `only Jonathan can approve or reject a plan`.
    - `pending_plan_survives_a_restart_and_approved_never_reruns`: propose, restart the daemon on the same state dir: plan still `Pending` with its warnings; approve, restart: plan `Approved`, S4 started once.
  - `launch.rs`: `previous_assignment_hash_is_listed` (the old default's hash is in `PREVIOUS_ASSIGNMENT`), using a new fixture `tests/fixtures/roles-phase5/assignment.md` (copy of the current file).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon coord::plan && cargo test -p overseer-daemon --test server -- propose_ approve_ reject_ decide_ pending_plan_ new_plan_ a_failing_ second_approval_`. Expected: FAIL.
- [ ] **Step 3: Implement** per Rulings 1, 4, 5, 6 and 7. `decide` re-runs `validate` on the plan being approved with fresh facts, then marks it `Approved` in one transaction (`decide_plan_on` false gives the not-pending error), then runs the steps outside the transaction and stores the outcome with `set_plan_outcome`, then announces. Wire `ProposePlan` and `DecidePlan` in `handle_request`; poke the courier after both.
- [ ] **Step 4: Run** the Step 2 command, then `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Approve, reject and run coordinator plans`.

### Task 5: Manual kickoff uses the new template

**Files:**
- Modify: `crates/overseer-daemon/src/server.rs` (`Request::Kickoff`), `crates/overseer-gui/src/app.rs` only if the text it receives changes shape

**Interfaces:**
- Consumes: Task 4 `kickoff_text` with `KickoffFacts`; `LockService::view` for the slot's held targets; the slot's cwd as the worktree when it lies under `worktree_root`.
- Produces: `Request::Kickoff{slot}` answers the template filled with held targets, the worktree (when under `worktree_root`), the CURRENT_SESSION pointer and no rules.

- [ ] **Step 1: Write the failing test** `manual_kickoff_names_targets_and_current_session` (`tests/server.rs`): assign `DEMO-12` to S3, claim `repo:demo-api:main` for S3, `Kickoff{slot: 3}` text contains `DEMO-12`, `You hold these targets: repo:demo-api:main` and `Keep your section of`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon --test server -- manual_kickoff_`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** it and `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Put held targets and the CURRENT_SESSION pointer in the manual kickoff`.

### Task 6: MCP tools and `ctl` commands

**Files:**
- Modify: `crates/overseer/src/mcp.rs`, `crates/overseer/src/ctl.rs`, `crates/overseer-daemon/src/launch.rs` (`ALLOWED_TOOLS`)
- Test: `crates/overseer/tests/mcp.rs`, `crates/overseer/tests/cli.rs`

**Interfaces:**
- Consumes: Task 1 requests; Task 2 and 4 responses.
- Produces:
  - MCP tools (names exact): `get_board {}`, `get_work_items {}`, `propose_plan {summary: string, slots: array (req)}` (the slot object schema of Ruling 3, `slot` string or integer), `request_status {slot (req)}`, `report_progress {text (req)}`, `set_board_summary {text (req)}`. `call_tool` maps `Response::Board` to `ctl::format_board(&view, now)` and `Response::Work` to `ctl::format_work`. `ProposePlan`, `DecidePlan` join the slow calls (`BRAIN_CALL_WITHIN`).
  - `ALLOWED_TOOLS` gains the six as `mcp__overseer__<name>` (14 in all).
  - `ctl` subcommands: `board [--json]`, `plan [--json]` (lists the pending plan and the last five, with warnings and outcome), `plan approve <id>`, `plan reject <id> [reason]`, `plan propose <file|->` (JSON of Ruling 3), `summary <text|->`, `status-of <slot>`, `report <text|->` (needs `OVERSEER_SLOT`: else `report needs a slot; run it from a session`). All send `from: env_slot()`.
  - `pub fn format_board(view: &BoardView, now: i64) -> String` (Ruling 12), `pub fn format_plan(rec: &PlanRecord) -> String`.
- [ ] **Step 1: Write the failing tests**
  - `ctl.rs` unit: `board_text_lists_summary_slots_work_locks_and_plan` (asserts each Ruling 12 line shape on a built `BoardView`); `board_text_without_summary_says_none_yet`; `plan_text_shows_slots_warnings_and_outcome`.
  - `mcp.rs` unit: `tools_list_has_fourteen_tools`; `propose_plan_takes_slot_names_and_numbers` (`"S3"` and `3` both become slot 3); `report_progress_without_a_slot_is_refused` (the existing `NOT_IN_A_SLOT` text).
  - `tests/mcp.rs`: `s0_proposes_and_gets_the_board` (S0 proposes, `get_board` text contains `Plan 1 waiting for Jonathan since`); `worker_cannot_propose` (isError true with the Ruling 2 text).
  - `tests/cli.rs`: `plan_approve_inside_a_session_is_refused` (`OVERSEER_SLOT=S3 overseer ctl plan approve 1` exits 1 with `only Jonathan can approve or reject a plan`); `plan_approve_as_jonathan_runs_it`.
  - `launch.rs`: the existing `ALLOWED_TOOLS` test updated to the 14 names.
- [ ] **Step 2: Run** `cargo test -p overseer`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Add the coordinator MCP tools and ctl commands`.

### Task 7: Roles

**Files:**
- Modify: `crates/overseer-daemon/src/roles/coordinator.md`, `crates/overseer-daemon/src/roles/worker.md`, `crates/overseer-daemon/src/roles/kickoff.md`, `crates/overseer-daemon/src/launch.rs`
- Create: `crates/overseer-daemon/tests/fixtures/roles-phase5/{coordinator,worker,kickoff}.md` (copies of the current files)

**Interfaces:**
- Consumes: the Task 6 tool names.
- Produces: role text only, plus the phase 5 hashes in `PREVIOUS_COORDINATOR`, `PREVIOUS_WORKER` and `PREVIOUS_KICKOFF`.

- [ ] **Step 1: Write the failing tests** (`launch.rs`): `phase5_roles_are_previous_defaults` (each fixture's `text_hash` is in its list, so an unedited phase 5 file is upgraded in place); `coordinator_role_names_the_board_tools` (the default contains `get_board`, `get_work_items`, `propose_plan`, `request_status`, `set_board_summary`, `notify_session`, and no `ctl assign`, `ctl start` or `Never claim targets`); `worker_role_mentions_report_progress`; `coordinator_role_keeps_brain_rules` (still has `{brain_rules}`).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon launch::`. Expected: FAIL.
- [ ] **Step 3: Write the roles.** Coordinator: keep the journal step (step 4 of today's text) and the resumable-slot rule. Replace the ctl steps with: call `get_board` (and `get_work_items` for the full item list); propose with `propose_plan`, one entry per slot that should get work, with items, targets to pre-claim, a worktree when the work is in a repo, the kickoff and any collision rules; then stop and wait, since Jonathan approves, edits or rejects in Overseer and the result arrives as a message; never start, assign or claim by other means. Use `request_status` to ask a running worker how it is going. Call `set_board_summary` with two to four lines after every plan result, every status answer and whenever Jonathan asks what is going on. Worker: add one line: when Overseer passes on a status request, answer with `report_progress` in two or three lines; also call it after finishing a step. Kickoff: `Run the start-of-day round: call get_board, read today's journal, and propose a plan with propose_plan.`
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS (the RL fixtures for phases 3 and 4 still upgrade).
- [ ] **Step 5: Commit** `Rewrite the coordinator role around the board tools`.

### Task 8: Board summary, header, Progress and plan notification (GUI)

**Files:**
- Modify: `crates/overseer-gui/src/app.rs`, `crates/overseer-gui/src/board.rs`, `crates/overseer-gui/src/session.rs`, `crates/overseer-gui/src/notify.rs`

**Interfaces:**
- Consumes: `CoordView`, `Event::CoordChanged`, `Request::CoordStatus`.
- Produces: `OverseerApp.coord: CoordView` (`empty_coord()`), `pub fn summary_lines(view: &CoordView, clock: impl Fn(i64) -> String) -> (String, String)` (title `Updated HH:MM` or `No summary yet`, body text or `Ask S0 "what's going on?" to get one.`), `pub fn done_count(work: &WorkView) -> (usize, usize)` and the header gaining `done d/n`, `pub fn progress_rows(view: &CoordView, slot: SlotId, clock, max: usize) -> Vec<(String, Tone)>` (reports normal, states muted, newest first, `max` 10), `pub fn plan_note(prev: &CoordView, next: &CoordView) -> Option<Note>` (title `S0 proposed a plan`, body the plan summary's first line or `Plan <n> is waiting for you`; fires once per new pending id, not at first sync), a board line `Plan <n> waiting for you (p)` in the coordinator panel, `p` on the board opening the plan view (Task 9; until then it sets `status`).

- [ ] **Step 1: Write the failing tests**: `summary_lines_with_and_without_summary`; `done_count_counts_done_closed_resolved_tickets_only`; `header_text_has_done_count`; `progress_rows_newest_first_states_muted`; `plan_note_fires_once_per_new_plan` (none at first sync, one for plan 2, none again for the same view); `p_routes_on_the_board_only` (with `routed`).
- [ ] **Step 2: Run** `cargo test -p overseer-gui`. Expected: FAIL.
- [ ] **Step 3: Implement**: replace the `Coordinator summary appears here in phase 6` text (`board.rs` coordinator panel) with the summary; replace `placeholder("Progress", "phase 4")` in `session.rs` with `progress_rows`; request `CoordStatus` with `request_marked` in `OverseerApp::new`; handle `CoordChanged` and `Response::Coord` in `on_frame`; post `plan_note` through `notify::post` with a click that brings the board forward and opens the plan view.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Show the coordinator summary, progress and plan notice`.

### Task 9: Plan approval view (GUI)

**Files:**
- Create: `crates/overseer-gui/src/plan_view.rs`
- Modify: `crates/overseer-gui/src/app.rs`, `crates/overseer-gui/src/lib.rs`

**Interfaces:**
- Consumes: `PlanRecord`, `Request::DecidePlan`, `TextField` from `start_dialog.rs`, the modal pattern of `open_picker`.
- Produces: `pub struct PlanDraft` (the editable copy) with pure methods `remove_slot(slot)`, `remove_item(slot, key)`, `remove_target(slot, target)`, `clear_worktree(slot)`, `set_kickoff(slot, text)`, `plan(&self) -> Plan`, `edited(&self) -> bool`; `pub fn rows(rec: &PlanRecord, draft: &PlanDraft) -> Vec<PlanRow>` for rendering; entity `PlanView` emitting `PlanEvent::{Decided(String), Closed}`; `OverseerApp::open_plan(window, cx)`; `modal_open` covers it and `NAV` gains `&& !PlanView`.
- Layout: title `Plan <n> from S0` and the summary; per slot a block: `S3  <label>`, items as pills with `x`, targets as pills with `x`, `worktree <path>` with `x`, the kickoff in a `TextField`, the rules lines, a `Drop S3` button; the warnings in orange above the buttons; buttons `Approve` (Cmd+Enter), `Reject` (with an optional reason field) and `Close` (Esc, decides nothing). A daemon error after Approve shows in red inside the view and the view stays open; success closes it and puts the first outcome line in `status`.

- [ ] **Step 1: Write the failing tests** (pure, in `plan_view.rs`): `draft_starts_equal_and_unedited`; `removing_an_item_marks_edited_and_drops_it_from_the_plan`; `dropping_every_slot_disables_approve` (`can_approve()` false); `kickoff_edit_round_trips`; `rows_show_warnings_and_worktree`; key routing: `cmd_enter_and_escape_route_to_plan_view` and `cmd_digits_do_not_route_while_plan_view_open`.
- [ ] **Step 2: Run** `cargo test -p overseer-gui plan_view`. Expected: FAIL.
- [ ] **Step 3: Implement.** Approve sends `DecidePlan{id, from: None, decision: Approve{plan: draft.plan()}}`; Reject sends `Reject{reason}`. A plan that stops being pending while the view is open (a `CoordChanged` without it pending) closes the view with status `Plan <n> was <approved|rejected|replaced> elsewhere`.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Add the plan approval view`.

### Task 10: Acceptance list

**Files:**
- Create: `docs/acceptance/phase-6.md`

- [ ] **Step 1: Write the list** in the style of `docs/acceptance/phase-5.md`: an isolated rig (`OVERSEER_HOME` under `/tmp`, a throwaway repo `demo-api`, `worktree_root` under the rig, stub `bd` and Jira), then rows with ID, check, steps, pass and `pending` result, covering: S0's tools listed and pre-approved in a real session (P1); a real S0 start-of-day round ending in `propose_plan` and stopping (P2); the plan notification and the `p` key (P3); approve unedited and check every outcome line, worktree, claim, start, kickoff and S0's relay (P4); approve with edits (P5); reject with a reason (P6); re-validation after a hand assignment (P7); double approval (P8); a worker refused for propose, summary, status and `ctl plan approve` (P9); request status and the answer to S0 (P10); summary on the board and refresh on "what's going on?" (P11); Progress timeline (P12); done count in the header (P13); manual kickoff text (P14); role upgrade from phase 5 files (P15); restart with a pending plan (P16); the full test suite three times (X1); and Z1 (real vault, `CURRENT_SESSION.md`, `~/Repos/.worktrees` and the installed app's state untouched). Known limitations: Ruling 2's `OVERSEER_SLOT` guard rail; a running slot is told its worktree, not moved; sprint name and day wait for phase 7.
- [ ] **Step 2: Check** the file has no em dashes (`grep -c $'\u2014'` prints 0) and every row ends `| pending |`.
- [ ] **Step 3: Commit** `Add the phase 6 acceptance list`.
