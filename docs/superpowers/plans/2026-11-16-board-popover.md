# Session pop-over and full kanban Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Open sessions as a pop-over over the board, and show every sprint ticket in six scrolling columns: Todo, Blocked, Next, In Progress, To Be Tested, Done.

**Architecture:** `Stage` gets the six new variants end to end (proto, daemon config and stage mapping, ledger, GUI). The kanban drops its card cap and scrolls per column. `Screen::Session(id)` keeps its meaning for state but now renders the board with the session drawn in a pop-over layer beside it.

**Tech Stack:** Rust workspace, GPUI (`crates/overseer-gui`), tokio daemon (`crates/overseer-daemon`), serde wire types (`crates/overseer-proto`), SQLite ledger (rusqlite).

**Spec:** `docs/superpowers/specs/2026-11-16-board-popover-design.md`

## Global Constraints

- Columns, in order: Todo, Blocked, Next, In Progress, To Be Tested, Done. Labels: `TODO`, `BLOCKED`, `NEXT`, `IN PROGRESS`, `TO BE TESTED`, `DONE`.
- Defaults: `stage_todo` Backlog, Prioritised Backlog, To Do; `stage_blocked` Blocked, Waiting, On Hold; `stage_next` Next, Selected for Development; `stage_in_progress` In Progress, In Review, Code Review, Review; `stage_to_be_tested` To Be Tested, Testing, In Testing, QA; `stage_done` Done, Closed, Resolved.
- An unmapped status goes to In Progress. A ticket with an open PR stays in its Jira column (no move to a review column).
- Old keys still load: `stage_waiting` as `stage_blocked`, `stage_testing` as `stage_to_be_tested`, each only when the new key is absent; `stage_in_review` names are appended to the In Progress list in effect.
- Ticket stage ages survive the upgrade: `since` and `moved` are unchanged for InReview to InProgress, Waiting to Blocked, Testing to ToBeTested.
- Pop-over closes on the close button, a click on the scrim, or Cmd+Esc; plain Esc keeps today's rule (`Session && !Terminal && !PanelEntry`). A click inside the panel never closes it.
- Public repo: invented names only (DEMO-n, demo-*). No em dashes anywhere. No emojis in commits.
- Commits: a short plain sentence, blank line, `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Run `git log -5` first. Never stash, reset, switch branches or push.
- Gates per task: `cargo test --workspace` (the overseer hook test and the server PTY burst test are flaky under load; rerun once if only they fail), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`.

## Review Focus

1. Board keys while the pop-over is open: arrows, Enter, r, a, p and l typed into the pop-over's terminal must reach the terminal, not the board. Pinned by a `routed` test in Task 3 and by drawing the pop-over as a sibling of the board.
2. Upgrading with existing ledger rows named `Waiting`, `InReview`, `Testing`: the first poll after upgrade must not reset ages. Pinned by a ledger test in Task 1.
3. A config.toml that sets only `stage_waiting` (old key): its names must land in Blocked. Pinned by a config test in Task 1.
4. A column with 30 tickets: all 30 render and the board's height does not grow. Pinned by a `columns` test in Task 2; height is a manual check.
5. Elapsed times on the board behind an open pop-over keep ticking. Pinned by a `needs_tick` test in Task 3.

---

### Task 1: Six new columns end to end

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (Stage, labels, tests)
- Modify: `crates/overseer-daemon/src/config.rs` (stage fields, defaults, legacy keys in `load`, tests)
- Modify: `crates/overseer-daemon/src/settings.rs:193-210` (Settings rows)
- Modify: `crates/overseer-daemon/src/work/stages.rs`
- Modify: `crates/overseer-daemon/src/work/store.rs:~476` (remove PR bump) and its test at ~1982
- Modify: `crates/overseer-daemon/src/ledger.rs` (V8 migration)
- Modify: `crates/overseer-gui/src/sprint_view.rs` (`stage_icon`, `stage_color`, tests using old variants)
- Modify: `README.md:~180` (stage table)

