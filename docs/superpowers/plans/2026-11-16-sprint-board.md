# Sprint Kanban and Open PRs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show Jonathan's sprint as a Jira-style kanban under the session cards and the open Bitbucket PRs with their ages in the top right of the board.

**Architecture:** The daemon maps each Jira status to a stage, records when each ticket entered its stage in the ledger, reads open PRs with `bb` in each configured repo, and sends stage, stage age, PRs and the sprint day in `WorkView`. The GUI turns that into column, progress and PR-row models with pure functions in a new `sprint_view.rs`, and renders them in `render_board`.

**Tech Stack:** Rust workspace, tokio, rusqlite, serde_json, gpui.

**Spec:** `docs/superpowers/specs/2026-11-16-sprint-board-design.md`

## Global Constraints

- Public repo: invented names only in tests, fixtures and docs (`DEMO-12`, `demo-api`, `demo-web`).
- No em dashes anywhere (code, comments, docs, commit messages).
- Tests never run real `acli` or `bb`, and never touch `~/Repos/2ndBrain`, `~/Repos/.worktrees` or the real `CURRENT_SESSION.md`. Use temp dirs and fixture JSON.
- Commits: no emojis, plain sentence-case subject like `git log`, trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Older GUIs and daemons must still talk: every new wire field has `#[serde(default)]`.
- Colours come from `crates/overseer-gui/src/app.rs` `mod colors`; the one new hue is testing purple `0xd2a8ff` (add it as `colors::PURPLE`).
- Defaults pinned by the spec:
  - `pr_command` = `["bb", "pullrequest", "list", "--state", "open", "--output", "json"]`
  - `pr_repos` = `[]` (empty turns the PR source off), `pr_poll_secs` = 300, `pr_author` = `""` (all authors)
  - `stage_todo` = Backlog, Prioritised Backlog, To Do, Selected for Development
  - `stage_in_progress` = In Progress
  - `stage_in_review` = In Review, Code Review, Review
  - `stage_testing` = To Be Tested, Testing, In Testing, QA
  - `stage_waiting` = Blocked, Waiting, On Hold
  - `stage_done` = Done, Closed, Resolved
- Status names match stage lists case-insensitively after trimming; an unmapped status is In progress.
- PR age colour: under 2 days green, from 2 days amber (`colors::ORANGE`), from 5 days red.
- Kanban columns show at most 4 cards then `+N more`; the PR panel shows at most 8 rows then `+N more`.
- Priority chips: `Sprint Breaker` is BREAKER (bg `0x4a1d1d`, fg `0xff9a94`); Highest and High are HIGH (`0x45290f` / `0xf0a868`); Medium is MED (`0x232a38` / `0xaab3c2`); Low and Lowest are LOW (`0x1b2029` / `0x8b949e`); no priority or anything else is MED.

## Review Focus

1. `bb` output in a shape we have not seen (wrapped in `values`, ids as numbers or strings, no `created_on`, leading warning text): parse what is there, skip entries with no id, never fail the whole repo. Pinned in Task 3.
2. One repo in `pr_repos` missing or `bb` failing there: the other repos' PRs still show and the `prs` source message names the failing repo. Pinned in Task 5.
3. A Jira status nobody configured (`Ready for Release`): the ticket lands in In progress and its card shows the raw status. Pinned in Tasks 2 and 6.
4. Daemon restart, and a ticket seen for the first time: the stage age survives the restart, and a ticket never seen moving shows no age. Pinned in Task 4.
5. No sprint start set, or a sprint with no tickets: pace marker and warning hidden; kanban section hidden when there are no sprint tickets. Pinned in Task 6.

---

### Task 1: Wire types

**Files:**
- Modify: `crates/overseer-proto/src/msg.rs` (`WorkItem`, `WorkView`, new types)
- Modify: every `WorkItem { .. }` and `WorkView { .. }` literal in the workspace (`cargo build` lists them), including `crates/overseer-gui/src/app.rs` `empty_work`

