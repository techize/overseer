# Day timeline (#84)

## Why

Each stretch of work in a slot needs a session log, and the end-of-day index
check is where a missing one turns up today: hours later, when the work is
no longer fresh. The day timeline shows, per slot, each stretch of work
Overseer recorded against the times the slot's logs cover, so a missing part
log is spotted and written while the user still remembers the work.

## Goals

- A "Today" panel on the board, below the attention panel: one row per slot
  that worked today, all rows on one time axis from the day's first stretch
  to now, updating live.
- Each stretch is drawn in three kinds of span: logged, a gap (red), and
  not logged yet (neutral).
- Hovering a stretch shows its start and end and the log(s) covering it; an
  uncovered stretch says so.
- Red gaps also appear in the attention list, so the panel can stay folded
  and the Dock count still catches them.
- `overseer ctl timeline` prints the same day.
- The day survives a daemon restart.

## Non-goals

- Past days. Only today (local time) is shown.
- Writing or fixing logs from the panel.
- Changing how stretches are cut (#80's rule stays: one per transcript, from
  its first work event to its last; `/clear` or the session ending ends it).
- Matching the 2ndBrain index's code. Overseer applies the same rule to the
  same Time lines; the index stays the final check.

## Anti-goals

- Every running slot showing red. A stretch still running is never red (the
  user's answer, below).
- A coverage rule that disagrees with the index check, so the panel says
  "fine" and the index flags a gap, or the other way round.
- A panel that needs the notes writer on.

## Approaches considered

1. **The daemon works out the day (chosen).** It stores stretches, reads
   the logs, works out coverage and sends the result as an event, like the
   attention list. The rule lives in one place and the attention item comes
   from the same data.
2. **The daemon sends raw stretches and log times; the app works out
   coverage.** The attention item needs the gaps in the daemon anyway, so
   the 10-minute rule would exist twice and could disagree.
3. **Rebuild the day from the `slot_runs` file.** No new table, but that
   file is only written with the notes writer on, which the issue rules out.

The user picked 1.

## Decisions

**When a gap is red (the user's answer, option A).** A stretch is *ended*
when a `/clear` or the session ending closed it. Only in an ended stretch
does an uncovered span longer than 10 minutes show red and raise an
attention item. In a stretch still running, uncovered time is drawn in a
neutral "not logged yet" colour. Uncovered spans of 10 minutes or less are
drawn as logged (the index ignores them too).

**Waiting time is not work (the user's answer).** A stretch runs from its
first work event to its last, so it includes time the slot sat finished and
waiting for the user (activity `Idle` with `idle_wait`). Any such wait
longer than 10 minutes is cut out of the stretch: drawn as a thin line,
never red, never needing a log. Waits of 10 minutes or less stay part of
the stretch. A stretch whose slot has been waiting more than 30 minutes
counts as ended for colouring (its gaps can go red) even though no
`/clear` or exit has closed it; if work resumes in the same transcript the
stretch is running again and the wait is cut out as above.

**Short stretches (my call).** A stretch whose work (after cutting waits)
is shorter than `session_log_min_mins` (default 15), the same floor the
end-of-session check uses, is never red and raises no item; it is drawn
neutral. Uncovered spans of 10 minutes or less inside a longer stretch are
drawn as logged, and their hover says `short gap, not flagged`.

**Where it lives (the user's answer, option A).** A panel on the board,
below the attention panel, folding to one line like the attention panel:
`Today: 2 gaps` or `Today: all logged` (wording is the builder's).

```
Today                         09:00     11:00     13:00   now
S1  ████████░░░░██████▓▓▓▓
S2       ██████████  ███▒▒▒▒▒
    █ logged  ░ gap >10m (red)  ▒ running, not logged yet
```

**Coverage rule.** For a slot, the covered minutes are the union of every
range from `session_logs::time_ranges` across all of that slot's logs for
today. That function already follows the index rule (bracket text skipped,
the line split at `,` and `;`, parts with idle/break/lunch/away/paused/gap
skipped, every range counts, a lone first start counts). A lone start
(`(start, None)`, a log written before its end) covers from its start to
now (my call: the log is still being written). Minutes come from
`session_logs::local_minutes`; a stretch that began before midnight is cut
at today's start. A range whose end is before its start is taken as
running past midnight and cut at midnight (my call).

**Which logs belong to a slot today.** The configured `session_logs` globs,
filled for today and the slot, as `find_logs` does, but without its
"modified since the run started" filter: every matching file for today
counts. A pattern without `{slot}` matches the same files for every slot,
as it does in `find_logs` today (my call; the default patterns all have
`{slot}`). A refactor that shares the glob-filling with `find_logs` is
expected.

**Notes writer off.** Reading logs needs only `brain_dir` and the
`session_logs` patterns, which the end-of-session check already reads with
the writer off. So coverage shows whenever `session_logs` is non-empty,
writer on or off (my call; the issue assumed the writer off means no logs).
With `session_logs` empty (the log check turned off) the panel shows
stretches only, all neutral, and raises no attention items. Stretches are
stored in the ledger, never in the notes folder, so they never need the
writer.

**Stretch storage.** A new ledger table (schema V11), `stretches(slot,
session, first, last, ended)`, unix seconds, `ended` NULL while running.
A row is inserted when a stretch's first work event arrives, its `last` is
updated as work continues (at most once a minute is fine), and `ended` is
set when #80's tracker ends it. Rows are keyed by `(slot, session,
first)`. Waits are stored in a second V11 table, `stretch_waits(slot,
session, start, end)` (`end` NULL while waiting), written when the slot's
activity enters and leaves `Idle` with `idle_wait`.

On daemon start (my call): rows left running whose slot still runs the same
transcript stay running, and the in-memory `Stretches` tracker is seeded
from them, so the next event extends the same row instead of starting a new
one. Rows whose slot is gone or has moved to another transcript are marked
ended at their `last`. Open waits are closed at daemon start time when the
slot no longer runs. Rows older than 7 days are deleted at start. The
existing `slot_runs` activity line is unchanged.

**Where gaps are measured (my call).** Any uncovered minute inside a
stretch's work (first to last, waits over 10 minutes cut out) counts,
edges included: a log starting 20 minutes after the first event leaves a
20-minute red gap. Time outside stretches is never a gap.

**Refresh.** A `TimelineService` (beside `AttentionService`) rebuilds the
day when a stretch starts, moves or ends, and on a 60-second tick (picks up
new or edited logs and moves "now"). It reads logs off the async runtime
(`spawn_blocking`, as `check_run` does). It sends
`Event::TimelineChanged(DayView)` only when the view differs from the last
one sent. The tick may reuse the attention tick.

**Attention item.** A new `AttentionKind::LogGap`, one item per slot with
red gaps, `since` = the end of the earliest red gap, text like
`2 unlogged gaps today, 10:40-11:05 and 14:10-14:30` (wording is the
builder's). Its key is `log_gap:<slot>`; it changes text as gaps change
and clears when a log covers them or the day rolls over. No dismiss, as in
#83. It counts toward the Dock badge like every other kind. `derive` takes
the gaps as one more input: `TimelineService` keeps its latest `DayView` on
a shared handle that the attention side reads, and nudges a re-derive when
the view changes; the attention side never asks the timeline to rebuild,
so there is no loop (my call). An
app older than this change cannot decode the new kind; like the #83
`AttentionChanged` note, it drops the connection until relaunched. The
builder adds a catch-all `#[serde(other)] Other` unit variant to
`AttentionKind` that the app and `ctl` skip, so later kinds do not repeat
this; the matches over the enum gain that arm (my call).

**Proto.** Additive, `#[serde(default)]` where it applies:

- `DayView { day: String (YYYY-MM-DD), start: i64, now: i64, rows:
  Vec<DayRow> }`
- `DayRow { slot: SlotId, stretches: Vec<DayStretch> }`
- `DayStretch { session: String, first: i64, last: i64, running: bool,
  spans: Vec<Span>, logs: Vec<String> }`, where `logs` are the file stems
  of the logs whose ranges overlap it
- `Span { from: i64, to: i64, kind: SpanKind }`, `SpanKind = logged | gap
  | unlogged` (gap is red, unlogged is the neutral running colour)
- `Event::TimelineChanged(DayView)`, `Request::Timeline` answered by
  `Response::Timeline(DayView)` (on connect and by `ctl`)

Times are unix seconds so the app does no log parsing.

**App.** The panel reads `DayView`. Rows in slot order; a running stretch
is drawn to now. Hover shows `10:02-11:40` plus the covering log stems, or
`no log covers this`, or `not logged yet` for running time. Clicking a row
opens that slot (my call, matching attention items). Hidden when there are
no stretches today.

**CLI.** `overseer ctl timeline`: one line per stretch, `S1 09:02-10:40
logged (2026-11-24-s1-demo)`, with gaps listed under it.

## Constraints

- Public repo: invented names in tests and docs (DEMO-n, demo-api,
  demo@example.test). No em dashes.
- Additive proto changes, with a test that old JSON still reads.
- Ledger migration in the existing `migrate` list; old ledgers upgrade.
- No log reading on the async runtime; no glob per hook event (rebuilds are
  driven by stretch changes and the tick).

## Acceptance

- One row per slot, from the first stretch of the day to now, updating
  live.
- Hovering a stretch shows its times and covering logs; an uncovered
  stretch says so.
- Covered and uncovered spans follow the index rule (every range on a Time
  line counts, gaps over 10 minutes flagged), with tests built from the
  existing `time_ranges` cases.
- A running stretch never shows red; once ended (or waiting over 30
  minutes), a gap over 10 minutes shows red and raises a `log_gap`
  attention item, which clears when a log covers it.
- Waits over 10 minutes are cut out of stretches and never go red.
- A daemon restart mid-stretch continues the same row.
- With `session_logs` empty, stretches show with no coverage and no items.
- Stretches survive a daemon restart.

## Left to the builder

- Panel styling, colours (red from the theme's warning family), fold
  wording and hover layout.
- Whether `TimelineService` shares a task or a tick with
  `AttentionService`.
- How often a running stretch's `last` is written (once a minute or less
  is fine).
- How the glob-filling is shared with `find_logs`.
