# Owned Item Summary and Detail View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show each owned item's title and status in the session panel's Owns box, and open the item's details (description, notes, PRs, child beads, folder) in the panel on click, fetched on demand by the daemon.

**Architecture:** A new `GetItemDetail` request goes to the daemon, which builds an `ItemDetail` from the work cache and adds the body fetched from Jira (REST API or acli) or `bd show`, cached per key for 5 minutes, with the folder overview as a fallback. The GUI renders it in place of the panel's boxes; the MCP tool `get_item_detail` renders it as text.

**Tech Stack:** Rust workspace (overseer-proto, overseer-daemon, overseer, overseer-gui), serde_json, ureq (JiraApi), tokio, gpui.

**Spec:** docs/superpowers/specs/2026-11-18-ticket-detail-design.md

## Global Constraints

- No em dashes anywhere (code, comments, docs, commits).
- Invented names only in code, tests and docs: DEMO-n, demo.atlassian.net, demo@example.test, demo-ws.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; run `git log -5` before committing; no emojis in commits.
- Never run writing `bd` commands against ~/Repos/2ndBrain. Never run the real `security` binary against the user's Keychain in tests.
- Cache TTL: 5 minutes. A failed fetch is not cached. `refresh: true` bypasses the cache.
- Overview fallback: first non-empty paragraph of `00-PROJECT-OVERVIEW.md` after front matter and the first heading, capped at about 600 characters; filled only when `body` is `None`.
- Bead notes: last 10 lines of the `notes` field. Dependencies: ids, `id (type)` when bd gives a type.
- acli detail command: `<jira_command[0]> jira workitem view <KEY> --fields summary,description,status,priority --json`; on failure the error names the command run.
- REST detail: `GET rest/api/3/issue/{key}?fields=summary,status,priority,description`, same auth as `tickets()`.
- Detail body preview: first 40 lines, then a "Show all" toggle.
- Release never fires from opening the detail; the regular polls do not change.
- Gate before each commit: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the tests of the crates touched.

## Review Focus

