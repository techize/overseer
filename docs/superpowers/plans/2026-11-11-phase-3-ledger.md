# Phase 3: Ledger Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Overseer knows the work. It reads Jira tickets and beads, groups beads under their tickets, lets Jonathan assign items to slots through a picker, enforces single ownership (refuse, warn, move), records ownership on the bead, shows owned items on every card and in the session panel, and starts a slot with a kickoff built from its items.

**Architecture:** A new `work` module in the daemon holds the work-item cache. Two sources fill it: `bd list --json` in the beads folder, and a configurable Jira command (default `acli jira workitem search --jql ... --json`). They are polled on timers and on request. Ownership lives in the SQLite ledger (schema version 2: `assignments`, `history`, `notices`), so it survives restarts. A pure `ownership` module decides each assignment. The protocol gains work-item requests and a `WorkItemsChanged` event. The GUI adds pills on cards, an Owns panel, and a picker modal. Settings come from a new `~/.config/overseer/config.toml`.

**Tech Stack:** Rust, tokio (process, time), rusqlite (bundled), `toml` 0.8, `regex` 1, GPUI 0.2.2.

**Spec:** `docs/superpowers/specs/2026-11-09-overseer-design.md`, sections 7.4, 7.7 (manual kickoff template), 7.9 (cards, Owns panel, picker), 7.10 (ledger, config) and 9 item 3.

## Global Constraints

- Overseer reads Jira only. It never writes to Jira (spec 3). The only write-back is to beads.
- It stores no Jira credentials. Jira goes through the configured command (spec 7.4), and a missing `acli` is a normal state: the Jira source reports "not found", the beads still work, and nothing errors.
- Bead ownership is recorded as the label `overseer:S<n>` through `bd update <id> --add-label` and `--remove-label`, never through the assignee field, which stays Jonathan's.
- Ownership survives daemon restarts and slot exits. Only an explicit release, or a move, ends it.
- The repo is public. Test fixtures are invented or sanitised. No real ticket text, account ids, hostnames or bead content from Jonathan's machines goes into the repo.
- Work-item commands (`bd`, the Jira command) run off the socket's request path with a 30 s timeout. A slow or hanging source must never stall slot requests or hooks.
- No em dashes in any human-readable text the app writes. Commit messages human style, no AI language.
- The protocol stays backward compatible within the phase: new `SlotInfo` fields use `#[serde(default)]`.
- The ticket pattern defaults to `PLAT-\d+` and is configurable.

## Rulings (decisions the spec leaves open)

- **Telling a session.** The spec says a moved or overlapped session is "told". Typing into a terminal safely needs the delivery rules from phase 4. In this phase a notice is stored in the ledger (`notices`) and shown in that slot's panel and in `ctl items`. Phase 4 will deliver the notices. Cost if wrong: a session learns late, but Jonathan sees it on the board.
- **Kickoff to a running slot.** Assigning to an empty or exited slot offers "Assign and start", which starts the slot with the kickoff as its first prompt. For a running slot the kickoff is not typed automatically. The panel offers "Send kickoff", enabled only while the slot is Idle.
- **Worktrees** move to phase 4 with targets. The kickoff names the ticket folder, not a worktree.
- **Beads under an assigned ticket** are not labelled. Only a directly assigned bead gets `overseer:S<n>`.
- **A bead naming a ticket that is not in the Jira cache** (acli missing, or the ticket is outside the JQL) still groups under a ticket node made from the bead, titled with the key alone.
- **Closed items.** The beads source lists the default (not closed) set. An owned item that leaves the cache keeps its assignment, and shows as "not in the current list" until released.

## Review Focus

1. **`acli` missing or failing** (not installed, auth expired, junk output, a warning line before the JSON). The beads still load, the source status says why Jira is empty, and nothing panics.
2. **`bd` slow or hanging.** Slot requests, attach and hooks keep responding while a refresh is in flight. The 30 s timeout ends it.
3. **Assigning the same item from two clients at once** (GUI and `ctl`). Exactly one wins and the other is refused. The decision and the ledger write happen under one lock.
4. **A restart with assignments in place.** Cards show the same pills after a daemon restart, before the first refresh has finished.
5. **A ticket key inside other text** (`DEMO-123:` in a title, `jira-DEMO-123` in external_ref, two keys in one title). The first match groups the bead, and `DEMO-1234` does not match `DEMO-123`.

