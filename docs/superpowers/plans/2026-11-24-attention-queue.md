# Attention Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One daemon-held list of everything waiting on the user, shown at the top of the board, as a Dock badge count, and by `overseer ctl attention` (#83).

**Architecture:** A pure `attention::derive` builds the list from the slot list, the lock view (relays), the work view (items, notices, PRs), stored PR first-seen times and each slot's repo and branch. A daemon task re-derives on every `SlotChanged`, `LocksChanged` and `WorkChanged` and on a 30-second tick, and sends `Event::AttentionChanged` only when the list changed. The PR poll learns comment counts and head-commit build state; a new ledger table keeps per-PR seen counts and first-seen times.

**Tech Stack:** Rust workspace (tokio, serde, rusqlite, ureq, gpui, objc `msg_send!`).

**Spec:** `docs/superpowers/specs/2026-11-24-attention-queue-design.md`

## Global Constraints

- Public repo: invented names only in tests and docs (DEMO-n, demo-api, demo@example.test). No em dashes anywhere (code, comments, docs, commits).
- Proto changes are additive with `#[serde(default)]`, each with a test that old JSON still reads.
- No extra Bitbucket polling beyond the existing cadence (`pr_poll_secs`); build and comment reads stay inside `PR_PARALLEL` and `SOURCE_TIMEOUT`.
- With the `pr_command` (bb) fallback, `comments` and `build` stay `None` and raise no items.
- Only the user's own PRs count: with `pr_author` set, every PR in `WorkView.prs` (already filtered to that author); plus any PR whose repo and head branch exactly equal a slot's. With `pr_author` empty, only the branch-matched ones.
- Thresholds: `waiting` after 120 s idle and only with an item owned by the slot; `relay_stuck` after 60 s; tick every 30 s.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. No emojis in commits.
- `/clear` already stores a notice (`store.tell` in `server.rs` `stretch_ended`, PR #82): nothing to add for it.

## Review Focus

1. A PR read that fails for one repo must not delete that repo's `pr_seen` rows (otherwise ages reset and comments look new or vanish). Test in Task 4.
2. A slot whose cwd is a git worktree (`.git` is a file) or a subfolder of the repo still yields its repo and branch; a detached HEAD yields none. Test in Task 5.
3. A PR's comment count going down (a deleted comment) raises nothing and lowers the recorded count, so the next new comment shows. Test in Task 4.
4. An activity or hold time later than `now` (clock skew) gives age 0, never a negative age or a panic. Test in Task 5 and Task 8.
5. An app that receives an event type it does not know keeps its connection and keeps receiving later events. Test in Task 1.

---

### Task 1: Proto types, events and unknown-event skipping

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (PullRequest, Relay, Request, Response, Event, new types)
- Modify: `crates/overseer-proto/src/frame.rs` (`decode`, KIND_EVENT branch)
- Modify: `crates/overseer-proto/src/client.rs` (reader loop)
- Modify: any exhaustive `match` on `Event` in other crates (add arms so the workspace builds; GUI arms may be no-ops until Task 8)

**Interfaces:**
- Produces:
  - `pub enum BuildState { Passed, Failed, Running }` (Serialize, Deserialize, Copy, Eq)
  - `PullRequest` gains `#[serde(default)] comments: Option<u32>`, `#[serde(default)] build: Option<BuildState>`, `#[serde(default)] head: Option<String>` (head commit hash)
  - `Relay` gains `#[serde(default)] held_since: Option<i64>`
  - `pub enum AttentionKind { NeedsYou, Waiting, RelayStuck, Notice, PrBuildFailed, PrChangesRequested, PrComments }` serialized snake_case
  - `pub struct AttentionItem { key: String, kind: AttentionKind, slot: Option<SlotId>, pr: Option<(String, u64)>, url: Option<String>, since: i64, text: String }`
  - `pub struct AttentionView { items: Vec<AttentionItem> }` with `pub fn count(&self) -> usize`
  - `Event::AttentionChanged(AttentionView)`; `Event::Unknown { kind: String }` marked `#[serde(skip)]` (never sent, only built by the decoder)
  - `Request::Attention`, `Request::PrSeen { repo: String, id: u64 }`, `Response::Attention(AttentionView)`

- [ ] **Step 1: Write the failing tests** in `msg.rs` tests and `frame.rs` tests

```rust
#[test]
fn old_pull_request_and_relay_json_still_read() {
    let pr: PullRequest = serde_json::from_str(
        r#"{"repo":"demo-api","id":7,"title":"t","branch":"DEMO-1","url":null,"author":null,"created":null,"ticket":null}"#,
    ).unwrap();
    assert_eq!((pr.comments, pr.build, pr.head), (None, None, None));
    let r: Relay = serde_json::from_str(
        r#"{"id":1,"at":2,"from":{"type":"You"},"to":3,"text":"hi","state":"Pending","delivered_at":null,"held":null}"#,
    ).unwrap();
    assert_eq!(r.held_since, None);
}

#[test]
fn attention_changed_round_trips() {
    let v = AttentionView { items: vec![AttentionItem {
        key: "needs_you:3".into(), kind: AttentionKind::NeedsYou, slot: Some(3), pr: None,
        url: None, since: 100, text: "permission prompt".into() }] };
    let ev = Event::AttentionChanged(v.clone());
    let back: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(back, ev);
    assert_eq!(v.count(), 1);
}

// frame.rs
#[test]
fn an_unknown_event_type_decodes_as_unknown() {
    let f = decode(KIND_EVENT, br#"{"type":"FromTheFuture","data":{"x":1}}"#).unwrap();
    assert_eq!(f, Frame::Event(Event::Unknown { kind: "FromTheFuture".into() }));
    // A known type with a bad body is still an error.
    assert!(decode(KIND_EVENT, br#"{"type":"WorkChanged","data":5}"#).is_err());
}
```

Plus a client test (in `client.rs` tests, using the existing test server helpers there or a `UnixListener` pair): the server writes an unknown event frame, then a `SlotChanged` event; the subscriber receives only the `SlotChanged`, and a following request still gets its response.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p overseer-proto`
Expected: compile errors for the missing types and variants.

- [ ] **Step 3: Implement**

`decode` for KIND_EVENT: try `serde_json::from_slice::<Event>`; on error, read the payload as `serde_json::Value` and take its `type` string; if it is not one of the known event type names (keep a `const EVENT_TYPES: &[&str]` next to `Event`, with a test that every variant but `Unknown` serializes to a name in it), return `Frame::Event(Event::Unknown { kind })`; otherwise return the error. The client reader does not forward `Unknown`; it logs `tracing::info!("skipping unknown event {kind}")` once per kind per connection (a `HashSet` inside the reader task).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p overseer-proto && cargo build --workspace`
Expected: PASS and the workspace builds.

- [ ] **Step 5: Commit** `Add attention, build and comment types to the protocol`

---

### Task 2: When a relay hold began

**Files:**
- Modify: `crates/overseer-daemon/src/locks/mod.rs` (`held` field, `view`, `set_held`, test near line 740)
- Modify: `crates/overseer-daemon/src/relay/courier.rs` (make `WORKING` and `NO_BOX` `pub(crate)`)

**Interfaces:**
- Consumes: `Relay.held_since` (Task 1)
- Produces: `LockView.relays[i].held_since` filled for pending held relays; `pub(crate) const WORKING`, `pub(crate) const NO_BOX` in `relay::courier`. `set_held(&self, slot, reason)` keeps its signature and takes `now` from `unix_now()` internally; add `set_held_at(&self, slot, reason, now: i64)` for tests and have `set_held` call it.

- [ ] **Step 1: Write the failing test** `held_since_is_when_the_hold_began_and_survives_reason_changes`

```rust
locks.set_held_at(2, Some("session is working"), 100);
locks.set_held_at(2, Some("input box not found"), 150); // reason changes, still held
let r = pending_to(&locks, 2);
assert_eq!(r.held.as_deref(), Some("input box not found"));
assert_eq!(r.held_since, Some(100));
locks.set_held_at(2, None, 200);
assert_eq!(pending_to(&locks, 2).held_since, None);
locks.set_held_at(2, Some("session is working"), 300);
assert_eq!(pending_to(&locks, 2).held_since, Some(300));
```

- [ ] **Step 2: Run to verify failure:** `cargo test -p overseer-daemon held_since` fails (no `set_held_at`).
- [ ] **Step 3: Implement:** `held: Mutex<[Option<(&'static str, i64)>; SLOT_COUNT]>`; the start time is kept while the slot stays held and reset on `None`. Announce only when the reason changes (as today).
- [ ] **Step 4: Run to verify pass:** `cargo test -p overseer-daemon locks::` PASS, including the existing held test.
- [ ] **Step 5: Commit** `Record when a relay hold began`

---

### Task 3: Comment counts and build state from Bitbucket

**Files:**
- Modify: `crates/overseer-daemon/src/work/bitbucket_api.rs` (`open_prs` fields, new `commit_statuses`)
- Modify: `crates/overseer-daemon/src/work/sources.rs` (`parse_prs`, new `build_state`)
- Modify: `crates/overseer-daemon/src/work/store.rs` (`read_target`, Api branch)

**Interfaces:**
- Consumes: `PullRequest.{comments, build, head}`, `BuildState` (Task 1)
- Produces:
  - `BitbucketApi::commit_statuses(&self, token: &str, workspace: &str, slug: &str, hash: &str) -> Result<String, String>` calling `repositories/{ws}/{slug}/commit/{hash}/statuses?pagelen=100` (all pages via `all_pages`)
  - `pub fn build_state(json: &str) -> Option<BuildState>` in `sources.rs`
  - `parse_prs` fills `comments` from `comment_count` (number) and `head` from `source.commit.hash`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn build_state_rules() {
    let s = |states: &[&str]| format!(r#"{{"values":[{}]}}"#,
        states.iter().map(|x| format!(r#"{{"state":"{x}"}}"#)).collect::<Vec<_>>().join(","));
    assert_eq!(build_state(&s(&["SUCCESSFUL", "FAILED"])), Some(BuildState::Failed));
    assert_eq!(build_state(&s(&["STOPPED", "INPROGRESS"])), Some(BuildState::Failed));
    assert_eq!(build_state(&s(&["SUCCESSFUL", "INPROGRESS"])), Some(BuildState::Running));
    assert_eq!(build_state(&s(&["SUCCESSFUL", "SUCCESSFUL"])), Some(BuildState::Passed));
    assert_eq!(build_state(&s(&[])), None);
    assert_eq!(build_state("not json"), None);
}

#[test]
fn parse_prs_reads_comment_count_and_head() {
    let out = r#"{"values":[{"id":7,"title":"t","comment_count":3,
        "source":{"branch":{"name":"DEMO-1"},"commit":{"hash":"abc123"}}}]}"#;
    let pr = &parse_prs(out, "demo-api", &[]).unwrap()[0];
    assert_eq!((pr.comments, pr.head.as_deref()), (Some(3), Some("abc123")));
}
```

Update `open_prs_follows_next` to expect `fields=%2Bvalues.participants%2C%2Bvalues.comment_count`. Add `commit_statuses_reads_the_head_commit` against the test server, asserting the path `/2.0/repositories/demo-ws/demo-api/commit/abc123/statuses?pagelen=100`.

- [ ] **Step 2: Run to verify failure:** `cargo test -p overseer-daemon work::` fails.
- [ ] **Step 3: Implement.** In `read_target`'s Api branch, after `parse_prs`, for each PR with a `head`, call `commit_statuses` inside the same `run_blocking_for` closure (sequential per repo, so `PR_PARALLEL` still bounds the work); a failed statuses read logs at `warn` and leaves `build` as `None` for that PR without failing the repo. The Command branch sets `build = None` and `comments = None` (bb's JSON may hold `comment_count`; drop it to honour the bb rule).
- [ ] **Step 4: Run to verify pass:** `cargo test -p overseer-daemon work::` PASS.
- [ ] **Step 5: Commit** `Read PR comment counts and head build state from Bitbucket`

---

### Task 4: Ledger V10 `pr_seen` and `Request::PrSeen`

**Files:**
- Modify: `crates/overseer-daemon/src/ledger.rs` (V10 schema, `VERSION = 10`, steps array, new functions, tests)
- Modify: `crates/overseer-daemon/src/work/store.rs` (record after each PR poll; `mark_pr_seen`)
- Modify: `crates/overseer-daemon/src/server.rs` (`handle_request` arm for `Request::PrSeen`)

**Interfaces:**
- Consumes: `PullRequest.{comments, build, approvals}`
- Produces:
  - `pub struct PrSeen { repo: String, id: u64, comments: u32, build_failed_since: Option<i64>, changes_since: Option<i64>, comments_since: Option<i64> }` in `ledger.rs`
  - `pub(crate) fn record_prs_on(conn, prs: &[PullRequest], read_repos: &[String], now: i64) -> Result<()>`
  - `pub(crate) fn mark_pr_seen_on(conn, repo: &str, id: u64, comments: u32) -> Result<()>` (sets `comments`, clears `comments_since`)
  - `pub fn pr_seen(&self) -> Result<Vec<PrSeen>>` on `Ledger`
  - `WorkStore::mark_pr_seen(&self, repo: &str, id: u64) -> Response` (looks up the PR's current count in the cache; unknown PR or `None` count is `Response::Ok` and no change), then re-announces so attention updates

Schema:
```sql
CREATE TABLE IF NOT EXISTS pr_seen (
    repo TEXT NOT NULL, id INTEGER NOT NULL,
    comments INTEGER NOT NULL, at INTEGER NOT NULL,
    build_failed_since INTEGER, changes_since INTEGER, comments_since INTEGER,
    PRIMARY KEY (repo, id))
```

`record_prs_on` rules, per PR in `prs`:
- no row: insert with `comments = pr.comments.unwrap_or(0)` (existing comments are not new);
- `build_failed_since`: keep if `build == Failed` and already set, set `now` if `Failed` and unset, NULL otherwise;
- `changes_since`: same with `approvals.changes_requested`;
- `comments_since`: when `comments > stored`, keep or set `now`; when `comments < stored`, lower `stored` to `comments` and NULL; equal: unchanged; `None`: unchanged.
- then delete rows whose `repo` is in `read_repos` and whose `(repo, id)` is not in `prs`. Repos whose read failed are not in `read_repos`, so their rows stay.

- [ ] **Step 1: Write the failing tests** in `ledger.rs`

```rust
#[test]
fn pr_seen_first_sighting_records_without_new_comments() // insert 5 comments, comments_since None
#[test]
fn pr_seen_new_comments_keep_their_first_time()           // 5 then 7 at t=10 then 8 at t=20: since 10
#[test]
fn pr_seen_mark_clears_and_fewer_comments_lower_the_count() // mark at 8: since None; then 6: stored 6
#[test]
fn pr_seen_failed_build_and_changes_since()               // Failed at 10, Failed at 20: since 10; Passed: None
#[test]
fn pr_seen_rows_go_only_for_repos_that_were_read()        // demo-api read without PR 7: gone; demo-web not read: kept
#[test]
fn a_v9_file_upgrades_to_v10()                            // like the V6 upgrade test: user_version == VERSION, pr_seen empty
```

- [ ] **Step 2: Run to verify failure:** `cargo test -p overseer-daemon pr_seen` fails.
- [ ] **Step 3: Implement** the schema, functions, the store call (inside the same transaction that stores the polled PRs, with `read_repos` = the targets whose result was `Ok`), and the server arm `Request::PrSeen { repo, id } => store.mark_pr_seen(&repo, id)`.
- [ ] **Step 4: Run to verify pass:** `cargo test -p overseer-daemon` PASS.
- [ ] **Step 5: Commit** `Keep per-PR seen comment counts and first-seen times`

---

### Task 5: `attention::derive`

**Files:**
- Create: `crates/overseer-daemon/src/attention.rs` (register in `lib.rs`)

**Interfaces:**
- Consumes: `SlotInfo`, `LockView`, `WorkView`, `PrSeen` (Task 4), `courier::{WORKING, NO_BOX}` (Task 2), `AttentionItem` (Task 1)
- Produces:
  - `pub struct Inputs<'a> { slots: &'a [SlotInfo], locks: &'a LockView, work: &'a WorkView, seen: &'a [PrSeen], branches: &'a HashMap<SlotId, (String, String)>, author_set: bool, now: i64 }`
  - `pub fn derive(i: &Inputs) -> Vec<AttentionItem>`
  - `pub fn slot_of_pr(pr: &PullRequest, branches: &HashMap<SlotId, (String, String)>) -> Option<SlotId>` (exact `(repo, branch)` equality; the lowest slot id when several match; reused by #86)
  - `pub fn repo_branch(cwd: &Path) -> Option<(String, String)>`: walk up from `cwd` to the folder holding `.git`; if `.git` is a file, follow its `gitdir:` and then `commondir` to the main `.git`; repo = the main checkout's folder name; branch = `HEAD`'s `ref: refs/heads/<b>`; detached HEAD is `None`. Reads files only, no `git` process.
  - `pub const WAIT_AFTER: i64 = 120; pub const STUCK_AFTER: i64 = 60;`

Item rules are the spec's table. Keys: `"{kind}:{slot}"` for slot items, `"{kind}:{repo}#{id}"` for PR items. `since` for PR items comes from the matching `PrSeen` column (an item with no row or no time is not raised). `relay_stuck` uses the oldest pending held relay to the slot: raised when `held == NO_BOX` and `now - held_since > STUCK_AFTER`, or `held == WORKING` and the slot's activity is `Idle` with `now - activity.since > STUCK_AFTER`; `since = held_since`. `notice` text is `"1 notice"` / `"{n} notices"` plus the newest notice's text after `": "`. Sorted by `since`, then `key`. Text is one line (newlines replaced by spaces).

- [ ] **Step 1: Write the failing tests**, one raise and one clear per row, plus:

```rust
#[test] fn needs_you_raises_and_idle_wait_does_not()
#[test] fn waiting_needs_two_minutes_and_an_owned_item()     // 119 s: none; 121 s no item owned: none; owned: raised
#[test] fn relay_stuck_no_box_after_a_minute()
#[test] fn relay_stuck_working_while_idle_is_the_58_case()   // held WORKING, activity Working: none; Idle 61 s: raised
#[test] fn notices_are_one_item_per_slot_from_the_oldest()
#[test] fn pr_items_need_author_or_a_matching_slot_branch()  // author_set false: only the branch-matched PR
#[test] fn pr_items_take_since_from_pr_seen_and_name_the_slot()
#[test] fn bb_prs_with_no_counts_raise_nothing()
#[test] fn items_sort_oldest_first_and_future_times_do_not_panic()
#[test] fn repo_branch_reads_a_checkout_a_worktree_and_a_subfolder() // tempdir fixtures; detached HEAD gives None
```

- [ ] **Step 2: Run to verify failure:** `cargo test -p overseer-daemon attention` fails.
- [ ] **Step 3: Implement** `derive`, `slot_of_pr`, `repo_branch`.
- [ ] **Step 4: Run to verify pass:** `cargo test -p overseer-daemon attention` PASS.
- [ ] **Step 5: Commit** `Derive the attention list from daemon state`

---

### Task 6: Daemon attention task and `Request::Attention`

**Files:**
- Modify: `crates/overseer-daemon/src/attention.rs` (add `AttentionService`)
- Modify: `crates/overseer-daemon/src/server.rs` (spawn next to `watch_session_logs` around line 419; `Ctx` gains `attention: &'a Arc<AttentionService>`; `handle_request` arm)

