# Board actions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Your turn badge, start work from a ticket card, link PR rows to their session, and show a hover card on ticket cards.

**Architecture:** GUI only (`crates/overseer-gui`). Each click decision is a pure function with unit tests. The handlers in `app.rs` and `sprint_view.rs` call these functions and reuse the existing paths: `go_session`, `assign_and_start` and `open_external`.

**Tech Stack:** Rust, GPUI.

**Spec:** `docs/superpowers/specs/2026-11-17-board-actions-design.md`

## Global Constraints

- No daemon or proto changes.
- Your turn means a Running slot with `activity.kind == Idle`. Its label is `Your turn <elapsed>`, its colour is `colors::ACCENT`, and a lock wait still overrides it.
- Header text: `N need you · M your turn`. A part with a zero count is not shown.
- The start target is either the ticket's owner when that owner is not Running, or else the lowest Empty/Exited slot in 1..=6. It is never S0 and never a Running slot.
- No-slot status text: `No free slot for <KEY>: stop or clear a session first.`
- Hover hint text: `Click: open S<n>`, `Click: start in S<n>` or `Click: no free slot`, plus ` · Cmd+click: folder` when there is a folder.
- Cmd+click on a ticket card opens its folder. A Done card's click is unchanged. Cmd+click on a PR row always opens its URL.
- Public repo: invented names only (DEMO-n, demo-*). No em dashes anywhere. No emojis in commits.
- Commits: a short plain sentence, a blank line, then `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Run `git log -5` first. Never stash, reset, switch branches or push.
- Gates per task: `cargo test --workspace` (rerun once if only the overseer hook test or the server PTY burst test fails), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`.

## Review Focus

1. Clicking a ticket owned by a Running slot never starts or reassigns anything. Pinned by a `ticket_click` test.
2. All of S1 to S6 Running: a click shows the no-slot status and opens no dialog. Pinned by a `start_slot` test.
3. A ticket owned by an Exited slot restarts in that slot, not in a lower empty one. Pinned by a `start_slot` test.
4. A Your turn slot that is waiting on a lock shows "blocked by lock". Pinned by a board test.
5. The hover does not flicker or cover the card. This is a manual check.

---

### Task 1: Your turn badge

**Files:** `crates/overseer-gui/src/board.rs` (`activity_state`, the card model `Tone`, `tone_color`, `edge_color`, `needs_you_count` plus a new `your_turn_count`, the header text), `crates/overseer-gui/src/session.rs` (session tabs)

**Interfaces:** Produces `Tone::YourTurn`, `pub fn your_turn_count(slots: &[SlotInfo]) -> usize`, `pub fn tab_your_turn(info: &SlotInfo) -> bool`, and `pub fn attention_text(need: usize, turn: usize) -> Option<String>`.

- [ ] Tests:
  - `activity_state` for Idle gives `Your turn 3m 05s`.
  - A running Idle slot's card model has `Tone::YourTurn`.
  - With a lock wait it has `Tone::Blocked`.
  - `tone_color(Tone::YourTurn) == colors::ACCENT`, and the same for `edge_color`.
  - `your_turn_count` counts only Running slots with Idle activity.
  - `attention_text(2,1) == Some("2 need you · 1 your turn")`, `(0,1)` gives `"1 your turn"`, `(1,0)` gives `"1 need you"`, and `(0,0)` gives `None`.
  - `tab_your_turn` holds for Running+Idle and not for Exited.
- [ ] Implement and use them in the header and the session tabs: a hollow `○` for Your turn, while Needs you keeps its current mark. Update any existing assertion that expected the word "Idle" for a running slot.
- [ ] Run the gates and commit: `Show Your turn when a session waits for its next prompt`

### Task 2: Start from a ticket card, and PR row to session

**Files:** `crates/overseer-gui/src/sprint_view.rs` (`ticket_card` click, PR row click and chip, `PrRow` gains `slot: Option<SlotId>`), `crates/overseer-gui/src/app.rs` (a new handler `start_ticket`, plus tests)

**Interfaces:**
- `pub enum TicketClick { Open(SlotId), Start(SlotId), NoSlot }`
- `pub fn start_slot(owner: Option<SlotId>, slots: &[SlotInfo]) -> Option<SlotId>`
- `pub fn ticket_click(owner: Option<SlotId>, slots: &[SlotInfo]) -> TicketClick`

These are pure functions; put them next to `card_click` in app.rs. Also `fn start_ticket(&mut self, key: String, window, cx)`.

- [ ] Tests:
  - With an owner Running, `ticket_click` gives `Open(owner)`.
  - With an owner Exited, it gives `Start(owner)`, even when S1 is Empty and the owner is S4.
  - With no owner and S1 Running, S2 Empty, it gives `Start(2)`.
  - S0 Empty with all of S1 to S6 Running gives `NoSlot`.
  - With no owner and S3 Exited while S1 and S2 are Running, it gives `Start(3)`.
  - `pr_rows` sets `slot` only when the PR's ticket owner is Running.
- [ ] Implement:
  - `start_ticket` follows the spec flow: Assign with `confirm: true, move_owner: false` unless the target already owns the ticket, then `assign_and_start`. Errors and NoSlot go to `self.status`. Return early if `modal_open()`.
  - Ticket card click with Cmd held (`event.modifiers().platform`) opens the folder if there is one. Otherwise `Open` calls `go_session` and `Start`/`NoSlot` call `start_ticket`.
  - Done cards are unchanged.
  - A linked PR row shows a `S<n>` chip. A plain click calls `go_session`, and Cmd+click calls `open_external(url)`.
  - Every ticket card is now clickable, including cards without a folder.
- [ ] Run the gates. Then a manual check in a sandbox: click an unowned card and see the start dialog prefilled for the first free slot. Click an owned running card and see the pop-over.
- [ ] Commit: `Start work from a ticket card and open a PR's session from its row`

### Task 3: Ticket hover card

**Files:** `crates/overseer-gui/src/sprint_view.rs`

**Interfaces:**
- Consumes `ticket_click` and `TicketClick` from Task 2, and `activity_state` from Task 1.
- Produces `pub struct HoverModel { lines: Vec<(String, String)>, title: String, key: String, hint: String }` built by a pure `fn hover_model(...)`. Choose the arguments from the data you need.
- Produces `pub fn hover_hint(click: &TicketClick, has_folder: bool) -> String`.

- [ ] Tests:
  - `hover_hint(&Open(3), true) == "Click: open S3 · Cmd+click: folder"`
  - `hover_hint(&Start(4), false) == "Click: start in S4"`
  - `hover_hint(&NoSlot, false) == "Click: no free slot"`
  - `hover_model` for a ticket with a linked PR and an owner gives rows for Status, Priority, Stage (with its age), Session (`S2 · Your turn 1m 00s`) and PR (`demo-api #12 <title> 3d`). The Beads row appears only when beads > 0.
  - A ticket with no owner gives Session `Not started`.
- [ ] Implement:
  - A `HoverCard` render view: key in the accent colour, the title wrapped, label/value rows, and the hint in grey. Its max width is about 360px.
  - Attach it with `.tooltip(...)` on ticket cards only (they already have ids).
  - `TicketCard` gains what the model needs: raw status always, the stage, and the full PR details. Keep the existing fields' behaviour.
- [ ] Run the gates. Then a manual check: hovering shows the card without flicker and it does not hide the card under the mouse.
- [ ] Commit: `Show a hover card on sprint tickets`