**Interfaces:**
- Produces: `pub enum Stage { ToDo, Blocked, Next, InProgress, ToBeTested, Done }` with `Stage::ALL` in that order and `label()` as in Global Constraints. Config fields `stage_todo, stage_blocked, stage_next, stage_in_progress, stage_to_be_tested, stage_done: Vec<String>`.

- [ ] **Step 1: Write failing tests**
  - `msg.rs`: `Stage::ALL.map(Stage::label) == ["TODO","BLOCKED","NEXT","IN PROGRESS","TO BE TESTED","DONE"]`; `serde_json::from_str::<Stage>("\"Waiting\"") == Ok(Stage::Blocked)`, `"InReview"` gives `InProgress`, `"Testing"` gives `ToBeTested` (serde aliases, so an older peer still parses).
  - `stages.rs` `statuses_map_to_stages`: Prioritised Backlog to ToDo, `Next` to Next, `Selected for Development` to Next, ` Code Review ` to InProgress, `To Be Tested` to ToBeTested, `Blocked` to Blocked, `RESOLVED` to Done, `Ready for Release` and `""` to InProgress.
  - `config.rs`: defaults equal the Global Constraints lists; `legacy_stage_keys_load`: a file with only `stage_waiting = ["Parked"]` and `stage_in_review = ["Peer Check"]` gives `stage_blocked == ["Parked"]` and `stage_in_progress` == the default list plus `"Peer Check"`; a file with both `stage_waiting = ["A"]` and `stage_blocked = ["B"]` gives `["B"]`.
  - `ledger.rs` `v8_renames_old_stage_names`: open a ledger at V7, insert rows `("DEMO-1","Waiting",100,1)`, `("DEMO-2","InReview",200,1)`, `("DEMO-3","Testing",300,0)`, reopen; `stages_on` gives Blocked/100/true, InProgress/200/true, ToBeTested/300/false, and `record_stages_on(&[("DEMO-1", Stage::Blocked)], 999)` leaves DEMO-1's since at 100.
  - `store.rs`: the test at ~1982 now expects a ToDo ticket with a linked PR to stay `Some(Stage::ToDo)`.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test --workspace stage` Expected: compile errors or failures on the new names.

- [ ] **Step 3: Implement**
  - Stage variants with `#[serde(alias = "Waiting")]` on Blocked, `#[serde(alias = "InReview")]` on InProgress, `#[serde(alias = "Testing")]` on ToBeTested.
  - Config: rename fields; legacy keys read in `Config::load` by also parsing the text as `toml::Table` and applying the Global Constraints rule (new key present in the table wins). Keep `toml::from_str::<Config>` tolerant of the old keys (they must not error).
  - Settings rows: the six new keys in column order; drop the old three.
  - Ledger: `VERSION = 8`, `V8_SCHEMA` = three `UPDATE ticket_stages SET stage = 'X' WHERE stage = 'Y'` statements, added to the migration list.
  - store.rs: delete the `has_pr` bump; the stage is Jira's stage.
  - GUI: icons and colours: Blocked `‖` `colors::ORANGE`; Next `→` `colors::ACCENT`; To Be Tested keeps Testing's icon and purple; In Progress keeps its own; remove InReview's.
  - README: replace the stage table with the six keys and defaults, and one line on the old keys still loading.

- [ ] **Step 4: Run gates** (Global Constraints). Expected: all pass.

- [ ] **Step 5: Commit** `Use the Todo, Blocked, Next, In Progress, To Be Tested, Done columns`

### Task 2: Every ticket in scrolling columns

**Files:**
- Modify: `crates/overseer-gui/src/sprint_view.rs` (`columns`, `Column`, `render_kanban` ~719, tests ~922)

**Interfaces:**
- Consumes: `Stage::ALL` from Task 1.
- Produces: `pub struct Column { stage: Stage, total: usize, cards: Vec<TicketCard> }` (no `more`); `MAX_CARDS` removed.