**Interfaces:**
- Consumes: `derive`, `repo_branch`, `Ledger::pr_seen`, `manager.list()`, `locks.view()`, `store.view()` (the existing work view accessor), `cfg.settings.pr_author`
- Produces:
  - `pub struct AttentionService` with `pub fn current(&self) -> AttentionView` and `pub fn spawn(ledger, manager, locks, store, events: broadcast::Sender<Event>, author_set: bool) -> Arc<AttentionService>`
  - `fn update(&self, items: Vec<AttentionItem>) -> bool` (stores and returns true only when different from the last list; the task sends `Event::AttentionChanged` on true)
  - `Request::Attention => Response::Attention(attention.current())`

The task subscribes to the event broadcast, keeps the latest slots, lock view and work view from the events (seeded from the services at start), and re-derives on each of those three events and on a `tokio::time::interval(30 s)` tick. It ignores its own `AttentionChanged`. On `Lagged`, it re-reads all three from the services. Branches are recomputed from each slot's `cwd` on every derive. `author_set` is read from settings at derive time if settings can change at runtime (check `SettingsCtx`); otherwise at spawn.

- [ ] **Step 1: Write the failing tests:** `update_reports_only_changes` (same list twice: true then false; changed `since`: true), and a server-level test using the existing test daemon harness: a slot with a `NeedsYou` activity makes `Request::Attention` return one `needs_you` item, and an `AttentionChanged` event arrives.
- [ ] **Step 2: Run to verify failure:** `cargo test -p overseer-daemon attention` fails.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify pass:** `cargo test -p overseer-daemon` PASS.
- [ ] **Step 5: Commit** `Send the attention list when it changes`

