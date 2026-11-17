# Board actions: design

Tuesday 17 November 2026. Second round after the session pop-over (PR 18).

## Intent and why

Jonathan works from the board now. Four things still send him elsewhere or leave him guessing:

- Starting a ticket takes several steps: press `a`, pick the ticket, then submit the start dialog.
- A session that has finished and is waiting for its next prompt only says "Idle".
- A PR row only opens the browser, even when a session on the board owns its ticket.
- A ticket card shows a truncated title and little else.

He said "go with your gut", so every call below is mine.

## Goals

1. **Your turn.** A running session whose activity is Idle shows "Your turn 3m 05s" in the accent colour on its card and its session tab. The board header counts them beside the Needs-you count.
2. **Start from a ticket card.** A click on a ticket card does one of two things:
   - If the ticket's owner slot is Running, it opens that slot's pop-over.
   - Otherwise it starts work: the ticket is assigned to a slot, the kickoff is fetched, and the start dialog opens prefilled. This is the same path `a` plus the picker takes today.
3. **PR row to session.**
   - When a PR's ticket is owned by a Running slot, the row shows that slot (`S3`) and a click opens its pop-over.
   - Otherwise a click opens the PR in the browser, as today.
4. **Ticket hover.** Hovering a ticket card shows a card with:
   - the key and the full title
   - the Jira status and the priority
   - the stage and how long it has been there
   - its session (slot and activity, or "Not started")
   - its linked PR (repo, id, title, age), if any
   - its beads count
   - a hint line saying what a click and a Cmd+click do

## Non-goals

- New Jira fields: no description and no assignee. The hover uses only data the board already has.
- Notifications for Your turn. Needs you already notifies, and a notification every time Claude stops would be noise.
- Changing anything in the daemon. All four are GUI-only.

## Anti-goals

- A click that silently takes over a busy session. Starting from a ticket never uses a Running slot and never moves a ticket off a Running owner.
- Losing the ticket-folder shortcut. Cmd+click on a ticket card opens the 2ndBrain folder, and a click on a Done card still opens it, since Done tickets have nothing to start.
- A hover that covers the card you are aiming at, or flickers while the mouse crosses the board.

## Decided design (my calls)

**Your turn**
- Applies to a Running slot with `activity.kind == Idle`. This includes a fresh session waiting for its first prompt, since that is also waiting for Jonathan.
- Style: `Tone::YourTurn` in `colors::ACCENT`, for both the badge and the card edge.
- A lock wait still overrides it to "blocked by lock", as it does Idle today.
- Session tabs: Needs you keeps its filled dot. Your turn gets a hollow `○`.
- Header: "2 need you · 1 your turn". A part with a zero count is not shown.

**Start from a ticket card**
- Target slot:
  - If the ticket has an owner that is not Running, use that owner.
  - Otherwise use the lowest-numbered Empty or Exited slot among S1 to S6. S0 is the coordinator and is never picked.
  - If neither exists, set the status line to "No free slot for DEMO-1: stop or clear a session first." and do nothing else.
- Flow:
  - Send `Request::Assign { key, slot, confirm: true, move_owner: false }`. The owner is either absent or not Running, so no confirm prompt is needed.
  - Then call the existing `assign_and_start(slot, key)`.
  - If the ticket is already owned by the target slot, skip the Assign.
  - Any error reply goes to the status line.
- Cmd+click opens the folder when there is one. A Done card keeps today's click (folder only).

**PR row to session**
- A linked row shows a small `S3` chip after the ticket key.
- A click opens the slot's pop-over. Cmd+click always opens the browser.
- A row with no link behaves as today.

**Ticket hover**
- A GPUI `.tooltip` view on each ticket card (not on Done cards), styled like the existing `TextTooltip` but multi-line and up to about 360px wide.
- The hint line reads one of:
  - "Click: open S3"
  - "Click: start in S4"
  - "Click: no free slot"
  - followed by " · Cmd+click: folder" when there is a folder.

## Left to the builder

- Exact layout of the hover card and the PR chip.
- Where the slot-picking and click-decision functions live. They should be pure functions with tests, like `card_click`.
