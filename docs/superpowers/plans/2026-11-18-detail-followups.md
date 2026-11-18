# Item Detail Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the #22 review leftovers (issue #48): the detail view keeps good text through failed refreshes, reads real acli and bd output, refuses odd keys, shares fetches, answers requests it cannot decode, follows live fields in the GUI, words build mismatches by direction, and S0 reads tickets in full before it plans.

**Architecture:** Pure parsing helpers land in `detail.rs` first; `WorkStore::item_detail` gains key validation, a stale-copy fallback, pruning and a single-flight map; `read_frame` reports an undecodable request's `seq` so the server answers it and keeps the connection; `build.rs` gains a build comparison the GUI uses for wording and for deciding when to send; the GUI's pure detail functions in `session.rs` take the new rules, with `app.rs` only wiring state; the coordinator role and its upgrade hash change last, with the full gate.

**Tech Stack:** Rust workspace (overseer-proto, overseer-daemon, overseer, overseer-gui), serde_json, ureq, tokio (`sync::watch` for single-flight), gpui.

**Spec:** docs/superpowers/specs/2026-11-18-detail-followups-design.md (follows docs/superpowers/plans/2026-11-18-ticket-detail.md)

## Global Constraints

- No em dashes (U+2014) in code, comments, docs or commit messages.
- Invented names only (the repo is public): DEMO-n, demo-n, demo.atlassian.net, demo@example.test, demo-ws, "Demo User". Fixtures built from captured output are rewritten to these.
- Source commands go through `run_source`.
- Never run the real `security` binary against the user's Keychain in tests. Never run writing `bd` commands against ~/Repos/2ndBrain.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; no emojis; plain human-style subjects like `git log -10` shows ("Harden the detail view against old daemons and stale replies"); run `git log -5` before each commit. Task bodies say `Part of #48`; the last commit says `Fixes #48`.
- Key rule: `^[A-Za-z0-9][A-Za-z0-9._-]*$`; error `{key} is not a valid item key`.
- acli not-found: stderr containing `Issue does not exist` gives `{key} was not found in Jira` (the REST wording).
- Body preview: 40 lines (`BODY_PREVIEW_LINES`); Show all / Show less only when the full body is longer than that.
- Bullet indent: `8 + 12 * depth` px.
- Build ids are `<commit>-<yyyymmddHHMM>`; `DEV_BUILD` on either side counts as a match.
- Gate before each commit: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the tests of the crates touched. The known flaky `a_bd_sync_owed_at_a_restart_still_runs` (issue #43) may fail; rerun it alone before treating it as a regression.

## Review Focus

1. The leading fetch of a shared detail is dropped (its connection closes or its request times out) while a second open waits on it: the second must not hang, it fetches for itself. (Task 2 test `a_dropped_leader_does_not_strand_the_follower`.)
2. Refresh pressed in the GUI and the reply never comes back well (daemon lost, transport error): the loaded text stays, with the error as the orange notice. (Task 4 test `a_failed_refresh_keeps_the_text`.)
3. A body of exactly 40 lines offers no toggle; 41 lines offers Show all, and Show less after it. (Task 4 test `show_all_only_past_forty_lines`.)
4. The build probe times out or the app reconnects while a click waits: the click is not stuck on Loading; an unknown daemon after the probe is sent the request as today. (Task 4 test `detail_waits_for_the_build_probe`, plus `daemon_known` always clearing the wait.)
5. Build ids whose commit part has hyphens, an 11-digit suffix, or equal stamps on different commits: compared by the last `-` suffix only, anything unparseable is the neutral wording. (Task 3 test `compare_builds_reads_the_timestamp_suffix`.)

## File Structure

- `crates/overseer-daemon/src/work/detail.rs`: parsing and the two new pure helpers (`valid_item_key`, `acli_not_found`).
- `crates/overseer-daemon/tests/fixtures/acli-view.json` (new), `bd-show.json` (rewritten to the real bd 1.0.2 shape), `roles-before-detail/coordinator.md` (new).
- `crates/overseer-daemon/src/work/store.rs`: fetch policy (validation, stale copy, overview, pruning, single-flight).
- `crates/overseer-daemon/src/work/jira_api.rs`: path encoding.
- `crates/overseer-proto/src/frame.rs`, `lib.rs`: undecodable request reporting. `crates/overseer-daemon/src/server.rs`: answering it.
- `crates/overseer-proto/src/build.rs`: build comparison and wording.
- `crates/overseer-gui/src/session.rs`: detail model rules; `app.rs`: refresh and probe wiring.
- `crates/overseer-daemon/src/roles/coordinator.md`, `launch.rs`: S0 wording and upgrade hash.
- `crates/overseer/tests/mcp.rs`: successful round trip. `README.md`: the panel and S0 lines.

---

### Task 1: Real-shape parsing and pure helpers

**Files:**
- Modify: `crates/overseer-daemon/src/work/detail.rs` (`block` list arm at 57-82, dependency parse at 183-200, tests at 239)
- Create: `crates/overseer-daemon/tests/fixtures/acli-view.json`
- Modify: `crates/overseer-daemon/tests/fixtures/bd-show.json`

**Interfaces:**
- Produces:
  - `pub fn valid_item_key(key: &str) -> bool`: true when `key` matches `^[A-Za-z0-9][A-Za-z0-9._-]*$` (ASCII only; no regex needed).
  - `pub fn acli_not_found(key: &str, err: &str) -> Option<String>`: `Some(format!("{key} was not found in Jira"))` when `err` contains `Issue does not exist`, else `None`.
  - `parse_bead_detail` reads a dependency's type from `dependency_type`, falling back to `type`; its id from `depends_on_id`, falling back to `id`; plain strings and objects without a type still give the bare id.
  - `adf_to_text`: a `listItem` whose first child is not a `paragraph` writes no marker; its children render at `depth + 1`. An empty `listItem` writes nothing.

- [ ] **Step 1: Write the fixtures.**
  - `bd-show.json` becomes the real bd 1.0.2 shape: an array of one object with `id` `demo-1`, `title` `Wire the detail view`, `description` `Show the item body.\n\nSecond paragraph.`, `notes` with the same 12 `note line N` lines, `status` `in_progress`, `priority` 1, `issue_type` `task`, `owner` `demo@example.test`, `created_at`/`updated_at` ISO times, `created_by` `Demo User`, and `dependencies: [{"id": "demo-2", "title": "Write the fixtures", "status": "open", "priority": 2, "issue_type": "task", "owner": "demo@example.test", "created_at": ..., "created_by": "Demo User", "updated_at": ..., "dependency_type": "blocks"}]`.
  - `acli-view.json` is the acli 1.3.41 view shape for `DEMO-7`: top-level `expand` (string), `id` `"10007"`, `key`, `self` `https://demo.atlassian.net/rest/api/3/issue/10007`, `fields`, and `null` for `changelog`, `editmeta`, `fieldsToInclude`, `names`, `operations`, `properties`, `renderedFields`, `schema`, `transitions`, `versionedRepresentations`. `fields.summary` `Rotate the demo widget keys`; `fields.status` the full object (`description`, `iconUrl`, `id`, `name` `In Progress`, `self`, `statusCategory` `{colorName: "yellow", id: 4, key: "indeterminate", name: "In Progress", self}`); `fields.priority` `{iconUrl, id: "3", name: "Medium", self}`; `fields.description` ADF `{type: "doc", version: 1, content: [...]}` holding, in order:
    1. a paragraph: text `Rotate the keys for `, text `demo-ws` with mark `code`, text ` before `, text `Friday` with mark `strong`, text `. See `, an `inlineCard` with `attrs.url` `https://demo.atlassian.net/browse/DEMO-8`;
    2. a heading level 3: `Acceptance criteria`;
    3. a bulletList: `Old keys stop working`; text `New keys are in the ` then `vault` with a `link` mark (`attrs.href` `https://demo.atlassian.net/wiki`);
    4. an orderedList: `Make the keys`; text `Ship them`, a `hardBreak`, text `then check`;
    5. a codeBlock: `demo rotate --all`.

- [ ] **Step 2: Write the failing tests** in `detail.rs`'s test module:
  - `acli_view_fixture_reads_like_jira`: `parse_issue_detail(ACLI_VIEW)` gives title `Rotate the demo widget keys`, status `In Progress`, priority `Some("Medium")`, and body exactly
    `"Rotate the keys for demo-ws before Friday. See https://demo.atlassian.net/browse/DEMO-8\n\n### Acceptance criteria\n- Old keys stop working\n- New keys are in the vault\n1. Make the keys\n2. Ship them\nthen check\n```\ndemo rotate --all\n```"`.
  - `ordered_lists_number_and_nest`: an orderedList of three paragraphs gives `1. a\n2. b\n3. c`; a bulletList item holding a paragraph `x` and a nested orderedList `y` gives `- x\n  1. y`.
  - `deep_headings_keep_their_level`: headings at levels 4, 5, 6 give `#### Four`, `##### Five`, `###### Six`; level 9 clamps to `######`; a missing level gives `#`.
  - `a_list_item_without_a_paragraph_has_no_lone_marker`: an item whose only child is a bulletList with item `inner` gives `  - inner` (no `- ` line before it); an empty `listItem` beside an item `kept` gives `- kept` only.
  - `bead_dependencies_in_every_shape`: the rewritten fixture still gives `["demo-2 (blocks)"]`; `{"dependencies": ["demo-3", {"id": "demo-4"}, {"depends_on_id": "demo-5", "type": "related"}]}` gives `["demo-3", "demo-4", "demo-5 (related)"]`.
  - `acli_not_found_maps_to_the_rest_wording`: the error `acli exited with 1: ✗ Error: Issue does not exist or you do not have permission to see it.` for `DEMO-1` gives `Some("DEMO-1 was not found in Jira")`; `acli exited with 1: timeout` gives `None`.
  - `item_keys_are_checked`: `DEMO-1`, `demo-12`, `ov-3.1`, `a_b` are valid; `""`, `-DEMO`, `.x`, `DEMO 1`, `DEMO-1;rm`, `DEMO/1`, `ÉDEMO`, `DEMO-1\n` are not.
  - The existing `bead_detail_takes_notes_tail_and_deps` keeps passing on the rewritten fixture unchanged.

- [ ] **Step 3: Run them to see them fail.** `cargo test -p overseer-daemon --lib work::detail` Expected: compile errors for `valid_item_key`/`acli_not_found`, then assertion failures on the dependency type and the lone marker.

- [ ] **Step 4: Implement** the two helpers and the two parser fixes in `detail.rs`.

- [ ] **Step 5: Run the gate.** fmt, clippy, `cargo test -p overseer-daemon --lib work::`. Expected: all pass (the store tests read `bd-show.json` and must still pass on the new shape).

- [ ] **Step 6: Commit** `Read the real acli and bd detail shapes` (body: `Part of #48`).

### Task 2: Store fetch policy and Jira path encoding

**Files:**
- Modify: `crates/overseer-daemon/src/work/store.rs` (`WorkStore` fields at ~345, `Fetched` at ~363, `item_detail` at 960-1062, `fetch_detail` acli arm at ~1088-1099, tests at ~3203)
- Modify: `crates/overseer-daemon/src/work/jira_api.rs` (`issue` at 168, tests at ~418)

**Interfaces:**
- Consumes: Task 1 `valid_item_key`, `acli_not_found`, the `acli-view.json` fixture.
- Produces:
  - `fn path_segment(s: &str) -> String` in jira_api.rs: percent-encodes every byte outside `A-Za-z0-9-._~` as `%XX` (upper hex); `issue` builds its URL with `path_segment(key)`.
  - `#[derive(Clone, Default)] struct Fetched` (Default for tests).
  - `detail_inflight: Mutex<HashMap<String, InFlight>>` on `WorkStore`, with `struct InFlight { refresh: bool, id: u64, rx: watch::Receiver<Option<Result<(Fetched, i64), String>>> }`.
  - `async fn fetch_shared(&self, key: &str, kind: &ItemKind, refresh: bool) -> Result<(Fetched, i64), String>`: joins an in-flight fetch for `key` when one exists and (`!refresh` or that fetch is a refresh); otherwise leads: registers an `InFlight`, runs `fetch_detail`, on success inserts into `detail_cache` (first removing every entry older than `DETAIL_TTL`), sends the result, and removes its own entry (matched by `id`) through a drop guard so a cancelled leader still unregisters. A follower whose sender is dropped without a value runs `fetch_shared` again.

Behaviour of `item_detail(key, refresh)`, in order:
1. `!valid_item_key(key)` gives `Err("{key} is not a valid item key")` before the item lookup and before anything runs.
2. Unknown key: `Err` as today.
3. A cache hit younger than `DETAIL_TTL` without `refresh` is used. Otherwise `fetch_shared`.
4. On fetch failure with any cache entry for `key` (expired or bypassed by `refresh`): reply with that entry's body, notes, dependencies and title/status/priority fill-ins, its original `fetched_at`, and `error` set to the failure. Without an entry: error, no `fetched_at`, as today.
5. After that, whenever the final `body` is `None` or blank, fill `overview` from the folder (today's code, moved out of the error arm).
- acli arm of `fetch_detail`: map the `run_source` error through `acli_not_found(key, &e)` first; only other errors get the command-line prefix and `missing_program_hint`.

- [ ] **Step 1: Write the failing tests.**
  - jira_api.rs `issue_path_encodes_the_key`: `path_segment("DEMO-1.a_b~") == "DEMO-1.a_b~"`, `path_segment("DEMO 1/x?y") == "DEMO%201%2Fx%3Fy"`, `path_segment("é") == "%C3%A9"`; the existing `issue_reads_one_ticket_and_maps_404` path stays `/ex/jira/demo-cloud/rest/api/3/issue/DEMO-1?fields=summary,status,priority,description`.
  - store.rs `a_bad_key_is_refused_before_any_fetch`: a logging fake `bd_command`; `item_detail("demo-1;rm", false)` is `Err("demo-1;rm is not a valid item key")` and the log file does not exist.
  - store.rs `a_failed_refresh_keeps_the_cached_copy`: fake bd succeeding on call 1 and failing with stderr `boom` after (counter file, like `a_failed_fetch_is_not_cached`); call 1 (`false`) has a body and `fetched_at = Some(t)`; call 2 (`true`) has the same body, notes and `depends_on`, `fetched_at == Some(t)`, and `error` containing `boom`.
  - store.rs `an_empty_body_brings_the_overview`: an acli fake printing `{"key":"DEMO-1","fields":{"summary":"S","description":null}}`, DEMO-1 owned with a folder whose `00-PROJECT-OVERVIEW.md` has `Widgets glow when wired.`; the reply has `error: None`, `body: None`, `overview == Some("Widgets glow when wired.")`, `fetched_at` set.
  - store.rs `acli_not_found_reads_like_rest`: an acli fake that writes `✗ Error: Issue does not exist or you do not have permission to see it.` to stderr and exits 1; `error == Some("DEMO-1 was not found in Jira")`.
  - store.rs `detail_for_a_command_ticket_uses_acli_view` switches its fake to `acli-view.json` and asserts title `Rotate the demo widget keys`, status `In Progress`, priority `Some("Medium")`.
  - store.rs `expired_entries_are_pruned_on_insert`: insert `demo-9` into `detail_cache` with `Instant::now().checked_sub(DETAIL_TTL + Duration::from_secs(1))` and `Fetched::default()`; fetch `demo-1`; the cache keys are exactly `["demo-1"]`.
  - store.rs `two_opens_share_one_fetch`: fake bd `sleep 0.3`, then logs and prints the fixture; `tokio::join!` of two `item_detail("demo-1", false)` gives equal bodies and one log line.
  - store.rs `a_refresh_does_not_join_a_plain_fetch`: join a plain call with one that sleeps 50 ms then calls with `refresh: true`; two log lines.
  - store.rs `a_dropped_leader_does_not_strand_the_follower`: join `tokio::time::timeout(50 ms, item_detail("demo-1", false))` with a call that sleeps 10 ms first; the timeout is `Err`, the follower returns a body, and `detail_inflight` is empty afterwards.

- [ ] **Step 2: Run them to see them fail.** `cargo test -p overseer-daemon --lib work::` Expected: missing `path_segment`, `detail_inflight`, `Fetched: Default`; then assertion failures.

- [ ] **Step 3: Implement** `path_segment`, the `item_detail` order above, `fetch_shared` with the drop guard, pruning, and the acli error mapping.

- [ ] **Step 4: Run the gate.** fmt, clippy, `cargo test -p overseer-daemon`. Expected: pass.

- [ ] **Step 5: Commit** `Keep the last good detail when a refresh fails, and share fetches` (body: `Part of #48`).

### Task 3: Undecodable requests and build comparison

**Files:**
- Modify: `crates/overseer-proto/src/frame.rs` (`decode` request arm at 77-83), `crates/overseer-proto/src/lib.rs` (re-exports at 10-15)
- Modify: `crates/overseer-daemon/src/server.rs` (reader task at 652-667)
- Modify: `crates/overseer-proto/src/build.rs` (`build_notice` at 43, tests at 104)
- Test: `crates/overseer-proto/tests/frame_roundtrip.rs`, `crates/overseer-daemon/tests/server.rs`

**Interfaces:**
- Produces (proto):
  - `#[derive(Debug)] pub struct UndecodableRequest { pub seq: u32, pub why: String }` implementing `Display` and `std::error::Error`.
  - `decode` for a request frame that fails to decode: parse the payload as `serde_json::Value`; when `seq` is a `u32`, return `io::Error::new(InvalidData, UndecodableRequest { seq, why })`; otherwise the plain invalid-data error as today.
  - `pub fn undecodable_request(e: &io::Error) -> Option<u32>` (downcasts `get_ref()`), re-exported from lib.rs.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum BuildOrder { Match, DaemonOlder, DaemonNewer, Different }`
  - `pub fn compare_builds(app: &str, daemon: &str) -> BuildOrder`: `Match` when equal or either is `DEV_BUILD`; else the part after the last `-` of each, when exactly 12 ASCII digits, compared as numbers (`DaemonOlder` / `DaemonNewer`); `Different` when either is unparseable or the stamps are equal.
  - `build_notice(app, daemon)` keeps its signature, returns `None` for `Match`, and words the rest:
    - `DaemonOlder`: `The daemon runs build {daemon}, older than this app's build {app}. Restart the daemon to finish the upgrade (overseer ctl restart, or Settings).`
    - `DaemonNewer`: `The daemon runs build {daemon}, newer than this app's build {app}. Restart Overseer (quit and reopen the app) to finish the upgrade.`
    - `Different`: `The app and the daemon are different builds ({app} and {daemon}); restart both.`
  - Both re-exported from lib.rs.
- Produces (daemon): `const UNKNOWN_REQUEST: &str = "the daemon does not understand this request; restart the daemon to finish an upgrade";` in server.rs. The reader task holds an `Outbox` clone; on an error where `undecodable_request(&e)` is `Some(seq)` it sends `Frame::Response { seq, resp: Response::Error { message: UNKNOWN_REQUEST.into() } }`, logs at debug, and keeps reading. Any other error still ends the loop.

- [ ] **Step 1: Write the failing tests.**
  - frame_roundtrip.rs `an_undecodable_request_keeps_its_seq`: a raw frame of kind 0 with payload `{"seq":7,"req":{"type":"NoSuchRequest"}}` gives an `InvalidData` error with `undecodable_request(&err) == Some(7)`; payloads `{"req":{"type":"NoSuchRequest"}}`, `{"seq":8589934592,"req":{}}` and `not json` give `None`.
  - server.rs `an_unknown_request_is_answered_and_the_connection_stays_open`: add `Client::send_raw(&mut self, kind: u8, payload: &[u8])` (u32 BE length of kind plus payload, then kind, then payload); send the seq-7 payload above; the next response frame is seq 7 with `Response::Error` whose message is `UNKNOWN_REQUEST`'s text; then `request(Request::ListSlots)` on the same client still answers.
  - server.rs `a_frame_without_a_seq_still_closes_the_connection`: `send_raw(0, br#"{"req":{"type":"NoSuchRequest"}}"#)`; `recv()` gives `None`.
  - build.rs `compare_builds_reads_the_timestamp_suffix`: (`abc1234-202611141000`, `abc1234-202611140930`) is `DaemonOlder`; swapped is `DaemonNewer`; equal is `Match`; (`dev`, `abc1234-202611140930`) and swapped are `Match`; (`b2`, `b1`) is `Different`; (`feat-x-abc1234-202611141000`, `feat-x-abc1234-202611140930`) is `DaemonOlder`; (`a-20261114093`, `b-202611140930`) is `Different`; (`a-202611140930`, `b-202611140930`) is `Different`.
  - build.rs `notice_only_when_both_known_and_different` is rewritten as `notice_names_who_to_restart`: the three exact strings above for (`abc1234-202611141000`, `abc1234-202611140930`), (`abc1234-202611140930`, `abc1234-202611141000`) and (`b2`, `b1`), and `None` for a match and for `dev`.

- [ ] **Step 2: Run them to see them fail.** `cargo test -p overseer-proto` and `cargo test -p overseer-daemon --test server -- unknown_request frame_without` Expected: missing items; the server test sees the connection close.

- [ ] **Step 3: Implement** the frame error, the reader's answer, `compare_builds` and the new `build_notice` wording. Check `crates/overseer/src/ctl.rs` and `crates/overseer-gui/src/settings_view.rs` tests that compare against `build_notice(...)` still pass (they call the function, so they should).

- [ ] **Step 4: Run the gate.** fmt, clippy, `cargo test -p overseer-proto -p overseer-daemon -p overseer`. Expected: pass.

- [ ] **Step 5: Commit** `Answer requests the daemon cannot decode, and say which side to restart` (body: `Part of #48`).

### Task 4: GUI detail rules

**Files:**
- Modify: `crates/overseer-gui/src/session.rs` (`owns_rows` at 219, `OpenDetail` at 263, `DetailModel` at 276, `body_lines` at 303, `detail_model` at 357-447, `detail_unavailable` at 461, `take_reply` at 477, `render_detail` at 1118, tests at ~2600)
- Modify: `crates/overseer-gui/src/app.rs` (fields at ~481, `probe_daemon` at 656, `daemon_known` at 672, `reconnected` at 740, `open_detail` at 985-1036)

**Interfaces:**
- Consumes: Task 3 `compare_builds`, `build_notice`.
- Produces:
  - `OpenDetail` gains `pub refreshing: bool`.
  - `pub fn reopen(prev: Option<&OpenDetail>, slot: SlotId, key: &str, req: u64, refresh: bool) -> OpenDetail`: keeps `show_all` for the same slot and key; with `refresh` and `prev` `Loaded` for the same slot and key, keeps that state and sets `refreshing`; otherwise `Loading`, `refreshing: false`.
  - `take_reply` (same signature): `Ok(d)` gives `Loaded(d)`; `Err(e)` while `Loaded(prev)` gives `Loaded(prev)` with `error = Some(e)`; otherwise `Failed(e)`. Both clear `refreshing`. The `req` check stays.
  - `DetailModel` gains `pub expandable: bool` (the full body has more than `BODY_PREVIEW_LINES` lines).
  - `pub fn bullet_indent(depth: usize) -> f32` returning `8. + 12. * depth as f32`.
  - `pub enum DetailGate { Send, Wait, Refuse(String) }` and `pub fn detail_gate(daemon: &DaemonKnown, probing: bool, app_build: &str) -> DetailGate`, replacing `detail_unavailable`: `Unknown` while `probing` is `Wait`; `Unknown` otherwise is `Send`; `Old` is `Refuse` with today's text; `Info` is `Send` on `BuildOrder::Match`, else `Refuse(format!("Cannot fetch details: {}", build_notice(app_build, &info.build).unwrap()))`.
  - `OverseerApp` gains `daemon_probing: bool` (set in `probe_daemon`, cleared in `daemon_known` and on the early return) and `detail_waiting: Option<bool>` (the `refresh` of a click held for the probe), plus `fn send_detail(&mut self, refresh: bool, cx: &mut Context<Self>)` holding today's spawn. `open_detail` builds state with `reopen`, then by `detail_gate`: `Send` calls `send_detail`, `Wait` sets `detail_waiting`, `Refuse` sets `Failed`. `daemon_known` takes `detail_waiting` and runs the gate again. `reconnected` sets `daemon_probing` through `probe_daemon`.

`detail_model` rules:
- Title: an empty or blank title shows the key (Loaded and Loading/Failed). `owns_rows` does the same for `OwnsRow::title`.
- Loading and Failed: also `children` from the view (beads whose `parent` is the key, as `{key} {title} ({status})`) and PR rows newest `created` first, undated last, through one helper shared with the live path.
- Loaded with the item in the view and `in_list`: Status, Stage (with `stage_since`) and PR rows come from the view item; Priority from the view item, falling back to the reply's when the view has none. Body, notes, dependencies, folder and children stay from the reply. Not listed: all from the reply, as today.
- `refreshing`: the Fetched row reads `Refreshing...` (added even when `fetched_at` is `None`); `loading` stays false.
- `body_lines` drops bullets whose text is empty after trimming, and a line that is only `-`.

- [ ] **Step 1: Write the failing tests** in session.rs:
  - `refresh_keeps_the_loaded_text`: `reopen` of a `Loaded` DEMO-1 with body `Kept body` and `refresh: true` stays `Loaded` with `refreshing: true`; its model has body `[Text("Kept body")]`, row `("Fetched", "Refreshing...")`, `loading: false`. `reopen` with `refresh: false` is `Loading`.
  - `a_failed_refresh_keeps_the_text`: `take_reply(.., Err("lost"))` on that refreshing state gives `Loaded` with the body kept, `notice == Some("lost")`, `refreshing: false`.
  - `loading_and_failed_show_child_beads_and_prs_newest_first`: a view with bead `ov-1` (`parent` DEMO-1, title `First step`, status `open`) and PRs 1 (undated), 2 (100), 4 (200) for DEMO-1; for both `Loading` and `Failed("x")`, `children == ["ov-1 First step (open)"]` and the PR rows are for 4, 2, 1 in that order.
  - `live_fields_follow_polls_while_listed`: a reply with status `To Do`, priority `None`, body `B`; the listed view item has status `In Review`, priority `High`, `Stage::InProgress` since 40, and PR 5; rows show `In Review`, `High`, a Stage row, PR 5; the body is `B`. With `in_list: false` the Status row is `To Do` and there is no PR 5 row.
  - `show_all_only_past_forty_lines`: 40 body lines give `expandable: false, truncated: false`; 41 give `true, true`; 41 with `show_all` give `true, false`.
  - `an_empty_title_shows_the_key`: a `Loaded` reply with title `"  "` gives title `DEMO-1`; `Loading` with an item titled `""` gives `DEMO-1`; `owns_rows` for that item gives title `DEMO-1`.
  - `empty_bullets_are_dropped`: `body_lines("- \n-\n- Real\n  - ")` is `[Bullet { depth: 0, text: "Real" }]`.
  - `bullet_indent_grows_twelve_per_level`: `bullet_indent(0) == 8.`, `bullet_indent(2) == 32.`
  - `detail_waits_for_the_build_probe` (replaces `an_older_daemon_is_not_sent_the_detail_request`): `(Unknown, true)` is `Wait`; `(Unknown, false)` is `Send`; `Old` refuses with `Restart the daemon`; app `def5678-202611141000` with daemon `abc1234-202611140930` refuses with `Restart the daemon`; the reverse refuses with `Restart Overseer`; `b2` with `b1` refuses with `restart both`; daemon `dev` and an equal build are `Send`.
- [ ] **Step 2: Run them to see them fail.** `cargo test -p overseer-gui session::` Expected: missing `reopen`, `refreshing`, `expandable`, `bullet_indent`, `detail_gate`.
- [ ] **Step 3: Implement** the session.rs functions and rules above.
- [ ] **Step 4: Wire `app.rs` and `render_detail`.** `open_detail`, `send_detail`, the probe fields as in Interfaces; `render_detail` indents bullets with `bullet_indent(depth)` and shows the toggle only when `m.expandable`.
- [ ] **Step 5: Run the gate.** fmt, clippy, `cargo test -p overseer-gui`. Expected: pass.
- [ ] **Step 6: Check by hand.** Build and run the GUI against a dev daemon (not the installed app). Open an owned item, press Refresh: the text stays and Fetched reads `Refreshing...` until the reply. Open a detail right after launch: it waits briefly, then loads. A body under 41 lines has no toggle; nested bullets sit right of their parents.
- [ ] **Step 7: Commit** `Keep the detail text through a refresh and follow live fields` (body: `Part of #48`).

### Task 5: Coordinator reads tickets, MCP round trip, docs and final gate

**Files:**
- Create: `crates/overseer-daemon/tests/fixtures/roles-before-detail/coordinator.md`
- Modify: `crates/overseer-daemon/src/roles/coordinator.md`, `crates/overseer-daemon/src/launch.rs` (`PREVIOUS_COORDINATOR` at 25-35, tests near 1151 and 1173)
- Test: `crates/overseer/tests/mcp.rs`
- Modify: `README.md` (session panel paragraph at ~464-474; S0 line at ~11)

**Interfaces:**
- Consumes: Task 1 `bd-show.json`; Task 2 behaviour; the existing `get_item_detail` tool.
- Produces: `PREVIOUS_COORDINATOR` gains `0x7757a9a9bae5e1c2, // before S0 read tickets in full`; `TempDaemon::start_with(tag: &str, edit: impl FnOnce(&mut overseer_daemon::Config)) -> TempDaemon` in tests/mcp.rs (`start` calls it with no edit).

- [ ] **Step 1: Save the fixture first.** `git show HEAD:crates/overseer-daemon/src/roles/coordinator.md > crates/overseer-daemon/tests/fixtures/roles-before-detail/coordinator.md` (byte-identical, before any edit).
- [ ] **Step 2: Write the failing tests.**
  - launch.rs `roles_from_before_the_detail_reads_upgrade`: `text_hash(BEFORE_DETAIL_COORDINATOR) == 0x7757a9a9bae5e1c2`, `PREVIOUS_COORDINATOR` contains it, and `ensure_template` over that file in a temp `roles` folder leaves `COORDINATOR_ROLE` with no `.new` beside it (as `phase5_roles_are_previous_defaults` does).
  - launch.rs `coordinator_role_reads_tickets_through_overseer`: `COORDINATOR_ROLE` contains `get_item_detail` and `acceptance criteria`, and not `Do not read Jira yourself`. Add `get_item_detail` to `coordinator_role_names_the_board_tools`.
  - tests/mcp.rs `get_item_detail_returns_a_bead`: `start_with` sets `beads_dir` to `home/beads` (create it and `home/beads/.beads`) and `bd_command` to an executable `home/bd` stub: `case "$1" in show) cat <bd-show.json>;; *) echo '[{"id":"demo-1","title":"Wire the detail view","status":"open","priority":1}]';; esac`, with the fixture copied from `include_str!("../../overseer-daemon/tests/fixtures/bd-show.json")`. S0 calls `get_item_detail` `{"key": "demo-1"}` until the text no longer contains `demo-1 is not a known item` (the beads poll has not run yet; up to 10 s, 100 ms apart); the final call has `isError: false` and text containing `demo-1 Wire the detail view`, `Show the item body.` and `Depends on: demo-2 (blocks)`.
- [ ] **Step 3: Run them to see them fail.** `cargo test -p overseer-daemon --lib launch` and `cargo test -p overseer --test mcp get_item_detail_returns_a_bead` Expected: the two role tests fail. The MCP test may already pass, since it pins Task 1 and 2 behaviour; keep it either way.
- [ ] **Step 4: Rewrite `coordinator.md`.** In step 1 replace `Do not read Jira yourself.` with `Read tickets only through \`get_item_detail\`, never by calling Jira or acli directly.` Insert a new step 5 and renumber the propose step to 6:
  `5. Before you propose work, read every candidate ticket and bead in full with \`get_item_detail\`: the description with its acceptance criteria, the notes and the dependencies. Decide who gets what from what the work actually is, not from its title. If a detail comes back with an error, say so in your plan and work from what it did return.`
  Add the hash to `PREVIOUS_COORDINATOR`.
- [ ] **Step 5: Update README.md.** Session panel paragraph: Refresh keeps the text and shows `Refreshing...`; a failed fetch keeps the last good copy with the error in orange; Show all appears only past 40 lines. S0 line: S0 reads each candidate ticket in full through `get_item_detail` before proposing.
- [ ] **Step 6: Check the diff for em dashes and real names.** `git diff main -- . | grep -nP "\x{2014}"`, `git diff main -- . | grep -noP "[a-z0-9-]+\.atlassian\.net" | grep -v ":demo\.atlassian\.net"` and `git diff main -- . | grep -noP "[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[a-z]{2,}" | grep -v ":demo@example\.test"` all return nothing.
- [ ] **Step 7: Run the full gate.** `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` Expected: all pass.
- [ ] **Step 8: Commit** `Have S0 read each ticket before it plans` (body: `Fixes #48`).