Each line has its test in the owning task below.

---

### Task 1: Config file

**Files:**
- Create: `crates/overseer-daemon/src/config.rs`
- Modify: `crates/overseer-daemon/Cargo.toml` (`toml = "0.8"`), `src/lib.rs`, `src/server.rs` (`DaemonConfig.settings: Config`), `crates/overseer/src/main.rs` (load at daemon start), `crates/overseer-proto/src/paths.rs` (`config_path() -> PathBuf`, `$OVERSEER_CONFIG` else `~/.config/overseer/config.toml`)

**Interfaces:**
- Produces: `Config { beads_dir: PathBuf, jira_command: Vec<String>, jql: String, jira_poll_secs: u64, beads_poll_secs: u64, ticket_pattern: String, brain_dir: PathBuf }` with `Default`. Defaults: `beads_dir` and `brain_dir` = `~/Repos/2ndBrain`, `jira_command` = `["acli","jira","workitem","search","--jql","{jql}","--json","--limit","200"]` (`{jql}` is replaced), `jql` = `sprint in openSprints() AND assignee = currentUser() ORDER BY priority DESC`, `jira_poll_secs` 300, `beads_poll_secs` 30, `ticket_pattern` `PLAT-\d+`.
- `Config::load(path: &Path, home: &Path) -> (Config, Option<String>)`: a missing file gives the defaults and `None`, an unreadable or invalid file gives the defaults and the error text (logged by the daemon). `~/` in paths is expanded against `home`. Keys are all optional (`#[serde(default)]`).

- [ ] **Step 1: Tests** `missing_file_is_defaults`, `partial_file_overrides_only_its_keys` (only `jql` set), `bad_toml_falls_back_with_an_error`, `tilde_paths_expand`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon config::` and see them fail.
- [ ] **Step 3: Implement**, load in `overseer daemon`, and pass `Config::default()` in every test `DaemonConfig`.
- [ ] **Step 4: Run** the tests and `cargo test --workspace`.
- [ ] **Step 5: Commit** "Read settings from config.toml".

### Task 2: Work-item protocol

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs`, `src/lib.rs` (re-exports)

**Interfaces (all `Serialize, Deserialize, Debug, Clone, PartialEq`):**
- `enum ItemKind { Ticket, Bead }`
- `struct WorkItem { key: String, kind: ItemKind, title: String, status: String, priority: Option<String>, parent: Option<String>, owner: Option<SlotId>, in_list: bool, folder: Option<PathBuf> }`. `parent` is the ticket key for a grouped bead. `in_list` is false for an owned item that left the cache. `folder` is the 2ndBrain ticket folder when found.
- `struct SourceStatus { name: String, ok: bool, count: usize, at: Option<i64>, message: Option<String> }`
- `struct Notice { slot: SlotId, at: i64, text: String }`
- `struct WorkView { items: Vec<WorkItem>, sources: Vec<SourceStatus>, notices: Vec<Notice> }`
- `Request::ListWorkItems`, `Request::RefreshWorkItems`, `Request::Assign { key: String, slot: SlotId, #[serde(default)] confirm: bool, #[serde(default)] move_owner: bool }`, `Request::Release { key: String }`, `Request::Kickoff { slot: SlotId }`, `Request::ClearNotices { slot: SlotId }`
- `Response::Work(WorkView)`, `Response::NeedsConfirm { message: String }`, `Response::Text { text: String }`
- `Event::WorkChanged(WorkView)`

- [ ] **Step 1: Tests** round-trip each new request, response and event through JSON. `assign_defaults_flags` checks `{"type":"Assign","key":"DEMO-1","slot":2}` parses with both flags false.
- [ ] **Step 2: Run** `cargo test -p overseer-proto` and see them fail.
- [ ] **Step 3: Implement.** Add match arms that answer `Response::Error { message: "not supported yet" }` in the daemon, and make the GUI ignore `Event::WorkChanged` for now, so the workspace builds.
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Add work items to the protocol".