---

### Task 7: `overseer ctl attention`

**Files:**
- Modify: `crates/overseer/src/ctl.rs` (`CtlCmd::Attention { json: bool }`, handler, formatter, tests)

**Interfaces:**
- Consumes: `Request::Attention`, `Response::Attention(AttentionView)`
- Produces: `fn attention_lines(v: &AttentionView, now: i64) -> Vec<String>` and `pub fn age_text(secs: i64) -> String` in `overseer-proto` (`msg.rs`, shared with the GUI): `"0m"` under a minute and for negative input, `"{m}m"` under an hour, `"{h}h"` under a day, `"{d}d"` beyond.

Line format: `{age:>4}  {who}  {text}` where `who` is `S{n}` for slot items, `{repo}#{id}` for PR items (with ` S{n}` appended when the PR has a slot). Empty list prints `Nothing waiting on you.`. `--json` prints the view as JSON.

- [ ] **Step 1: Write the failing tests:** `age_text_buckets` (`-5 -> "0m"`, `59 -> "0m"`, `61 -> "1m"`, `3600 -> "1h"`, `90000 -> "1d"`), `attention_lines_format` with one slot item and one PR item with a slot, and the empty message.
- [ ] **Step 2: Run to verify failure:** `cargo test -p overseer attention` fails.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify pass:** `cargo test -p overseer` PASS.
- [ ] **Step 5: Commit** `Add ctl attention`