1. ADF with unknown node types, nested lists, marks, hard breaks, or an empty/null description: text comes out, nothing panics, an empty description shows as "No description". (Task 1 test `adf_odd_nodes_fall_back_to_text`.)
2. Jira off, no token, or Jira timing out: the detail still arrives with cached fields, the overview, and an `error` naming the cause. (Task 2 test `detail_says_why_when_jira_is_off`.)
3. Clicking Release on a row neither opens the detail nor leaves it opening; the release still happens. (Task 4 manual check, step 6, plus the model test that the row's open target excludes the button.)
4. The item is released or moved to another slot while its detail is open: the detail closes back to the panel. (Task 4 test `detail_closes_when_the_item_leaves_the_slot`.)
5. A reply for an earlier key arriving after the user opened another item does not overwrite the newer view. (Task 4 test `a_late_reply_for_another_key_is_ignored`.)

---

### Task 1: Protocol types and pure parsers

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (Request near `ListWorkItems` at ~520, Response at ~1033, round-trip tests at ~1531)
- Create: `crates/overseer-daemon/src/work/detail.rs` (register in `work/mod.rs`)
- Create: `crates/overseer-daemon/tests/fixtures/bd-show.json`, `crates/overseer-daemon/tests/fixtures/jira-issue.json`

**Interfaces:**
- Produces (proto):
  - `Request::GetItemDetail { key: String, refresh: bool }`
  - `Response::ItemDetail(ItemDetail)`
  - `pub struct ItemDetail { key: String, kind: ItemKind, title: String, status: String, priority: Option<String>, stage: Option<Stage>, stage_since: Option<i64>, folder: Option<PathBuf>, body: Option<String>, notes: Option<String>, depends_on: Vec<String>, children: Vec<ChildBead>, prs: Vec<PullRequest>, overview: Option<String>, fetched_at: Option<i64>, error: Option<String> }` with `#[serde(default)]` on every `Option`/`Vec` field.
  - `pub struct ChildBead { key: String, title: String, status: String }`
- Produces (detail.rs):
  - `pub const DETAIL_TTL: Duration = Duration::from_secs(300);`
  - `pub const NOTES_LINES: usize = 10;` `pub const OVERVIEW_CAP: usize = 600;`
  - `pub fn adf_to_text(v: &Value) -> String`
  - `pub struct IssueDetail { pub title: String, pub status: String, pub priority: Option<String>, pub body: Option<String> }`
  - `pub fn parse_issue_detail(out: &str) -> Result<IssueDetail, String>`: skips text before the first `{` like `parse_jira`; accepts the REST shape (under `fields`, status/priority as `{name}`) and a flat object (top-level `summary`/`description`, status/priority as `{name}` or string); description as ADF object or string; empty string or empty ADF gives `None`.
  - `pub struct BeadDetail { pub body: Option<String>, pub notes: Option<String>, pub depends_on: Vec<String> }`
  - `pub fn parse_bead_detail(out: &str) -> Result<BeadDetail, String>`: accepts an object or an array whose first element is the object.
  - `pub fn overview_paragraph(md: &str) -> Option<String>`

ADF rendering rules (exact output the tests pin): `heading` gives its text on its own line, prefixed by `#` repeated to its level and a space; `paragraph` gives its text followed by a blank line; `bulletList` items give `- ` plus text, `orderedList` items give `1. `, `2. `...; nested lists indent two spaces per level; `codeBlock` gives its text between lines of three backticks; `hardBreak` gives a newline; `text` gives its `text` (marks ignored); `mention`/`emoji`/`inlineCard` give `attrs.text` or `attrs.url`; any other node gives its children's text. Trim trailing blank lines.

- [ ] **Step 1: Capture the bd fixture.** In a new directory under the session scratchpad (never ~/Repos/2ndBrain), run with a 60 s timeout: `git init`, `bd init --prefix demo`, create two beads, set notes with 12 lines on the first, add a dependency from the first to the second, then `bd show <first> --json > bd-show.json`. Copy it to `crates/overseer-daemon/tests/fixtures/bd-show.json` with ids rewritten to `demo-1`/`demo-2`. If `bd init` hangs or fails, stop it, handwrite the fixture with top-level `id`, `title`, `description`, `notes`, `status`, `priority`, and `dependencies: [{"depends_on_id": "demo-2", "type": "blocks"}]`, and say so in the commit message and the report. Write `jira-issue.json` by hand in the REST shape for DEMO-1 with an ADF description holding a heading, a paragraph, a nested bullet list and a code block.

- [ ] **Step 2: Write the failing tests** in `detail.rs`'s test module:
  - `adf_renders_headings_lists_and_code`: the fixture description renders to exactly the expected string per the rules above (write the expected string literally).
  - `adf_odd_nodes_fall_back_to_text`: a doc with an unknown `panel` node holding a paragraph, a `hardBreak`, a `mention` with `attrs.text = "@Demo"` and a `null` description; asserts the text appears and `parse_issue_detail` of `{"fields":{"summary":"S","description":null}}` gives `body: None`.
  - `issue_detail_reads_rest_and_flat_shapes`: the fixture gives title, status `In Progress`, priority `Some("High")`; a flat `{"key":"DEMO-2","summary":"T","status":"To Do","description":"plain"}` gives body `Some("plain")`; leading text `Loading...\n` before the JSON is skipped; `"not json"` is `Err`.
  - `bead_detail_takes_notes_tail_and_deps`: from the bd fixture, `notes` has exactly the last 10 lines, `depends_on == ["demo-2 (blocks)"]`; the same object wrapped in an array parses the same.
  - `overview_skips_front_matter_and_heading`: `"---\ntitle: x\n---\n# DEMO-1\n\nFirst para\nline two.\n\nSecond."` gives `Some("First para\nline two.")`; a 2000-character paragraph is cut to at most 600 characters ending in `...` on a char boundary; `""` gives `None`.
  - In msg.rs, add `Request::GetItemDetail { key: "DEMO-1".into(), refresh: true }` and a `Response::ItemDetail` with defaults to the existing round-trip test lists.

- [ ] **Step 3: Run them to see them fail.** Run: `cargo test -p overseer-daemon --lib work::detail` and `cargo test -p overseer-proto`. Expected: compile errors for the missing items.

- [ ] **Step 4: Implement** the proto types and the five functions in `detail.rs`. Add `GetItemDetail` to any exhaustive `match` on `Request` (server.rs returns a placeholder `Response::Error { message: "answered by the connection".into() }` arm for now, like `RefreshWorkItems`).

- [ ] **Step 5: Run the gate.** `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p overseer-proto && cargo test -p overseer-daemon --lib work::detail`. Expected: all pass.

- [ ] **Step 6: Commit** `Add item detail types and Jira, ADF and bd detail parsers` (body: `Part of #22`).

### Task 2: Daemon fetch, cache and request handling

**Files:**
- Modify: `crates/overseer-daemon/src/work/jira_api.rs` (new method next to `tickets()` at ~134; tests module at ~318)
- Modify: `crates/overseer-daemon/src/work/store.rs` (new method and cache field on `WorkStore`; tests module at ~1754)
- Modify: `crates/overseer-daemon/src/server.rs` (`handle_frame` arm beside `RefreshWorkItems` at ~778; replace Task 1's placeholder)

**Interfaces:**
- Consumes: Task 1's `ItemDetail`, `ChildBead`, `parse_issue_detail`, `parse_bead_detail`, `overview_paragraph`, `DETAIL_TTL`, `NOTES_LINES`.
- Produces:
  - `JiraApi::issue(&self, token: &str, key: &str) -> Result<String, String>`: the raw JSON from `GET rest/api/3/issue/{key}?fields=summary,status,priority,description`; 404 maps to `"{key} was not found in Jira"`, other errors through `classify`.
  - `WorkStore::item_detail(&self, key: &str, refresh: bool) -> Result<ItemDetail, String>` (async): `Err` only when the key is neither in the view's items nor owned.

Behaviour:
- Base fields come from `self.view()`: the item with that key (owned, cached, or `in_list: false`). `children`: beads whose `parent` is the key. `prs`: `view.prs` with `ticket == Some(key)`, newest `created` first, undated last.
- Ticket body: `JiraSource::Api` gets the token with `jira_token(&self.jira_email, vars, &self.security)` and calls `issue` via `run_blocking`; `JiraSource::Command(cmd)` runs `[cmd[0], "jira", "workitem", "view", key, "--fields", "summary,description,status,priority", "--json"]` through `run_source` with `SOURCE_TIMEOUT` and cwd `/`, error mapped through `missing_program_hint` and prefixed with the command line; `JiraSource::Off` gives error `"Jira is off: no jira_site or jira_command"`.
- Bead body: `self.bd(&["show", key, "--json"])` through `run_source` in `self.beads_dir`.
- On success the fetched `body`/`notes`/`depends_on` are stored in a `Mutex<HashMap<String, (Instant, Fetched, i64)>>` with the unix time as `fetched_at`; a hit younger than `DETAIL_TTL` is used unless `refresh`. On failure `error` is set, nothing is cached, and `overview` is filled from `folder/00-PROJECT-OVERVIEW.md` when the folder is set and the file reads.
- Use the env the polls use (`self.env.clone()`, `wait().await`, `get()`).

- [ ] **Step 1: Write the failing tests.**
  - jira_api.rs `issue_reads_one_ticket_and_maps_404`: using the existing fake-server helpers and `with_bases`, a 200 returns the body and the request path is `/ex/jira/demo-cloud/rest/api/3/issue/DEMO-1` with the `fields` query; a 404 gives `Err("DEMO-1 was not found in Jira")`.
  - store.rs `detail_for_a_bead_runs_bd_show_and_caches`: with a fake `bd_command` script that appends its args to a log file and prints the bd fixture, `item_detail("demo-1", false)` twice returns the notes tail both times and the log has one `show demo-1 --json` line; with `refresh: true` a second line appears.
  - store.rs `detail_for_a_command_ticket_uses_acli_view`: a fake `jira_command[0]` script logging its args and printing `jira-issue.json`; asserts the logged args are `jira workitem view DEMO-1 --fields summary,description,status,priority --json` and the body is the ADF text.
  - store.rs `detail_says_why_when_jira_is_off`: store with no Jira source, an owned ticket DEMO-1 with a folder whose `00-PROJECT-OVERVIEW.md` has a paragraph; the result has `body: None`, `error == Some("Jira is off: no jira_site or jira_command")`, `overview` set, and a second call runs nothing new (nothing cached, nothing to run).
  - store.rs `a_failed_fetch_is_not_cached`: fake script fails once then succeeds (a counter file); first call has `error` set, second call has the body.
  - store.rs `unknown_key_is_an_error`: `item_detail("DEMO-999", false)` is `Err`.
  - store.rs `detail_lists_child_beads_and_prs_newest_first`: two beads with `parent: DEMO-1` and two PRs for DEMO-1 with different `created`; asserts order.

- [ ] **Step 2: Run them to see them fail.** `cargo test -p overseer-daemon --lib work::` Expected: missing methods.

- [ ] **Step 3: Implement** `JiraApi::issue`, `WorkStore::item_detail` and the cache, then the `handle_frame` arm: spawn on `store.spawn`, reply `Response::ItemDetail(d)` or `Response::Error { message }`.

- [ ] **Step 4: Add a server test** in `crates/overseer-daemon/tests/server.rs`, `get_item_detail_is_answered`, using its existing daemon harness: an unknown key returns `Response::Error`. (The fetch paths are covered by the store tests.)

- [ ] **Step 5: Run the gate.** fmt, clippy, `cargo test -p overseer-daemon`. Expected: pass (the known flaky `a_bd_sync_owed_at_a_restart_still_runs`, issue #43, may fail; rerun it alone before treating it as a regression).

- [ ] **Step 6: Commit** `Fetch an item's details on request, cached for five minutes` (body: `Part of #22`).

### Task 3: MCP tool `get_item_detail`

**Files:**
- Modify: `crates/overseer/src/mcp.rs` (tool list near `get_work_items` at ~165, plan at ~403, reply formatting near `Response::Work` at ~497)
- Modify: `crates/overseer-daemon/src/launch.rs` (`ALLOWED_TOOLS` at 194 becomes `[&str; 17]`; the tool-name test list near 1181)
- Modify: `README.md` (the MCP tools list)
- Test: `crates/overseer/tests/mcp.rs`

**Interfaces:**
- Consumes: Task 1 `ItemDetail`, Task 2's request handling.
- Produces: `fn detail_text(d: &ItemDetail) -> String` in mcp.rs.

`detail_text` layout (exact): line 1 `{key} {title}`; line 2 `Status: {status} · Priority: {priority or "not set"}` plus ` · Stage: {label}` when set; blank line; the body, or `No description` when `body` is `None` and `error` is `None`; when `error` is set, `Could not fetch the description: {error}` and, if present, a blank line and `From the folder overview: {overview}`; then, when present, sections `Notes:`, `Depends on: a, b`, `Pull requests:` (one `- {repo} #{id} {title}` line each), `Child beads:` (one `- {key} {status} {title}` line each), `Folder: {path}`.

- [ ] **Step 1: Write the failing tests.** In mcp.rs unit tests: `detail_text_lists_body_prs_and_children` and `detail_text_shows_the_error_and_overview` asserting the exact lines above for two hand-built `ItemDetail`s. In tests/mcp.rs: `get_item_detail_is_listed_and_sends_the_request`, following the `get_work_items` test at ~238, asserting the tool appears in `tools/list` with a required `key` string and optional `refresh` boolean. Update the launch.rs allowed-tools test to expect `mcp__overseer__get_item_detail`.
- [ ] **Step 2: Run to see them fail.** `cargo test -p overseer mcp` and `cargo test -p overseer-daemon --lib launch`.
- [ ] **Step 3: Implement** the tool schema (description: `Read one ticket's or bead's details: description, notes, PRs, child beads. Cached for 5 minutes; refresh: true fetches again.`), `ToolPlan::Send(Request::GetItemDetail { key, refresh })`, the `Response::ItemDetail` reply through `detail_text`, `ALLOWED_TOOLS`, and the README line.
- [ ] **Step 4: Run the gate.** fmt, clippy, `cargo test -p overseer -p overseer-daemon`.
- [ ] **Step 5: Commit** `Add the get_item_detail MCP tool` (body: `Part of #22`).

### Task 4: GUI Owns rows and the detail view

**Files:**
- Modify: `crates/overseer-gui/src/session.rs` (`OwnsRow`/`owns_rows` at ~189-226, `render_owns` at ~846, `render_panel` at ~747; tests at ~1621)
- Modify: `crates/overseer-gui/src/app.rs` (state field on `OverseerApp`, request sending beside `release_item` at ~963, clearing on tab change and on work updates)

**Interfaces:**
- Consumes: Task 1 `ItemDetail`, `Request::GetItemDetail`, `Response::ItemDetail`.
- Produces:
  - `pub enum DetailState { Loading, Loaded(ItemDetail), Failed(String) }`
  - `pub struct OpenDetail { pub slot: SlotId, pub key: String, pub state: DetailState, pub show_all: bool }`, held as `detail: Option<OpenDetail>` on `OverseerApp`.
  - `pub struct DetailModel { pub key: String, pub title: String, pub rows: Vec<(String, String)>, pub body: Vec<BodyLine>, pub truncated: bool, pub notice: Option<String>, pub children: Vec<String>, pub loading: bool }`
  - `pub enum BodyLine { Heading(String), Bullet { depth: usize, text: String }, Code(String), Text(String), Blank }`
  - `pub const BODY_PREVIEW_LINES: usize = 40;`
  - `pub fn body_lines(text: &str) -> Vec<BodyLine>` (parses `detail_text`-style plain text from Task 1's ADF rules: `#` headings, `- `/`n. ` bullets with two-space depth, fenced code, blank lines).
  - `pub fn detail_model(open: &OpenDetail, view: &WorkView, now: i64) -> DetailModel`: while `Loading` or `Failed`, rows come from the `WorkItem` in `view`; `loading` true for `Loading`; `notice` is the failure or `error` text; body capped at `BODY_PREVIEW_LINES` unless `show_all`, with `truncated` set when lines were dropped.
  - `pub fn detail_still_owned(open: &OpenDetail, view: &WorkView) -> bool`
  - `OverseerApp::open_detail(&mut self, slot: SlotId, key: String, refresh: bool, cx)`, `close_detail(&mut self, cx)`.

Rows in order: `Status`, `Priority` (`Not set` when none), `Stage` (`{label} · {age}` when `stage_since` set), one `PR` row per PR (`{repo} #{id} {title}`), `Notes` and `Depends on` for beads, `Folder` (tilde path), `Fetched` (`{age} ago` from `fetched_at`).

- [ ] **Step 1: Write the failing tests** (pure functions in session.rs tests):
  - `body_lines_reads_headings_bullets_and_code`: the Task 1 fixture's expected text gives the matching `BodyLine`s.
  - `detail_model_while_loading_uses_the_cache`: `Loading` for owned DEMO-1 gives `loading: true`, title and `Status` row from the `WorkItem`, empty body.
  - `detail_model_caps_long_bodies`: a 100-line body gives 40 lines and `truncated: true`; with `show_all` 100 lines and `truncated: false`.
  - `detail_model_shows_the_failure_and_overview`: `Loaded` with `error` and `overview` set gives `notice == Some(<error>)` and the overview as the body.
  - `detail_closes_when_the_item_leaves_the_slot`: `detail_still_owned` is false once the `WorkItem`'s owner differs from `open.slot` or the item is gone.
  - `a_late_reply_for_another_key_is_ignored`: a helper `fn take_reply(open: &mut Option<OpenDetail>, key: &str, resp: Result<ItemDetail, String>)` only updates when `open.key == key`; assert a reply for DEMO-1 leaves an open DEMO-2 `Loading`.
- [ ] **Step 2: Run to see them fail.** `cargo test -p overseer-gui session`.
- [ ] **Step 3: Implement the Owns row layout.** First line: key (semibold) then status (muted, `text_xs`); second line: the title in a `div` with `line_clamp(2)` (or the gpui equivalent; if none, cut to 140 characters with `...`); the `gone`/`not read yet` line below. Release stays right-aligned. The row div gets an `id(("own", n))`, `cursor_pointer`, and `on_click` calling `open_detail(id, key, false)`; the Release button's handler calls `cx.stop_propagation()` before `release_item`.
- [ ] **Step 4: Implement the detail view.** In `render_panel`, when `self.detail` is open for this slot, render instead of the boxes: a top bar (`Back` button calling `close_detail`, the key, `Refresh` calling `open_detail(.., true)`), the full title, the rows, the orange notice line, the body (`Heading` semibold, `Bullet` indented 12 px per depth with a `•`, `Code` in the terminal font on a darker background, `Text`, `Blank` as a small gap, `Loading description...` while loading), `Show all` / `Show less` when `truncated` or `show_all`, then `Child beads`. The panel's existing `overflow_y_scroll` covers scrolling. Clear `detail` on slot tab change and whenever a work update makes `detail_still_owned` false.
- [ ] **Step 5: Run the gate.** fmt, clippy, `cargo test -p overseer-gui`.
- [ ] **Step 6: Check by hand.** Build and run the GUI against a dev daemon (not the installed app), assign DEMO-style test items if the dev daemon has sources, and confirm: rows show two lines; clicking a row opens the detail with cached fields immediately; Back returns; clicking Release releases and does not open the detail; Refresh refetches. Take a screenshot of the detail view for the PR.
- [ ] **Step 7: Commit** `Show owned items' titles and open their details in the session panel` (body: `Part of #22`).

### Task 5: Docs and final pass

**Files:**
- Modify: `README.md` (session panel section: the Owns box rows and the detail view, including the 5-minute cache and Refresh)

- [ ] **Step 1:** Write the README paragraph; grep the diff for em dashes and real names: `git diff origin/main -- . | grep -nP "\x{2014}|PLAT-|atlassian\.net" | grep -v demo` returns nothing.
- [ ] **Step 2:** Run the full gate: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- [ ] **Step 3: Commit** `Describe the owned item detail view` (body: `Fixes #22`).