### Task 3: Sources and grouping

**Files:**
- Create: `crates/overseer-daemon/src/work/mod.rs`, `work/sources.rs`, `work/group.rs`, `crates/overseer-daemon/tests/fixtures/bd-list.json`, `tests/fixtures/acli-search.json` (invented content only)
- Modify: `Cargo.toml` (`regex = "1"`)

**Interfaces:**
- `struct Bead { id: String, title: String, status: String, priority: Option<String>, external_ref: Option<String>, labels: Vec<String> }`
- `struct Ticket { key: String, title: String, status: String, priority: Option<String> }`
- `parse_beads(json: &str) -> Result<Vec<Bead>, String>`: `bd list --json` is an array. `priority` is a number in bd (render `P<n>`).
- `parse_jira(out: &str) -> Result<Vec<Ticket>, String>`: tolerant. Skips any text before the first `[` or `{`. Accepts an array of issues, or an object holding `issues`. Reads `key`, `fields.summary`, `fields.status.name`, `fields.priority.name`, each optional except `key`.
- `ticket_of(bead: &Bead, pattern: &Regex) -> Option<String>`: first match in `title`, else in `external_ref`. The pattern is wrapped in word boundaries (`\b(?:PAT)\b`).
- `group(tickets: &[Ticket], beads: &[Bead], pattern: &Regex) -> Vec<WorkItem>`: tickets first in Jira order, each followed by its beads, then the ticket nodes made from beads alone (title = key, status "unknown"), then standalone beads. `owner`, `folder` empty here; `in_list` true.
- `async fn run_source(cmd: &[String], cwd: &Path, timeout: Duration, env: &[(OsString, OsString)]) -> Result<String, String>`: runs the command with tokio::process, returns stdout. Not found gives `"<program> not found"`, a timeout gives `"timed out after 30 s"`, a non-zero exit gives the exit code and the first stderr line.

- [ ] **Step 1: Tests:** `parses_bd_list_fixture`, `jira_with_a_leading_warning_line`, `jira_object_with_issues`, `jira_junk_is_an_error`, `ticket_in_title_or_external_ref`, `pattern_does_not_match_a_longer_key` (`DEMO-1234` vs `DEMO-123`), `first_of_two_keys_wins`, `bead_without_jira_ticket_gets_a_ticket_node`, `standalone_beads_last`, `run_source_missing_program`, `run_source_times_out` (`sleep 5` with a 200 ms timeout).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon work::` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests.
- [ ] **Step 5: Commit** "Read work items from beads and Jira".

### Task 4: Ledger schema 2 (assignments, history, notices)

**Files:**
- Modify: `crates/overseer-daemon/src/ledger.rs`

**Interfaces:**
- Migration: `user_version` 1 becomes 2, adding `assignments(key TEXT PRIMARY KEY, slot INTEGER NOT NULL, at INTEGER NOT NULL)`, `history(id INTEGER PRIMARY KEY, at INTEGER, key TEXT, slot INTEGER, event TEXT, detail TEXT)` and `notices(id INTEGER PRIMARY KEY, slot INTEGER, at INTEGER, text TEXT)`. A fresh file is created at version 2. `in_memory` gets the same schema.
- `assignments(&self) -> Result<Vec<(String, SlotId)>>`, `assign(&self, key: &str, slot: SlotId, at: i64) -> Result<()>` (replaces), `release(&self, key: &str) -> Result<Option<SlotId>>`, `log(&self, at: i64, key: &str, slot: Option<SlotId>, event: &str, detail: &str) -> Result<()>`, `add_notice(&self, slot: SlotId, at: i64, text: &str) -> Result<()>`, `notices(&self) -> Result<Vec<Notice>>`, `clear_notices(&self, slot: SlotId) -> Result<()>`.
- Add `pub fn transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>` so an assignment's checks and writes commit together.

- [ ] **Step 1: Tests:** `version_1_file_migrates_to_2_keeping_sessions` (create a v1 file with the old schema and a session row, open, the row is still live and the new tables exist), `assign_release_round_trip`, `assignment_survives_reopen`, `notices_per_slot_and_clear`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon ledger::` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests.
- [ ] **Step 5: Commit** "Keep assignments, history and notices in the ledger".