- [ ] **Step 1: Write failing tests**
  - `columns_show_every_ticket`: 30 ToDo tickets give a ToDo column with `cards.len() == 30` and `total == 30`.
  - `done_column_lists_tickets_latest_first`: Done tickets DEMO-1 (since 100), DEMO-2 (since 300), DEMO-3 (no since) give Done cards `["DEMO-2","DEMO-1","DEMO-3"]`.
  - Existing `columns_sort_and_overflow` updated: same sort order, no truncation.

- [ ] **Step 2: Run and see them fail.** `cargo test -p overseer-gui columns` Expected: FAIL.

- [ ] **Step 3: Implement**
  - `columns`: no truncation; Done gets cards sorted by `stage_since` descending, unknown last; other columns keep today's sort.
  - `render_kanban`: the kanban keeps a fixed height (size so the six session cards and the kanban fit 1440x900); each column is a flex column with its header (icon, label, `total`) pinned and its card list in an `overflow_y_scroll` child with a unique element id per stage. Remove the `+N more` row. Done stays the narrowest column and renders its cards compactly (key and title, one line).

- [ ] **Step 4: Run gates.** Then a manual check: run the GUI against a daemon with many tickets (or a test WorkView) and confirm columns scroll and the session cards stay on screen.

- [ ] **Step 5: Commit** `Show every sprint ticket in scrolling kanban columns`

### Task 3: Session pop-over over the board

**Files:**
- Modify: `crates/overseer-gui/src/app.rs` (`needs_tick`, `Render for OverseerApp` ~1552, tests ~1650 and ~1784)
- Modify: `crates/overseer-gui/src/session.rs` (`render_session` ~1153: fit a panel, add close button)

**Interfaces:**
- Consumes: `go_board`, `go_session`, `render_board`, `render_session` (all existing).
- Produces: `Screen::Session(id)` renders board plus pop-over. No new public types.

- [ ] **Step 1: Write failing tests** in `app.rs` tests
  - `needs_tick` now: `needs_tick(Screen::Session(2), &slots_with(&[3]))` is true (board visible behind); `needs_tick(Screen::Session(2), &slots_with(&[]))` is false. Update the existing assertions at ~1784 to match.
  - `board_keys_stay_out_of_the_popover`: with the pop-over stack `["Overseer","Session","Terminal"]`, `routed` of `left`, `enter`, `r`, `a`, `p`, `l` is `None`; `cmd-escape` is `Some("overseer::GoBoard")`; `escape` is `None`.

- [ ] **Step 2: Run and see `needs_tick` fail.** `cargo test -p overseer-gui needs_tick` Expected: FAIL.

- [ ] **Step 3: Implement**
  - `needs_tick`: `Screen::Session(_)` behaves like `Screen::Board`.
  - `render`: for `Screen::Session(id)`, render the board, then as a sibling (not a child of the board element) an absolute full-window layer: a scrim (`colors::BG` at about 70% opacity) whose mouse-down calls `go_board`, and centred on it a panel about 90% of the window's width and height holding `render_session(id, ...)`. The panel stops mouse-down propagation so clicks inside never reach the scrim. Dialogs, picker, plan and settings stay after it so they paint on top.
  - `render_session`: fill its parent instead of the window; add to its header the slot name, ticket key and title when present, and a close button (`x`, element id `popover-close`) that calls `go_board`. Remove any now-redundant "back to board" control.
  - Focus: unchanged; `go_session` focuses the terminal and `go_board` focuses the board.

- [ ] **Step 4: Run gates.** Then a manual check in the GUI: open a running slot by click and by Cmd+3; type arrows and `l` in the terminal and confirm they reach the session; click inside the panel (stays open), on the scrim (closes), close button and Cmd+Esc (close); open Locks from the pop-over and press Esc (pop-over returns); the terminal reflows to the panel size and back.

- [ ] **Step 5: Commit** `Open sessions in a pop-over over the board`
