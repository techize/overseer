# Resizable kanban: design

Tuesday 17 November 2026. Jonathan asked for the kanban on the board to be resizable. He picked "both": the kanban fills the space by default, and a drag handle sets its height.

## Why

The kanban is fixed at 340px. On a bigger window the extra room sits empty below it, and on a smaller one columns scroll sooner than they need to.

## Goals

- **Fill by default.** The kanban takes the height left below the session cards. Resizing the window resizes the kanban.
- **Drag to set.** A thin handle on the kanban's top edge, with the up-down resize cursor, sets a fixed height. The height is kept across restarts.
- **Reset.** Double-clicking the handle goes back to filling the space.

## Anti-goals

- The session cards or the footer pushed off screen. A fixed height that no longer fits shrinks the kanban rather than the rest of the board.
- A kanban too short to use. The minimum is 180px.

## Approaches

1. Fill plus a drag handle, with the height saved in a small GUI state file (picked).
2. Fill only. Rejected: Jonathan picked both.
3. Save the height in config.toml. Rejected: that file belongs to the daemon and to Settings, and a drag would rewrite it often.

## Decided design (my calls)

- **Storage.** The height is saved as `kanban_height` in `<state dir>/gui.toml` (Application Support/Overseer, next to the socket). It is written when a drag ends and removed on reset. A missing file, unreadable file or bad value means fill.
- **Layout.** The session rows stay their natural height. In fill mode the kanban gets `flex_1`. With a fixed height it gets that height. Either way it can shrink to fit, down to the 180px minimum.
- **Drag.**
  - Mouse down on the handle records the pointer and the kanban's current rendered height.
  - Moving up makes the kanban taller, and moving down makes it shorter.
  - Releasing saves the height the kanban actually ended up at, so a height that did not fit is not stored.
- The handle only exists when the kanban is shown.
