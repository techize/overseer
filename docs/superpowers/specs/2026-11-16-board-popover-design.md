# Session pop-over and full kanban: design

Monday 16 November 2026. Follows the sprint board (PR 16) and workspace PRs (PR 17).

## Intent and why

Jonathan wants to work entirely from the board. Today clicking a session card swaps the whole window to that one session, so the other five and the sprint drop out of sight until he goes back. He wants to dip into a session, type, and drop back to the full board in one move.

The kanban today caps each column at 4 cards with "+N more", and its columns do not match how his Jira board is set up. He wants every ticket visible and the columns he actually uses.

## Goals

- Clicking a running session card opens that session in a pop-over drawn over the board; the board stays visible, dimmed, behind it.
- A click on the dimmed board, the close button, or Cmd+Esc hides the pop-over and returns to the board. The session keeps running.
- The existing Cmd+0 to Cmd+6 keys open that slot's pop-over instead of the full-window screen.
- The kanban shows every ticket. Each column scrolls on its own.
- Columns, in order: Todo, Blocked, Next, In Progress, To Be Tested, Done.

## Non-goals

- Several sessions open at once, or sessions in their own OS windows.
- Plain number keys for slots (Cmd+0 to Cmd+6 already do it). Starting work from a ticket card, a "waiting for you" badge, PR row to session, ticket hover. Listed for a second round.
- Changing Jira from the board.

## Anti-goals

- Losing typing: a click inside the pop-over must never close it, and keys typed in the terminal go to the terminal, plain Esc included, as today.
- A board that grows taller as tickets are added: the kanban keeps a fixed height and scrolls inside its columns, so the session cards stay on a 1440x900 screen.
- Stage ages reset by the column change.

## Constraints

- GPUI front end (`crates/overseer-gui`), daemon work store, wire types in `overseer-proto`.
- The pop-over hosts what the session screen shows today (`render_session`: the terminal plus its side panel, message, release, note and holds actions), so nothing is lost by moving it.
- Public repo: invented names in tests and docs. No em dashes.

## Approaches

1. **Pop-over drawn on the board (picked).** A layer over the board with the terminal in a large panel. Board stays visible; one session at a time; reuses the terminal view.
2. **A separate OS window per session.** Allows side by side, but "click off" only sends the window behind the board, and Jonathan is back to managing windows. Rejected.

## Decided design

### Session pop-over

- A panel about 90% of the window's width and height, centred, over the board dimmed with a dark scrim.
- Header row: slot name (`S2`), the ticket key and title if any, the session's state, and a close button (x).
- Body: today's session screen content, laid out in the panel, with the terminal focused when the pop-over opens and resized to fit.
- Closing: the close button, a click on the scrim, or Cmd+Esc. Keys keep today's rules: plain Esc stays with the terminal (Claude Code needs it) and closes the pop-over only when focus is outside the terminal and outside an entry row, exactly as it goes back to the board today.
- When the session ends while the pop-over is open, it stays open showing the final output until closed, as the session screen does now.
- The old full-window session screen goes away; every path that opened it (card click, keyboard Enter, notifications, the locks screen's return) opens the pop-over instead.
- Cmd+0 to Cmd+6 open that slot's pop-over; with a pop-over already open they switch it to the other slot.
- State (my call): `Screen::Session(id)` now means "the board with slot id's pop-over open", so notification suppression, `start_target` and `locks_return` keep working unchanged. The pop-over keeps the `Session` key context. It is drawn as a sibling of the board element, not inside it, so the board's own keys (arrows, Enter, r, a, p, l) never fire while the pop-over has focus. The elapsed-time ticker runs while any slot is running, since the board stays visible behind the pop-over.
- Locks stays a full-window screen (my call). Opening it from a pop-over hides the pop-over; leaving Locks reopens the pop-over it came from, as `locks_return` does for the session screen today. Plan and Settings keep their own Esc behaviour.
- Clicking a notification for another slot while a pop-over is open switches the pop-over to that slot (my call).

### Kanban columns

`Stage` becomes, in display order: Todo, Blocked, Next, In Progress, To Be Tested, Done. Labels: `TODO`, `BLOCKED`, `NEXT`, `IN PROGRESS`, `TO BE TESTED`, `DONE`.

Settings, one list of Jira status names per column, matched case-insensitively as today:

| Setting | Default |
|---|---|
| `stage_todo` | Backlog, Prioritised Backlog, To Do |
| `stage_blocked` | Blocked, Waiting, On Hold |
| `stage_next` | Next, Selected for Development |
| `stage_in_progress` | In Progress, In Review, Code Review, Review |
| `stage_to_be_tested` | To Be Tested, Testing, In Testing, QA |
| `stage_done` | Done, Closed, Resolved |

- An unmapped status goes to In Progress and the card shows the raw status, as today.
- A ticket with an open PR stays in its Jira column and shows its PR chip (Jonathan's choice). The old display-only move to In Review is removed.
- Old config keys still load: `stage_waiting` is read as `stage_blocked` and `stage_testing` as `stage_to_be_tested`, each only when the new key is absent (the new key wins). `stage_in_review` names are added to whichever In Progress list is in effect. An existing config.toml keeps working.
- Ledger: stage ages survive the change. Recorded stages map InReview to InProgress, Waiting to Blocked, Testing to ToBeTested (a migration or a name mapping on read, builder's choice), so cards do not all restart at "just now". The remap is not a stage change: a ticket's `since` and `moved` stay as recorded, and the first poll after the upgrade must not reset them.
- Icons and colours: Blocked keeps Waiting's orange and `‖`; To Be Tested keeps Testing's purple; Next gets its own icon and colour from the GUI's `colors` constants in `app.rs`, not a setting (my call: `colors::ACCENT` and `→`); In Progress keeps its own.

### Kanban layout

- Every ticket shows; no 4-card cap and no "+N more".
- The kanban has a fixed height (builder sizes it so the six session cards and the kanban fit 1440x900); each column scrolls vertically on its own with its header pinned, and shows its count in the header.
- Done stays narrow: its count and its tickets, most recently finished first, scrolling like the others.
- Sorting within a column is unchanged: priority, then time in stage, oldest first.

## Left to the builder

- GPUI layering for the scrim and panel, and how focus returns to the board on close.
- Exact kanban height and column widths.
- Ledger migration versus read-time mapping.
- Whether `Stage` keeps old serde names via aliases for an older daemon or GUI; both are installed together, so this is not required.