---

### Task 8: Attention panel, Dock badge and PR seen in the app

**Files:**
- Modify: `crates/overseer-gui/src/app.rs` (handle `AttentionChanged`, request `Attention` on connect and reconnect, open slot or PR on click, send `PrSeen`)
- Create: `crates/overseer-gui/src/attention_view.rs` (panel rendering and its pure row model)
- Modify: `crates/overseer-gui/src/board.rs` (place the panel at the top of the board)
- Modify: `crates/overseer-gui/src/notify.rs` (add `set_dock_badge(count: usize)`, a no-op outside an app bundle, using the same bundle check as notifications)
- Modify: the existing PR panel click (`sprint_view.rs` or `session.rs`, wherever a PR's URL is opened) to also send `Request::PrSeen`

**Interfaces:**
- Consumes: `AttentionView`, `age_text`, `Request::{Attention, PrSeen}`
- Produces: `pub fn rows(v: &AttentionView, now: i64) -> Vec<Row>` with `Row { key, who, text, age }`; `pub fn badge_label(count: usize) -> Option<String>` (`None` at 0, else the number)

Panel: collapsed to one line `Nothing waiting` when empty; otherwise a header `WAITING ON YOU · {n}` and one row per item, oldest first, with who, text and age. Click on a slot item focuses that slot the same way clicking its board card does. Click on a PR item opens `url` in the browser and sends `PrSeen`. The badge is set with `NSApp.dockTile.setBadgeLabel:` (nil at zero) on the main thread whenever the view changes. Styling follows `theme.rs`.

- [ ] **Step 1: Write the failing tests:** `rows_show_slot_and_pr_items_oldest_first`, `badge_label_is_empty_at_zero` (`0 -> None`, `3 -> Some("3")`).
- [ ] **Step 2: Run to verify failure:** `cargo test -p overseer-gui attention` fails.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify pass:** `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings` PASS.
- [ ] **Step 5: Commit** `Show the attention list on the board and the Dock`
- [ ] **Step 6: Note in the README's feature list** one bullet for the attention list and `ctl attention`, and commit it with this task.

---

## Final verification

- `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
- A search for U+2014 (em dash) in `crates`, `docs/superpowers` and `README.md` finds nothing new.
- Not covered by tests: the real Bitbucket `comment_count` and statuses responses, and the Dock badge in a bundled app. Both are checked after install.