### Task 5: Ownership rules

**Files:**
- Create: `crates/overseer-daemon/src/work/ownership.rs`

**Interfaces:**
- `enum Decision { Assign, AlreadyYours, Refuse { owner: SlotId }, Warn { message: String, tell: Vec<SlotId> }, Move { from: SlotId } }`
- `fn decide(items: &[WorkItem], owners: &HashMap<String, SlotId>, key: &str, slot: SlotId, confirm: bool, move_owner: bool) -> Result<Decision, String>`. Rules from spec 7.4:
  - Unknown key (not in `items` and not owned): `Err("no work item <key>")`.
  - A ticket, or a standalone bead, owned by another slot: `Refuse`. With `move_owner`: `Move { from }`.
  - A bead whose parent ticket is owned by another slot: without `confirm`, `Warn` with the text `This bead is under DEMO-12, owned by S3. Assign to S4 anyway?` and `tell: [3]`. With `confirm`, the same `Warn` is returned as accepted (the caller assigns and stores the notice).
  - A ticket some of whose beads are owned by other slots: the same warn and confirm flow, naming those beads and slots.
  - Owned by this slot already: `AlreadyYours`.
- `fn promotions(before: &[WorkItem], after: &[WorkItem], owners: &HashMap<String, SlotId>) -> Vec<(String, SlotId, SlotId)>`: a bead that was standalone in `before`, owned by A, and is under a ticket owned by B in `after` (A different from B) yields `(bead, A, B)`.

