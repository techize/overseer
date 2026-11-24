# Attention queue (#83)

## Why

The user runs several slots at once. Today, finding out whether anything is
waiting on them means looking at each slot's panel, the relay list, the
notices and Bitbucket. The attention queue is one list of everything waiting
on the user, oldest first, with a count on the Dock icon, so a glance answers
"is anything stuck or waiting on me?".

## Goals

- One list, held by the daemon, of items waiting on the user. Each names its
  slot (or PR), says what is waiting and since when, and opens the slot or PR
  when clicked.
- Items clear themselves when their cause goes away. Nothing has to remember
  to remove them.
- Open PRs report failed builds and new comments, not only "changes
  requested".
- The Dock icon shows the count, empty at zero.
- `overseer ctl attention` prints the same list.

## Non-goals

- Dismissing or snoozing items. An item lasts as long as its cause. (Can come
  later if the list turns out noisy.)
- GitHub PRs. Overseer reads PRs from Bitbucket only; GitHub can follow the
  same shape later.
- New macOS notifications. The existing ones (`notify.rs`) stay as they are.
- The nudges in #89 and the timeline in #84. They will feed this list later.

## Anti-goals

- An item that stays after its cause is gone (the #58 kind of bug). The
  derived design exists to make this impossible.
- A noisy list. A slot that is simply working, or a relay that waits a few
  seconds for a turn to end, is not an item.

## Approaches considered

1. **Derived list (chosen).** One pure function builds the list from state the
   daemon already holds, on every change. Clearing is automatic. Only two new
   things are stored: per-PR "last seen" comment counts and a notice for the
   `/clear` part-log prompt.
2. **Stored list.** Each source adds and removes its own rows. Allows history
   and dismissal, but every source needs matching remove code, and one missed
   remove leaves a stuck item.
3. **Derived in the app.** Least daemon change, but seen-state would live in
   the app, `ctl` and S0 could not read it, and two windows could disagree.

The user picked 1.

## Sources and rules

Each item has: `kind`, `slot: Option<SlotId>`, `pr: Option<(repo, id)>`,
`since` (unix seconds), `text` (one line), and a stable `key` (so the app can
keep selection and the list does not jump).

| Kind | Raised when | `since` | Clears when |
|---|---|---|---|
| `needs_you` | slot activity is `NeedsYou` (a permission prompt or other notification), not `idle_wait` | `activity.since` | activity leaves `NeedsYou`, or the slot exits |
| `waiting` | slot activity is `Idle` with `idle_wait` (finished its turn, waiting for the next instruction) for at least 2 minutes, **and the slot has a ticket assigned** | `activity.since` | the slot gets input, loses its ticket, or exits |
| `relay_stuck` | a pending relay held with "input box not found" for more than 1 minute, or held with "session is working" while the slot's activity has been `Idle` for more than 1 minute (the #58 symptom) | when the relay first became held (continuously) | the relay is delivered, dropped, or no longer held |
| `notice` | one or more stored notices exist for the slot (session log checks, promotions, the `/clear` part-log prompt); one item per slot with the count in its text | the oldest notice's `at` | the notices are cleared (existing `clear_notices`) |
| `pr_build_failed` | an open PR's build state is `failed` | when Overseer first saw that failure | a later poll shows `passed` or `running`, or the PR is no longer open |
| `pr_changes_requested` | `approvals.changes_requested` is set | first seen | it is no longer set, or the PR closed |
| `pr_comments` | the PR's comment count is above the count recorded when the user last opened the PR from Overseer | first seen above | the user opens the PR (from the queue or the PR panel), or the PR closed |

**Which PRs count (the user's answer):** only the user's own: PRs by the
configured `pr_author`, or whose head branch is a branch one of the user's
slots is working on. Teammates' PRs, including ones awaiting the user's
review, are not items.

**Which finished slots count (the user's answer):** a slot that finished its
turn is `waiting` only when it has a ticket assigned. A parked slot with no
ticket is not an item, so the count can reach zero. `needs_you` prompts
count whatever the ticket.

**Build state (my call):** from the head commit's statuses: any FAILED or
STOPPED is `failed`; otherwise any INPROGRESS is `running`; all SUCCESSFUL
is `passed`; no statuses is `None` and raises nothing.

**Relays (my call):** a slot that is genuinely working holds its relay
normally and is never an item. `held_since` is when the relay first became
held and stays set while it remains held, whatever the reason.

One item per slot per kind; one item per PR per kind. Order: oldest `since`
first. A PR item names the owning slot when one slot's repo and branch
exactly equal the PR's repo and head branch, in a shared function that #86
will reuse; otherwise `slot` is empty.

## How it works

- **Daemon module `attention`**: `pub fn derive(slots, locks, work, seen,
  now) -> Vec<AttentionItem>`, pure and unit-tested per row of the table.
  The daemon calls it whenever `SlotChanged`, `LocksChanged` or
  `WorkChanged` is sent, and on a 30-second tick (so the time thresholds
  fire without an event). It sends `Event::AttentionChanged(AttentionView)`
  only when the list differs from the last one sent.
- **Hold times for relays**: today `set_held` keeps only the reason. It also
  records when the current hold began, and `Relay` gains
  `held_since: Option<i64>` (`#[serde(default)]`).
- **PR fetching**: the existing Bitbucket poll (every `pr_poll_secs`, default
  300) also reads each open PR's `comment_count` (a field on the PR object)
  and its build statuses (`/pullrequests/{id}/statuses`), within the existing
  parallelism and timeout limits. `PullRequest` gains `comments:
  Option<u32>` and `build: Option<BuildState>` (`passed | failed | running`),
  both `#[serde(default)]`; the PR panel can show them too. When the
  `pr_command` (bb) fallback is in use these stay `None` and no PR build or
  comment items are raised.
- **Seen state**: a new ledger table (schema V10) `pr_seen(repo, id,
  comments, at, build_failed_since, changes_since, comments_since)`. Opening
  a PR from the app sends `Request::PrSeen { repo, id }`, which records the
  current comment count. A PR with no row has nothing new: the first
  sighting records its count, so existing comments do not all appear as new
  on upgrade. The `*_since` columns hold when each PR item was first seen
  (my call), so ages survive a daemon restart; rows for PRs no longer open
  are removed on the next poll.
- **The `/clear` prompt** gets a stored notice as well as its relay, so it
  appears as a `notice` item until notices are cleared.
- **Proto**: `Event::AttentionChanged(AttentionView)`, `Request::Attention`
  (answers `Response::Attention(AttentionView)`, used on connect and by
  `ctl`), `Request::PrSeen`. The client's event decoding learns to skip an
  event type it does not know (logged once per type per connection; my
  call), so later additions do not cut off an older app. Whether that is a
  catch-all variant or reading the type first is the builder's choice. An app older than this change will still drop its
  connection on the first `AttentionChanged`; it reconnects once relaunched.
- **App**: an Attention panel at the top of the board, collapsed to a
  one-line count when empty, listing items oldest first with slot, text and
  age ("12m"). Clicking a slot item opens the slot; clicking a PR item opens
  the PR in the browser and sends `PrSeen`. The Dock badge
  (`NSApp.dockTile.setBadgeLabel`) shows the count, cleared at zero; it is
  set only inside an app bundle, like notifications.
- **CLI**: `overseer ctl attention` prints one line per item: age, slot or
  PR, text.

## Constraints

- Public repo: invented names in tests and docs (DEMO-n, demo@example.test).
  No em dashes.
- Additive proto changes with `#[serde(default)]`, plus a test that old JSON
  still reads.
- No extra Bitbucket polling beyond the existing cadence; build and comment
  reads stay inside `PR_PARALLEL` and `SOURCE_TIMEOUT`.

## Acceptance

- `derive` has a test per table row for raising and for clearing.
- The list survives an app restart (asked for on connect).
- The Dock badge matches the count and is empty at zero.
- Old `PullRequest`, `Relay` and ledger data still read.
- The client skips an unknown event type instead of dropping the connection.

## Left to the builder

- Exact panel styling and the age format.
- Whether the 30-second tick reuses an existing timer.
- How `derive` gets the slot, lock and work snapshots (shared handles or a
  small cache fed by the existing events).