**Interfaces:**
- Produces:
  - `pub enum Stage { ToDo, InProgress, InReview, Testing, Waiting, Done }`, deriving `Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize`, with `Stage::ALL: [Stage; 6]` in column order and `fn label(self) -> &'static str` returning `TO DO`, `IN PROGRESS`, `IN REVIEW`, `TESTING`, `WAITING`, `DONE`.
  - `WorkItem` gains `#[serde(default)] pub stage: Option<Stage>` (the displayed stage; `None` for beads and for tickets from an old daemon) and `#[serde(default)] pub stage_since: Option<i64>` (unix seconds the ticket entered its Jira stage, only when the daemon saw it move).
  - `pub struct PullRequest { pub repo: String, pub id: u64, pub title: String, pub branch: String, pub url: Option<String>, pub author: Option<String>, pub created: Option<i64>, pub ticket: Option<String> }` deriving `Debug, Clone, PartialEq, Serialize, Deserialize`.
  - `pub struct SprintDay { pub day: u32, pub days: u32 }` deriving the same plus `Copy, Eq`.
  - `WorkView` gains `#[serde(default)] pub prs: Vec<PullRequest>` and `#[serde(default)] pub sprint_day: Option<SprintDay>`. The existing `sprint: Option<String>` label stays as it is (Ruling: added beside the label instead of replacing it, so old clients keep their header).
  - `pub const PRS_OFF: &str = "pr_repos is empty; PRs are off"` beside `JIRA_OFF`.

- [ ] **Step 1: Write the failing test** in `msg.rs` tests:

```rust
#[test]
fn old_work_view_json_still_reads() {
    let v: WorkView = serde_json::from_str(
        r#"{"items":[{"key":"DEMO-1","kind":"Ticket","title":"t","status":"Done","priority":null,"parent":null,"owner":null,"in_list":true,"folder":null}],"sources":[],"notices":[]}"#,
    ).unwrap();
    assert_eq!(v.items[0].stage, None);
    assert_eq!(v.items[0].stage_since, None);
    assert!(v.prs.is_empty());
    assert_eq!(v.sprint_day, None);
    assert_eq!(Stage::ALL.map(Stage::label), ["TO DO", "IN PROGRESS", "IN REVIEW", "TESTING", "WAITING", "DONE"]);
}
```

(Match the `kind` spelling to how `ItemKind` already serialises; copy it from an existing test if it differs.)

- [ ] **Step 2:** `cargo test -p overseer-proto old_work_view_json_still_reads` fails to compile.
- [ ] **Step 3:** Add the types and fields; fix every literal with `stage: None, stage_since: None` / `prs: Vec::new(), sprint_day: None`.
- [ ] **Step 4:** `cargo build --workspace --all-targets && cargo test -p overseer-proto` passes.
- [ ] **Step 5:** Commit `Add sprint stage, PR and sprint day fields to the work view`.

### Task 2: Config, settings and the stage mapping

**Files:**
- Modify: `crates/overseer-daemon/src/config.rs` (fields and defaults)
- Modify: `crates/overseer-daemon/src/settings.rs` (rows)
- Create: `crates/overseer-daemon/src/work/stages.rs`; register in `work/mod.rs`
- Modify: `crates/overseer-daemon/src/work/sprint.rs` (sprint day)

**Interfaces:**
- Consumes: `Stage` (Task 1).
- Produces:
  - `Config` fields `pr_repos: Vec<PathBuf>`, `pr_command: Vec<String>`, `pr_poll_secs: u64`, `pr_author: String`, `stage_todo`, `stage_in_progress`, `stage_in_review`, `stage_testing`, `stage_waiting`, `stage_done` (each `Vec<String>`), defaults as in Global Constraints. `pr_repos` entries expand `~` the way other path settings do.
  - `pub struct Stages` with `pub fn from_config(cfg: &Config) -> Stages` and `pub fn stage_of(&self, status: &str) -> Stage`.
  - In `sprint.rs`: `pub fn sprint_day(start: Ymd, days: u32, today: Ymd) -> Option<SprintDay>`: `None` before the first weekday, the reached weekday count (capped at `days`) during and after; `days` 0 means `DEFAULT_DAYS`. `sprint_label` uses it so the two never disagree. Also make `to_days` `pub(crate)` for Task 3.
  - Settings rows: `pr_repos` (`WORK`, `SettingKind::List`), `pr_poll_secs` (`WORK`, `POLL`), `pr_author` (`WORK`, `Text { allow_empty: true }`), the six `stage_*` rows (`SPRINT`, `SettingKind::List`), and `pr_command` as a `read_only` row in `COMMANDS` with `COMMAND_NOTE`, like `jira_command`.

- [ ] **Step 1: Write the failing tests.** In `stages.rs`:

