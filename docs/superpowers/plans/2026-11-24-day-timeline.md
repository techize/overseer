# Day Timeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A live per-slot strip for today on the board showing each stretch of work against the times the slot's session logs cover, with unlogged gaps over 10 minutes in red and raised as attention items.

**Architecture:** Stretches (#80) and the slot's waits are stored in the ledger (V11). A pure `timeline::build_day` turns stretches, waits and today's log ranges into a `DayView`; a `TimelineService` rebuilds it on stretch changes and a 60 s tick and broadcasts `Event::TimelineChanged`. `AttentionService` reads that event to raise `log_gap` items; the GUI draws a "Today" panel and `ctl timeline` prints it.

**Tech Stack:** Rust workspace (`overseer-proto`, `overseer-daemon`, `overseer` CLI, `overseer-gui` on gpui), rusqlite, tokio, serde.

**Spec:** `docs/superpowers/specs/2026-11-24-day-timeline-design.md`

## Global Constraints

- Public repo: invented names only in tests and docs (DEMO-n, demo-api, demo-ws, demo@example.test). No em dashes anywhere. No emojis in commits.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Additive proto changes, `#[serde(default)]` on new fields of existing types, with a test that old JSON still reads.
- Ledger migration goes in the existing `migrate` list as step 11; `VERSION` becomes 11; old ledgers upgrade keeping their rows.
- No log reading on the async runtime (`spawn_blocking`); no glob per hook event.
- Thresholds: gap flagged when uncovered span > 10 minutes (600 s); waits > 10 minutes (600 s) cut out; a stretch waiting > 30 minutes (1800 s) counts as ended; short floor is `session_log_min_mins` (default 15); stretch rows older than 7 days deleted at daemon start; timeline tick 60 s.
- Never run the real `security` binary in tests. Work only in `/Users/jonathan/overseer/.claude/worktrees/share-ready` on branch `day-timeline`.
- Test command: `cargo test -p <crate> <filter>`; full: `cargo test --workspace`. Lint: `cargo clippy --workspace --all-targets -- -D warnings`. Known flake: `pre_tool_use_ignores_other_tools` (rerun alone if it fails).

## Review Focus

1. A stretch that crosses midnight, or a daemon left running overnight: today's row must start at local midnight and yesterday's work must not appear red today.
2. A log edited while its slot is running (a lone start `**Time**: 09:05`, then the end added later): the panel must move from not-logged-yet to logged on the next tick, never flash red.
3. A daemon restart mid-stretch: the same row continues; no ended stretch with a red tail appears.
4. An app older than this change: connecting must not crash the daemon side; an unknown `AttentionKind` in a newer daemon must not drop a newer app's connection (the `Other` catch-all).
5. `session_logs` empty: the panel still shows stretches, all neutral, and no `log_gap` items.

Tests for each are named in the owning task.

---

### Task 1: Proto types

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (Request, Response, Event, EVENT_TYPES, AttentionKind, new day types beside `AttentionView`)
- Modify: every exhaustive `match` over `AttentionKind` (`crates/overseer-daemon/src/attention.rs` `kind_name`, `crates/overseer/src/ctl.rs`, `crates/overseer-gui/src/attention_view.rs`) to add the new arms so the workspace builds.

**Interfaces:**
- Produces:
  - `pub struct DayView { pub day: String, pub start: i64, pub now: i64, pub rows: Vec<DayRow> }` (derive Debug, Clone, PartialEq, Default, Serialize, Deserialize)
  - `pub struct DayRow { pub slot: SlotId, pub stretches: Vec<DayStretch> }`
  - `pub struct DayStretch { pub session: String, pub first: i64, pub last: i64, pub running: bool, pub spans: Vec<Span>, pub logs: Vec<String> }`
  - `pub struct Span { pub from: i64, pub to: i64, pub kind: SpanKind }`
  - `pub enum SpanKind { Logged, Gap, Unlogged, Wait, Short }` serde snake_case. `Wait` is a cut-out wait (thin line); `Short` is an uncovered span of 600 s or less, drawn as logged with hover `short gap, not flagged`.
  - `impl DayView { pub fn gaps(&self, slot: SlotId) -> Vec<&Span> }` (spans of kind `Gap` for that slot, in time order)
  - `Event::TimelineChanged(DayView)`, `"TimelineChanged"` added to `EVENT_TYPES`
  - `Request::Timeline`, `Response::Timeline(DayView)`
  - `AttentionKind::LogGap` (serde `log_gap`) and `#[serde(other)] AttentionKind::Other` (last variant)

- [ ] **Step 1: Write failing tests** in msg.rs's test module:
  - `day_view_round_trips`: a `DayView` with one row, one stretch, spans of each `SpanKind`, through `Event::TimelineChanged` JSON and back, equal.
  - `timeline_request_and_response_round_trip`.
  - `attention_kind_log_gap_is_snake_case`: `serde_json::to_string(&AttentionKind::LogGap) == "\"log_gap\""`.
  - `unknown_attention_kind_reads_as_other`: `serde_json::from_str::<AttentionKind>("\"from_the_future\"")` is `Ok(AttentionKind::Other)`; an `AttentionView` JSON holding one item of kind `"from_the_future"` decodes.
  - `event_types_lists_timeline_changed`: `EVENT_TYPES.contains(&"TimelineChanged")`.
- [ ] **Step 2:** `cargo test -p overseer-proto` fails (types missing).
- [ ] **Step 3:** Add the types and variants. In `kind_name` return `"log_gap"` and `"other"`. In ctl and the GUI, `Other` items are skipped when listing (filter them out where items are turned into rows/lines); `LogGap` lists like a slot item.
- [ ] **Step 4:** `cargo test -p overseer-proto` passes; `cargo build --workspace` builds.
- [ ] **Step 5: Commit** `Add day timeline and log gap types to the protocol`.

### Task 2: Ledger V11 stretches and waits

**Files:**
- Modify: `crates/overseer-daemon/src/ledger.rs`

**Interfaces:**
- Produces (on `Ledger`, each `-> Result<..>` like the existing methods):
  - `pub struct StoredStretch { pub slot: SlotId, pub session: String, pub first: i64, pub last: i64, pub ended: Option<i64> }`
  - `pub struct StoredWait { pub slot: SlotId, pub session: String, pub start: i64, pub end: Option<i64> }`
  - `fn put_stretch(&self, slot: SlotId, session: &str, first: i64, last: i64) -> Result<()>` (insert or update `last`, keyed by `(slot, session, first)`)
  - `fn end_stretch(&self, slot: SlotId, session: &str, first: i64, last: i64, ended: i64) -> Result<()>` (sets `last` and `ended`; inserts if missing)
  - `fn open_wait(&self, slot: SlotId, session: &str, start: i64) -> Result<()>` (no-op if an open wait for that slot exists)
  - `fn close_wait(&self, slot: SlotId, end: i64) -> Result<()>` (closes the slot's open wait, if any)
  - `fn stretches_since(&self, since: i64) -> Result<Vec<StoredStretch>>` (rows whose `last >= since`, ordered by slot then first)
  - `fn waits_since(&self, since: i64) -> Result<Vec<StoredWait>>` (rows whose `end` is NULL or `>= since`)
  - `fn open_stretches(&self) -> Result<Vec<StoredStretch>>` (rows with `ended` NULL)
  - `fn prune_stretches(&self, before: i64) -> Result<()>` (deletes stretches and waits whose last/end is before `before`)
- Schema (V11):

```sql
CREATE TABLE IF NOT EXISTS stretches (
    slot INTEGER NOT NULL, session TEXT NOT NULL,
    first INTEGER NOT NULL, last INTEGER NOT NULL, ended INTEGER,
    PRIMARY KEY (slot, session, first));
CREATE TABLE IF NOT EXISTS stretch_waits (
    slot INTEGER NOT NULL, session TEXT NOT NULL,
    start INTEGER NOT NULL, end INTEGER,
    PRIMARY KEY (slot, start))
```

- [ ] **Step 1: Write failing tests:** `v10_ledger_upgrades_to_v11_keeping_pr_seen` (build a V10 file the way the existing V10 upgrade test does, open, `pr_seen` row still there, `user_version` 11); `put_stretch_updates_last_in_place` (two puts with the same first give one row with the later `last`); `end_stretch_sets_ended`; `open_stretches_lists_only_unended`; `one_open_wait_per_slot` (two `open_wait` then `close_wait(…, 500)` give one wait ending 500); `stretches_since_filters_on_last`; `prune_deletes_old_rows_only`.
- [ ] **Step 2:** `cargo test -p overseer-daemon ledger` fails.
- [ ] **Step 3:** Implement schema step 11 and the methods.
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5: Commit** `Store stretches and waits in the ledger`.

### Task 3: Today's log ranges and the pure day builder

**Files:**
- Modify: `crates/overseer-daemon/src/session_logs.rs` (share glob filling; add today's lookup and day start)
- Create: `crates/overseer-daemon/src/timeline.rs` (pure builder; `pub mod timeline;` in `lib.rs`)

**Interfaces:**
- Consumes: `StoredStretch`, `StoredWait` (Task 2); `DayView`, `DayRow`, `DayStretch`, `Span`, `SpanKind` (Task 1); `session_logs::time_ranges`, `local_minutes`.
- Produces:
  - `session_logs::local_day_start(t: i64) -> i64`: unix seconds of local midnight of `t`'s day.
  - `session_logs::today_logs(brain: &Path, home: &Path, patterns: &[String], slot: SlotId, now: i64) -> Vec<DayLog>` where `pub struct DayLog { pub name: String, pub ranges: Vec<(u32, Option<u32>)> }`: every file the patterns match for today and `slot`, no modified-since filter, sorted, deduplicated; `find_logs` and this share one glob-fill helper.
  - `timeline::Inputs<'a> { pub now: i64, pub day_start: i64, pub day: String, pub stretches: &'a [StoredStretch], pub waits: &'a [StoredWait], pub logs: &'a HashMap<SlotId, Vec<DayLog>>, pub check_logs: bool, pub min_secs: i64 }`
  - `timeline::build_day(i: &Inputs) -> DayView`
  - Constants in timeline.rs: `GAP_SECS = 600`, `WAIT_CUT_SECS = 600`, `WAIT_ENDS_SECS = 1800`.

Rules `build_day` applies (from the spec):
- Stretches are clipped to `[day_start, now]`; ones ending before `day_start` are dropped. Rows in slot order, stretches by `first`.
- `DayView.start` is `day_start`; `DayView.now` is `now`.
- A running stretch (`ended` None) is drawn to `now`. It is `running: true` unless its slot has an open wait older than `WAIT_ENDS_SECS`, in which case it counts as ended for colouring.
- Waits (closed or open, measured to `now`) longer than `WAIT_CUT_SECS` that overlap the stretch become `Wait` spans and are not work. Shorter waits are work.
- Covered time for a slot = union of its logs' ranges: minute `m` maps to `day_start + m*60`; `(s, None)` covers `s` to `now`; `(s, Some(e))` with `e < s` covers `s` to midnight.
- Each uncovered work span: if `check_logs` is false, `Unlogged`; else if 600 s or less, `Short`; else if the stretch is running, `Unlogged`; else if the stretch's total work is under `min_secs`, `Unlogged`; else `Gap`.
- Covered work spans are `Logged`. `logs` lists the stems of logs whose ranges overlap the stretch.

- [ ] **Step 1: Write failing tests** in timeline.rs, with `day_start = 0` style fixed numbers (no clock) unless noted:
  - `logged_stretch_is_all_logged`: stretch 9:00-10:00 ended, log `09:00 - 10:00` gives one `Logged` span and `logs == ["2026-11-24-s1-demo"]`.
  - `ended_stretch_gap_over_ten_minutes_is_red`: stretch 9:00-10:00 ended, log `09:00 - 09:40`: `Logged` 9:00-9:40 then `Gap` 9:40-10:00.
  - `gap_of_ten_minutes_or_less_is_short`: log `09:00 - 09:50` gives `Short` 9:50-10:00.
  - `running_stretch_is_never_red`: same as the red case but `ended: None`, no wait: tail is `Unlogged`.
  - `edge_gap_counts`: log `09:20 - 10:00` gives `Gap` 9:00-9:20.
  - `long_wait_is_cut_out`: stretch 9:00-12:00 ended, closed wait 10:00-11:30, log `09:00 - 10:00, 11:30 - 12:00` gives `Logged`, `Wait`, `Logged`, no `Gap`.
  - `short_wait_is_work`: wait 10:00-10:05 inside an unlogged ended stretch is part of the `Gap`.
  - `waiting_over_thirty_minutes_counts_as_ended`: running stretch 9:00-10:00, open wait from 10:00, `now` 10:31, log `09:00 - 09:30`: `Gap` 9:30-10:00, `running == false`.
  - `lone_start_covers_to_now`: running stretch, log `**Time**: ~9:05`, `now` 9:40: covered from 9:05.
  - `short_stretch_is_never_red`: ended stretch of 12 min with no log, `min_secs = 900`: `Unlogged`.
  - `logs_off_shows_stretches_only`: `check_logs: false`, ended unlogged stretch: `Unlogged`, no `Gap` (Review Focus 5).
  - `stretch_from_yesterday_is_clipped_to_midnight`: stretch from `day_start - 3600` to `day_start + 3600`, ended, logged `00:00 - 01:00`: one `Logged` span starting at `day_start` (Review Focus 1).
  - `stretch_ended_yesterday_is_dropped`.
  - `index_rule_cases`: reuse the `time_ranges` examples (`08:02 - 10:35, 10:46 - 12:15`, `idle 13:15 - 16:00` skipped) to show the 11-minute hole between 10:35 and 10:46 is a `Gap` in an ended stretch spanning 8:02-12:15.
  - In session_logs tests: `local_day_start_is_local_midnight` (`local_minutes(local_day_start(t)).1 == 0` and same date as `t` for a fixed `t`); `today_logs_ignores_modified_time` (tempdir with a file matching today's pattern and an old mtime is found; another slot's file is not).
- [ ] **Step 2:** `cargo test -p overseer-daemon timeline` and `session_logs` fail.
- [ ] **Step 3:** Implement. Existing `find_logs` tests must still pass unchanged.
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5: Commit** `Work out each slot's day from stretches, waits and logs`.

### Task 4: Record stretches and waits as they happen

**Files:**
- Modify: `crates/overseer-daemon/src/stretches.rs` (expose the open stretch; seed)
- Modify: `crates/overseer-daemon/src/server.rs` (`watch_session_logs`, `stretch_ended`, daemon start)

**Interfaces:**
- Consumes: Task 2 ledger methods.
- Produces:
  - `Stretches::open(&self, slot: SlotId) -> Option<Stretch>` (the slot's open stretch, if it has work)
  - `Stretches::seed(&mut self, s: &Stretch)` (puts a stored running stretch back as open)
  - A `tokio::sync::watch::Sender<()>` (or `Notify`) named `timeline_nudge` that `watch_session_logs` pokes whenever it writes a stretch row; Task 5 receives it.
- Behaviour:
  - After each `seen`: if it ended a stretch, `end_stretch(.., ended = s.at)`; then for the slot's open stretch, `put_stretch` when it is new or its `last` moved by 60 s or more since the last write for that slot. `ended(slot)` on exit calls `end_stretch(.., ended = exit time)`. The existing `slot_runs` line and checks are unchanged.
  - Waits are recorded from `Event::SlotChanged` in the same task: activity `Idle` with `idle_wait` opens a wait at `activity.since` for the slot's current session; any other activity, or the slot exiting, closes it at the new activity's `since` (or exit time).
  - At daemon start, before the hook feed starts: `prune_stretches(now - 7*86400)`; for each `open_stretches()` row, if the slot is running with the same session id, `seed` it; otherwise `end_stretch(.., ended = last)`. Open waits of slots not running are closed at start time.
  - Ledger errors are logged with `tracing::warn!` and never stop the task.

- [ ] **Step 1: Write failing tests:** in stretches.rs, `open_reports_the_running_stretch` and `seeded_stretch_is_extended_not_restarted` (seed 100-200 for session `a`, then a `Stop` at 300 for `a`: `open` gives first 100, last 300). In `crates/overseer-daemon/tests/server.rs`, following the existing hook-driven stretch tests: `stretch_rows_are_written_as_work_happens` (hooks for a slot, then ledger `stretches_since(0)` has one running row; after `/clear` with a new transcript it is ended); `restart_continues_the_running_stretch` (Review Focus 3: write an open row, start the daemon with that slot's session still the same, send a work hook, still one row with the original `first`).
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Tests pass, plus the existing stretch and session-log tests.
- [ ] **Step 5: Commit** `Record stretches and waits in the ledger as they happen`.

### Task 5: TimelineService and the Timeline request

**Files:**
- Create: `crates/overseer-daemon/src/timeline_service.rs` (or a `TimelineService` section in timeline.rs; builder's choice, one file per responsibility)
- Modify: `crates/overseer-daemon/src/server.rs` (spawn beside `AttentionService`; handle `Request::Timeline`)

**Interfaces:**
- Consumes: `build_day`, `today_logs`, `local_day_start` (Task 3); ledger reads (Task 2); `timeline_nudge` (Task 4).
- Produces:
  - `TimelineService::spawn(ledger: Arc<Ledger>, logs: Option<LogSource>, min_secs: i64, nudge: <Task 4 receiver>, events: broadcast::Sender<Event>) -> Arc<TimelineService>` where `LogSource { brain_dir: PathBuf, home: PathBuf, patterns: Vec<String> }` is `None` when `session_logs` is empty.
  - `TimelineService::current(&self) -> DayView`
  - Rebuilds on a nudge, on `Event::SlotChanged` (waits move), and every 60 s. Log reading runs in `spawn_blocking`. Sends `Event::TimelineChanged` only when the view's rows differ from the last sent (ignore `now` when comparing, so the tick alone does not resend an unchanged day).
  - `Request::Timeline => Response::Timeline(timeline.current())`.

- [ ] **Step 1: Write failing tests** in `tests/server.rs`: `timeline_request_answers_today` (after work hooks for a slot, `Request::Timeline` returns a `DayView` with one row for that slot); `timeline_changed_is_sent_when_a_stretch_starts`; `log_written_later_turns_unlogged_to_logged` (Review Focus 2: a running slot with a lone-start log, then the log is rewritten with an end and the stretch ended; the next view shows `Logged` and never `Gap` for the covered part).
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement and wire.
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5: Commit** `Keep today's timeline current in the daemon`.

### Task 6: Log gap attention items

**Files:**
- Modify: `crates/overseer-daemon/src/attention.rs`

**Interfaces:**
- Consumes: `DayView::gaps` (Task 1); `Event::TimelineChanged` (Task 5).
- Produces: `Inputs` gains `pub day: &'a DayView`. `derive` adds one `LogGap` item per slot with gaps: key `log_gap:<slot>`, `since` = `to` of the earliest gap, text `1 unlogged gap today, 09:40-10:00` / `2 unlogged gaps today, 09:40-10:00 and 14:10-14:30` / for three or more, the first two then `and N more`. Times are local `HH:MM`.
- `AttentionService::spawn` seeds `day` as `DayView::default()` and replaces it on `Event::TimelineChanged(v)`. (Ruling: the event replaces the spec's shared handle plus nudge; same effect, no loop, matches how the service already reads slots, locks and work.)

- [ ] **Step 1: Write failing tests** in attention.rs: `log_gap_raised_per_slot_with_gaps`, `log_gap_text_lists_two_then_more`, `log_gap_since_is_end_of_first_gap`, `no_gaps_no_item`, `log_gap_clears_when_covered` (same inputs with the gap now `Logged` gives no item).
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5: Commit** `Raise unlogged gaps on the attention list`.

### Task 7: `overseer ctl timeline`

**Files:**
- Modify: `crates/overseer/src/ctl.rs`

**Interfaces:**
- Consumes: `Request::Timeline` / `Response::Timeline(DayView)`.
- Produces: `CtlCmd::Timeline { json: bool }`. Text output, one line per stretch then one indented line per `Gap`, `Unlogged` or `Wait` span:

```
S1 09:02-10:40 logged (2026-11-24-s1-demo)
S2 09:30-11:05 running
  gap 10:12-10:40
  not logged yet 10:40-11:05
```

  Stretch status word: `logged` when every work span is `Logged` or `Short`, `running` when running, `gaps` otherwise; covering log stems in brackets when any. `Nothing worked on today.` when there are no rows. `--json` prints the `DayView`.

- [ ] **Step 1: Write failing tests** beside the `attention` ctl tests: parse `["x", "timeline", "--json"]`; a pure `timeline_lines(&DayView, clock) -> Vec<String>` test for the example above and for the empty day.
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5: Commit** `Print today's timeline from ctl`.

### Task 8: Today panel on the board

**Files:**
- Create: `crates/overseer-gui/src/timeline_view.rs` (pure layout plus the element)
- Modify: `crates/overseer-gui/src/app.rs` (hold `timeline: DayView`; request `Request::Timeline` on connect next to `Request::Attention`; apply `Event::TimelineChanged`; clear on connection lost like attention), `crates/overseer-gui/src/board.rs` (place the panel directly below the attention panel), `crates/overseer-gui/src/lib.rs`, `README.md` (one short section: the Today panel, its colours, `ctl timeline`)

**Interfaces:**
- Consumes: `DayView` and friends (Task 1).
- Produces (pure, tested): `timeline_view::fold_label(v: &DayView) -> String` (`Today: all logged`, `Today: 1 gap`, `Today: 2 gaps`); `timeline_view::bars(v: &DayView, width: f32) -> Vec<RowBars>` mapping each span to `x`/`w` on one shared axis from `v.start`(first stretch's `first`, else `day_start`) to `v.now`; `timeline_view::hover(s: &DayStretch, span: &Span, clock) -> String` giving `09:02-10:40` and then either the log stems, `no log covers this` (Gap), `not logged yet` (Unlogged), `short gap, not flagged` (Short) or `waiting` (Wait).
- Colours: Logged in the theme's success/neutral fill, Gap in the warning (red) family, Unlogged a muted neutral, Wait a thin line, Short drawn as Logged. Folding like the attention panel; hidden when `rows` is empty. Clicking a row opens that slot (same action attention slot items use).

- [ ] **Step 1: Write failing tests** for `fold_label` (0, 1, 2 gaps), `bars` (two rows on one axis: a stretch at the axis start has `x == 0`, one ending at `now` ends at `width`), and `hover` for each `SpanKind`.
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement the pure functions, then the element and wiring.
- [ ] **Step 4:** `cargo test -p overseer-gui` passes; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` green.
- [ ] **Step 5: Commit** `Show today's timeline on the board`.