- [ ] **Step 1: Tests:** one per rule above, using the warn message text verbatim, plus `promotion_detected_once` (the same lists again give nothing new because `before` already has the parent) and `no_promotion_when_owners_match`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon ownership` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests.
- [ ] **Step 5: Commit** "Decide assignments by the ownership rules".

### Task 6: Work store in the daemon

**Files:**
- Create: `crates/overseer-daemon/src/work/store.rs`
- Modify: `src/server.rs` (requests, poll tasks, event fan-out), `tests/server.rs`

**Interfaces:**
- `WorkStore::new(cfg: &Config, ledger: Arc<Ledger>, events: broadcast::Sender<Event>, env: BaseEnv) -> Arc<WorkStore>`.
- `fn view(&self) -> WorkView`: the cached items with owners from the ledger, owned items missing from the cache added with `in_list: false`, and `folder` filled from a listing of `<brain_dir>/Sprints/*/` (a folder named `<KEY>` or starting with `<KEY>-`, cached per refresh).
- `async fn refresh(&self, which: Source)`: runs the source with `run_source`, regroups, detects promotions (each one logs history and adds a notice to both slots), stores the status, and sends `Event::WorkChanged`. Refreshes of one source never overlap; a request during a refresh waits for it.
- `fn assign(&self, key, slot, confirm, move_owner, now) -> Response`: under the ledger transaction, `decide`, then write the assignment, history and notices. `Warn` without `confirm` returns `NeedsConfirm`. `Refuse` returns `Error` naming the owner (`DEMO-1 is owned by S3`). After commit, for a bead, `bd update <id> --add-label overseer:S<n>` (plus `--remove-label overseer:S<old>` on a move) runs in a spawned task, and a failure is logged and stored as a notice for that slot (`could not label <id> in beads: <reason>`).
- `fn release(&self, key, now) -> Response`: removes the assignment, logs it, removes the bead label in the background.
- Poll tasks: beads every `beads_poll_secs`, Jira every `jira_poll_secs`, both once at start.
- Server: `ListWorkItems` answers `Work(view)`, `RefreshWorkItems` refreshes both and then answers `Work`, `Assign`, `Release`, `ClearNotices` as above.

- [ ] **Step 1: Tests** in `tests/server.rs` with stub `bd` and Jira scripts (written like `stub_claude`, printing fixture JSON and logging their argv): `work_items_load_from_stub_sources`, `assign_then_second_slot_is_refused`, `move_releases_old_owner_and_adds_a_notice`, `warn_needs_confirm_then_assigns`, `bead_assign_labels_through_bd` (argv logged `update <id> --add-label overseer:S2`), `assignments_survive_restart_before_any_refresh` (sources stubbed to `sleep 10`), `missing_jira_program_reports_status_and_beads_still_load`, `hanging_source_does_not_block_list_slots` (a `sleep 10` bd stub, `ListSlots` answers within 1 s), `concurrent_assigns_one_wins` (two clients assign the same ticket to S1 and S2 at once, exactly one `Ok`).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon --test server` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Serve work items and assignments from the daemon".

### Task 7: Kickoff template

**Files:**
- Create: `crates/overseer-daemon/src/roles/assignment.md`
- Modify: `crates/overseer-daemon/src/launch.rs` (template written to `<state>/roles/assignment.md` like the others), `src/work/store.rs`, `src/server.rs`

**Interfaces:**
- `fn kickoff_text(template: &str, slot: SlotId, items: &[WorkItem], brain_dir: &Path) -> Option<String>`: `None` when the slot owns nothing. Fills `{slot}`, `{items}` (one line per item: `- DEMO-123 (In Progress): title`, a bead under it indented), `{folders}` (each item's `folder`, or `none found`) and `{brain}`.
- Default template text (exact):

```
You are {slot}. Overseer has assigned you this work:

{items}

Ticket folders in 2ndBrain: {folders}

Work only on these items. Before starting, read the ticket folder and run `bd show <id>` for each bead. If something here is already done or is owned by another session, say so and stop. Report back here when you finish a step.
```

- `Request::Kickoff { slot }` answers `Text { text }`, or `Error` "S3 owns no work items".

- [ ] **Step 1: Tests:** `kickoff_lists_items_and_folders`, `kickoff_none_without_items`, `bead_under_ticket_is_indented`, and a server test `kickoff_request_returns_text`.
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Build a kickoff from a slot's work items".

### Task 8: `ctl` commands and the roles

**Files:**
- Modify: `crates/overseer/src/ctl.rs`, `crates/overseer-daemon/src/roles/coordinator.md`, `roles/worker.md`, `crates/overseer/tests/cli.rs`

**Interfaces:**
- `overseer ctl items [--json]`: one line per item, beads indented under their ticket: `DEMO-123  In Progress  S3  title` (owner `-` when free). Source status lines first (`jira  ok  12 items  14:05`, or `jira  acli not found`). Notices last. `--json` prints the `WorkView`.
- `overseer ctl assign <slot> <key> [--yes] [--move]`: prints the confirm question and exits 2 on `NeedsConfirm`, exits 1 on `Error`.
- `overseer ctl release <key>`, `overseer ctl refresh`, `overseer ctl kickoff <slot>` (prints the text).
- Coordinator role: step 4's table gains a "work items" column, and after Jonathan approves a row, S0 runs `ctl assign <slot> <key>` for each item before `ctl start`. It never passes `--yes` or `--move` unless Jonathan said so for that item. `ctl items` replaces its own `bd ready` and Jira reads for the board picture (it may still run `bd ready --json` for readiness).
- Worker role: one line saying its owned items are listed by `overseer ctl items` and it must not take items owned by another slot.

- [ ] **Step 1: Tests** in `cli.rs`: `assign_needs_confirm_exits_2`, `items_prints_tickets_then_indented_beads` (against a daemon with stub sources).
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`.
- [ ] **Step 5: Commit** "Add item, assign, release and kickoff commands to ctl".

### Task 9: Board pills, header and the Owns panel

**Files:**
- Modify: `crates/overseer-gui/src/app.rs` (hold `WorkView`, fetch on connect, apply `Event::WorkChanged`), `board.rs`, `session.rs`

**Interfaces:**
- `fn owned_by(view: &WorkView, slot: SlotId) -> Vec<&WorkItem>` (tickets first, then beads, in view order).
- `fn pills(items: &[&WorkItem]) -> Vec<String>`: the key of each owned ticket and standalone bead. A bead under an owned ticket is not repeated. More than three become the first three plus `+N`.
- `CardModel` gains `pills: Vec<String>`. Pills render as small rounded tags under the title.
- `fn source_line(view: &WorkView, clock) -> String`: `jira 12 · beads 133 · 14:05`, or `jira: acli not found · beads 133 · 14:05`.
- The board header shows `source_line` on the right. An empty card shows `+ assign` beside `+ start`.
- The Owns panel lists `owned_by` with each item's status and a Release button, items with `in_list: false` marked "not in the current list". It shows notices for the slot with a Clear button. A `Send kickoff` button (enabled only when the slot's activity is Idle) requests `Kickoff` and types the text with `send_input` followed by `\r`. The Holds placeholder now says "phase 4".

- [ ] **Step 1: Tests** (pure): `owned_by_orders_tickets_first`, `pills_skip_beads_under_owned_tickets`, `pills_collapse_after_three`, `source_line_shows_missing_jira`, `card_model_has_pills`.
- [ ] **Step 2: Run** `cargo test -p overseer-gui` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests and launch the app once against a temp daemon with stub sources to look at the board (screenshot in the report).
- [ ] **Step 5: Commit** "Show owned work on cards and in the session panel".

### Task 10: The picker

**Files:**
- Create: `crates/overseer-gui/src/picker.rs`
- Modify: `app.rs` (open with `a` on the board for the selected card, from `+ assign`, and from an "Assign…" panel button; Esc closes), `start_dialog.rs` (accept a prefilled label and prompt, showing "Starts with the kickoff for <keys>" when a prompt is set)

**Interfaces:**
- `struct PickerRow { key: String, depth: u8, title: String, status: String, priority: Option<String>, owner: Option<SlotId>, folder: Option<PathBuf> }`
- `fn rows(view: &WorkView, filter: &str) -> Vec<PickerRow>`: the view in its grouped order. The filter matches key or title, case-insensitive; a matching bead keeps its parent ticket row, and a matching ticket keeps all its beads.
- Keys: type to filter (`TextField` reused), Up/Down to move, Enter to assign to the slot, Esc to close.
- Results: `Ok` closes the picker. `NeedsConfirm` shows the message with "Assign anyway" (Enter again sends `confirm: true`). `Error` naming an owner shows the message with "Move to S<n>" (sends `move_owner: true`). Other errors show inline.
- An empty or exited slot: after `Ok`, the app asks the daemon for `Kickoff` and opens the start dialog with the label set to the first item's key and the kickoff as the prompt ("Assign and start").

- [ ] **Step 1: Tests** (pure): `rows_group_and_indent`, `filter_keeps_parent_of_matching_bead`, `filter_keeps_children_of_matching_ticket`, `filter_is_case_insensitive`, `picker_keys` (Up/Down clamp, Enter, Esc).
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`, then drive the picker once in the real app against stub sources: assign, refuse, warn and move.
- [ ] **Step 5: Commit** "Add the work-item picker".

### Task 11: Acceptance list

**Files:**
- Create: `docs/acceptance/phase-3.md` (the same layout as `phase-2.md`: Setup, then tables of ID, Check, Steps, Pass, Result, then Findings, Known limitations, Sign-off)

Cover: config loading; Jira missing on the home Mac; beads loading from `~/Repos/2ndBrain`; grouping; picker assign, refuse, warn, move; bead label written and removed (checked with `bd show`); restart keeps ownership; pills and the Owns panel; Assign and start with the kickoff; Send kickoff only when idle; `ctl` commands; S0 using `ctl assign`. Run every check that does not need Jonathan through a temp daemon (as in phase 2) and record the results. Leave the GUI and real-Jira checks for Jonathan.

- [ ] **Step 1: Write** the list, run the scriptable checks, record the results.
- [ ] **Step 2: Commit** "Add the phase 3 acceptance list".