```rust
#[test]
fn statuses_map_to_stages() {
    let s = Stages::from_config(&Config::default());
    for (status, want) in [
        ("Prioritised Backlog", Stage::ToDo), ("in progress", Stage::InProgress),
        (" Code Review ", Stage::InReview), ("To Be Tested", Stage::Testing),
        ("Blocked", Stage::Waiting), ("RESOLVED", Stage::Done),
        ("Ready for Release", Stage::InProgress), ("", Stage::InProgress),
    ] {
        assert_eq!(s.stage_of(status), want, "{status}");
    }
}
```

In `sprint.rs`: `sprint_day(ymd(2026,11,9), 10, ymd(2026,11,16)) == Some(SprintDay{day:6,days:10})`, `ymd(2026,11,6)` gives `None`, `ymd(2026,11,23)` gives `Some(SprintDay{day:10,days:10})`, and `days` 0 gives `days: 10`. In `config.rs`: defaults for the new keys load from an empty file and from a file setting `pr_repos = ["~/Repos/demo-api"]` (expanded under the home dir). The existing settings tests that check every config key has a row must pass with the new rows.

- [ ] **Step 2:** Tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p overseer-daemon` passes.
- [ ] **Step 5:** Commit `Add PR and sprint stage settings`.

### Task 3: Parse `bb` PR output

**Files:**
- Modify: `crates/overseer-daemon/src/work/sources.rs`
- Create: `crates/overseer-daemon/tests/fixtures/bb-prs.json` (invented: two PRs in `{"values": [...]}`, one with branch `feature/DEMO-12-widget`, one titled `DEMO-14 Fix the flag` on branch `fix-flag`)

**Interfaces:**
- Consumes: `PullRequest` (Task 1), `to_days` (Task 2).
- Produces:
  - `pub fn parse_prs(out: &str, repo: &str) -> Result<Vec<PullRequest>, String>`: skips text before the first `[` or `{` like `parse_jira`; accepts an array or an object holding `values` (else `pullrequests`); per entry reads `id` (number, or a string of digits), `title`, `source.branch.name`, `links.html.href`, `author.display_name` (else `author.nickname`), `created_on`; an entry with no usable id is skipped; `ticket` is left `None` (Task 5 links it).
  - `pub fn parse_iso_time(s: &str) -> Option<i64>`: RFC 3339 like `2026-11-14T09:12:33.123456+00:00` or `...Z` to unix seconds, honouring the offset; fractional seconds ignored.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn parses_bb_values_fixture() {
    let prs = parse_prs(include_str!("../../tests/fixtures/bb-prs.json"), "demo-api").unwrap();
    assert_eq!(prs.len(), 2);
    assert_eq!(prs[0].repo, "demo-api");
    assert_eq!(prs[0].branch, "feature/DEMO-12-widget");
    assert!(prs[0].created.is_some());
}

#[test]
fn bb_odd_shapes_are_tolerated() {
    let prs = parse_prs(r#"note: cached
[{"id":"7","title":"a"},{"title":"no id"},{"id":8}]"#, "demo-web").unwrap();
    assert_eq!(prs.iter().map(|p| p.id).collect::<Vec<_>>(), [7, 8]);
    assert_eq!(prs[1].title, "");
    assert_eq!(prs[0].created, None);
    assert!(parse_prs("nothing here", "x").is_err());
}

#[test]
fn iso_times_honour_the_offset() {
    assert_eq!(parse_iso_time("1970-01-02T00:00:00Z"), Some(86_400));
    assert_eq!(parse_iso_time("1970-01-02T01:00:00.5+01:00"), Some(86_400));
    assert_eq!(parse_iso_time("2026-11-14"), None);
}
```

- [ ] **Step 2:** Tests fail. **Step 3:** Implement. **Step 4:** `cargo test -p overseer-daemon sources` passes.
- [ ] **Step 5:** Commit `Read open PRs from bb output`.

### Task 4: Ledger stage records

**Files:**
- Modify: `crates/overseer-daemon/src/ledger.rs`

**Interfaces:**
- Consumes: `Stage` (Task 1).
- Produces:
  - `V7_SCHEMA`: `CREATE TABLE IF NOT EXISTS ticket_stages (key TEXT PRIMARY KEY, stage TEXT NOT NULL, since INTEGER NOT NULL, moved INTEGER NOT NULL)`; `VERSION` becomes 7.
  - `pub fn record_stages_on(conn: &Connection, seen: &[(String, Stage)], now: i64) -> Result<()>`: a new key is inserted with `since = now, moved = 0`; a key whose stage changed is updated to `since = now, moved = 1`; an unchanged key is left alone. Keys not in `seen` are kept (a ticket can drop out of one Jira read).
  - `pub fn stages_on(conn: &Connection) -> Result<HashMap<String, (Stage, i64, bool)>>` (stage, since, moved). Stage stored with the existing `enum_name` / `enum_from_name`; unreadable rows skipped.

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn stage_records_keep_when_a_ticket_moved() {
    let l = Ledger::open_in_memory_or_temp_for_tests(); // use the helper existing ledger tests use
    l.transaction(|c| record_stages_on(c, &[("DEMO-1".into(), Stage::ToDo)], 100)).unwrap();
    l.transaction(|c| record_stages_on(c, &[("DEMO-1".into(), Stage::ToDo)], 200)).unwrap();
    assert_eq!(l.transaction(stages_on).unwrap()["DEMO-1"], (Stage::ToDo, 100, false));
    l.transaction(|c| record_stages_on(c, &[("DEMO-1".into(), Stage::Testing)], 300)).unwrap();
    assert_eq!(l.transaction(stages_on).unwrap()["DEMO-1"], (Stage::Testing, 300, true));
}
```

Plus: a ledger file at version 6 opened by the new code keeps its rows and gains `ticket_stages` (follow the existing migration test for V6), and reopening the file keeps `ticket_stages` rows (the restart case).

- [ ] **Step 2:** Fails. **Step 3:** Implement. **Step 4:** `cargo test -p overseer-daemon ledger` passes.
- [ ] **Step 5:** Commit `Record when each ticket entered its stage`.

### Task 5: Store: PR source, stages and the view

**Files:**
- Modify: `crates/overseer-daemon/src/work/store.rs`
- Test: `store.rs` tests (fake `bb` and `acli` as `sh -c` scripts printing fixture JSON, as the existing store tests fake `bd`)

**Interfaces:**
- Consumes: Tasks 1 to 4.
- Produces:
  - `Source::Prs`. `refresh(Source::Prs)` runs `pr_command` through `run_source` in each `pr_repos` folder with the same `SOURCE_TIMEOUT` and env as Jira, one after another, under a new `prs_turn` lock. Repo name is the folder's file name. Results from repos that answered replace the cache; a failing repo keeps its previous PRs (Ruling: keeps the panel stable through a blip; cost if wrong is a stale row until the next good read) and the status message is `"<repo>: <error>"`, joined with `; ` for several. With `pr_repos` empty the status is `status("prs", Some(PRS_OFF.into()))` and nothing runs.
  - `pr_author` non-empty keeps only PRs whose `author` equals it, ignoring case.
  - Each PR's `ticket` is the first `pattern` match in its title, else in its branch.
  - On a good Jira read, the store calls `record_stages_on` with each ticket's `Stages::stage_of(status)` in a ledger transaction.
  - `view()` sets, for each ticket item: `stage = Some(jira stage)`, promoted to `InReview` when it is `ToDo` or `InProgress` and some PR's `ticket` is its key; `stage_since = Some(since)` only when the record's `moved` is true. Beads keep `None`. `view().prs` is the cached PRs, `view().sources` gains the `prs` status third, `view().sprint_day` comes from `sprint_day(start, days, today)` when `sprint_start` is set.
  - `start_polling` adds `(Source::Prs, pr_poll)`; `refresh_all` also refreshes PRs.

- [ ] **Step 1: Write the failing tests** (names and assertions):
  - `pr_source_reads_each_repo_and_links_tickets`: two temp repo folders `demo-api` and `demo-web`, a fake `pr_command` that prints a different PR list depending on `$PWD`; after `refresh(Source::Prs)`, `view().prs` has both repos' PRs, the PR on branch `feature/DEMO-12-widget` has `ticket == Some("DEMO-12")`, and the `prs` source is `ok` with the total count.
  - `one_failing_repo_keeps_the_others`: `pr_repos` holds `demo-api` and a missing folder `demo-gone`; `demo-api`'s PRs are in the view and the `prs` message contains `demo-gone`.
  - `pr_author_filters_prs`: `pr_author = "dana demo"` keeps only `Dana Demo`'s PR.
  - `empty_pr_repos_is_off`: the `prs` source message is `PRS_OFF` and `view().prs` is empty.
  - `ticket_with_an_open_pr_shows_in_review`: Jira says `DEMO-12` is `In Progress`, a PR links to it, `view()` gives `stage == Some(Stage::InReview)`; the ledger record stays `InProgress`.
  - `stage_age_only_after_a_move`: first Jira read `DEMO-1` `To Do` gives `stage_since == None`; a second read with `To Be Tested` gives `Some(_)`.
- [ ] **Step 2:** Fail. **Step 3:** Implement. **Step 4:** `cargo test -p overseer-daemon` passes, including the existing source-line and view tests (update ones that assert exactly two sources).
- [ ] **Step 5:** Commit `Poll open PRs and send ticket stages to the board`.

### Task 6: GUI models

**Files:**
- Create: `crates/overseer-gui/src/sprint_view.rs` (register in `lib.rs`)
- Modify: `crates/overseer-gui/src/board.rs` (`done_count`), `crates/overseer-gui/src/app.rs` (`colors::PURPLE`)

**Interfaces:**
- Consumes: Task 1 types.
- Produces (all pure, `now` passed in):
  - `pub fn sprint_tickets(view: &WorkView) -> Vec<&WorkItem>`: tickets with `in_list`, not placeholders (the filter `done_count` uses today).
  - `done_count` counts `stage == Some(Stage::Done)` when a stage is present, else falls back to today's status check (old daemon). Its existing tests keep passing.
  - `pub enum Prio { Breaker, High, Med, Low }` and `pub fn prio_of(priority: Option<&str>) -> Prio` per Global Constraints.
  - `pub fn age_text(secs: i64) -> String`: under an hour `Nm`, under a day `Nh`, else `Nd` (floor; negative counts as 0).
  - `pub enum AgeTone { Fresh, Ageing, Old }` and `pub fn pr_age_tone(secs: i64) -> AgeTone` (2 and 5 day thresholds).
  - `pub struct CardModel { pub key: String, pub title: String, pub prio: Prio, pub age: Option<String>, pub slot: Option<SlotId>, pub pr: Option<(String, AgeTone)>, pub beads: usize, pub raw_status: Option<String>, pub folder: Option<PathBuf> }` (name it `TicketCard` if `CardModel` clashes with board.rs). `pr` text is `#<id>` of the oldest linked PR with its tone. `beads` counts beads whose `parent` is the key. `raw_status` is the Jira status when it is In progress only because no list named it; pass `Stages`-free: set it when `stage == InProgress` and the status, lowercased, is not `in progress`.
  - `pub struct Column { pub stage: Stage, pub total: usize, pub cards: Vec<TicketCard>, pub more: usize }` and `pub fn columns(view: &WorkView, now: i64) -> Vec<Column>`: six columns in `Stage::ALL` order, cards sorted by `Prio` then by `stage_since` oldest first (`None` last), cut to 4 with `more` the rest. The Done column carries no cards; it is drawn as its count plus `last_done(view)`.
  - `pub fn last_done(view: &WorkView) -> Option<String>`: the Done ticket with the newest `stage_since`, else the first Done ticket.
  - `pub struct Progress { pub done: usize, pub total: usize, pub per_stage: [usize; 6], pub expected: Option<usize>, pub breakers_open: usize }` and `pub fn progress(view: &WorkView) -> Option<Progress>`: `None` when there are no sprint tickets; `expected = Some(total * day / days)` (integer floor) only with a `sprint_day`. `pub fn behind_text(p: &Progress) -> Option<String>` gives `Behind pace: {expected} expected done by today` when `done < expected`.
  - `pub struct PrRow { pub repo: String, pub id: u64, pub ticket: Option<String>, pub title: String, pub age: Option<String>, pub tone: AgeTone, pub url: Option<String> }` and `pub fn pr_rows(view: &WorkView, now: i64) -> (Vec<PrRow>, usize)`: oldest first (`created` `None` last, tone `Fresh`), at most 8, plus the count left over. `pub fn pr_header(view: &WorkView, now: i64) -> String` gives `OPEN PRS · {n}` and, with any ages, `OPEN PRS · {n} · oldest {age}`.

- [ ] **Step 1: Write the failing tests** in `sprint_view.rs`, using a small `ticket(key, stage, prio)` builder:
  - `age_text_and_tones`: `age_text(59)` is `0m`, `age_text(3600*5)` is `5h`, `age_text(86_400*3)` is `3d`; `pr_age_tone(86_400*2 - 1)` is `Fresh`, `86_400*2` is `Ageing`, `86_400*5` is `Old`.
  - `prio_mapping`: `Sprint Breaker` Breaker, `Highest` High, `High` High, `Medium` Med, `Lowest` Low, `None` Med.
  - `columns_sort_and_overflow`: six In progress tickets (one Breaker, one High with `stage_since` 100, one High with 50) give a column with total 6, 4 cards, `more == 2`, first card the Breaker, then the High at 50.
  - `unmapped_status_shows_raw`: a ticket with stage In progress and status `Ready for Release` has `raw_status == Some("Ready for Release")`; one with status `In Progress` has `None`.
  - `progress_and_pace`: 10 tickets, 3 Done, `sprint_day` day 6 of 10 gives `expected == Some(6)` and `behind_text` `Some("Behind pace: 6 expected done by today")`; with `sprint_day: None`, `expected == None` and `behind_text` `None`; with no tickets, `progress` is `None`.
  - `pr_rows_oldest_first_capped`: ten PRs give 8 rows and 2 left, oldest first, a PR with no `created` last; `pr_header` reads `OPEN PRS · 10 · oldest 6d` for a six-day-old oldest.
  - `done_count_uses_stage_when_present`: a ticket with status `Shipped` and stage Done counts as done.
- [ ] **Step 2:** Fail. **Step 3:** Implement. **Step 4:** `cargo test -p overseer-gui` passes.
- [ ] **Step 5:** Commit `Add the sprint kanban and PR panel models`.

### Task 7: Render the kanban and PR panel

**Files:**
- Modify: `crates/overseer-gui/src/sprint_view.rs` (render helpers), `crates/overseer-gui/src/board.rs` (`render_board`)

**Interfaces:**
- Consumes: Task 6 models.
- Produces: the board layout from the mockup.
  - The top row becomes a flex row: the coordinator panel (`flex_1`) and, when the `prs` source is not off, the PR panel (fixed width about 380 px) to its right, same card styling (`CARD` bg, `BORDER_COORD` border). Header `pr_header`; each row: an age dot in the tone colour (`GREEN`, `ORANGE`, `RED`), `repo #id`, ticket key in `ACCENT`, truncated title, age right-aligned in the tone colour; a tooltip on the dot reads `fresh`, `ageing` or `old`; clicking a row with a `url` runs `open <url>` as `app.rs` already does for links; `+N more` in `MUTED`. With no open PRs it says `No open PRs`. In a window too narrow for both, the PR panel wraps below the coordinator panel.
  - The kanban sits between the slot grid and the status line, shown only when `progress` is `Some`. Header row: `SPRINT`, the stacked bar (each stage's share in its colour: To do `GREY`, In progress `GREEN`, In review `ACCENT`, Testing `PURPLE`, Waiting `ORANGE`, Done `GREEN` at full strength with the others dimmed is the builder's call), a white 2 px tick at `expected / total` when `expected` is set, `{done} of {total} done`, `behind_text` in `ORANGE`, and `{n} sprint breakers open` in `RED` when non-zero.
  - Six columns, each headed by an icon character and the label with its count in the stage colour (icons: To do `○`, In progress `▶`, In review `⇄`, Testing `⚗`, Waiting `⏸`, Done `✓`; if the font lacks one, pick a plain character, builder's call). Cards: priority chip (colours from Global Constraints), key, two-line title, then a chip row: slot `S{n}`, PR `#{id}` in its tone colour, `{n} beads`, raw status in `MUTED`, and the age right-aligned. Done column: the count large and `last {key}` under it.
  - Clicking a card with a folder opens it as the picker does; without a folder, nothing.
  - The board must still fit a 1440x900 window with the kanban visible: cap card height to the two-line title plus one chip row.
- [ ] **Step 1:** `cargo build -p overseer-gui` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
- [ ] **Step 2:** Run the app against a temp daemon with fake `acli` and `bb` scripts (invented tickets and PRs) and screenshot it; check against the mockup: kanban under the cards, PR panel top right, colours and icons present, nothing cut off at 1440x900.
- [ ] **Step 3:** Commit `Draw the sprint kanban and open PRs on the board`.

### Task 8: Docs

**Files:**
- Modify: the config reference and acceptance list the repo keeps for work-item settings (find where `jira_command` and `sprint_days` are documented under `docs/` and README) to add the new keys and their defaults, and a short "PRs from Bitbucket" note saying `bb` runs in each `pr_repos` folder.
- [ ] **Step 1:** Edit; a search of `docs` and `README.md` for the em dash character finds nothing new.
- [ ] **Step 2:** Commit `Document the PR and sprint stage settings`.
